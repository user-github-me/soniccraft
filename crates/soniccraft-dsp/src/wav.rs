//! WAV I/O: a tolerant reader for every PCM variant and atomic writers (hound).
//!
//! # Reading
//!
//! The reader walks the RIFF chunks itself instead of trusting a 44-byte header, because real-world TTS
//! output is messy (DESIGN.md §5.2):
//!
//! * unknown chunks (`FLLR` — macOS `say` puts audio at byte 4096 —, `LIST`, `JUNK`, `bext`, ...) are
//!   skipped, honouring the pad byte after odd-sized chunks;
//! * a `data` size of `0` or `0xFFFFFFFF` (streaming writers such as ffmpeg on a pipe), or one larger
//!   than the file, means "the rest of the file", truncated to whole frames;
//! * the RIFF size field is ignored; `RF64`/`BW64` files are accepted (their `ds64` size is used);
//! * format tags 1 (integer PCM: 8-bit unsigned, 16/24/32-bit signed), 3 (IEEE float: 32/64-bit) and
//!   `0xFFFE` (WAVE_FORMAT_EXTENSIBLE with a PCM or float sub-format) are supported.
//!
//! Integer samples are scaled by `2^(bits-1)` (so 16-bit → f32 → 16-bit is lossless). Non-finite
//! float samples (NaN, ±∞) are replaced with silence; [`read_repaired`] and [`parse_repaired`] also say
//! how many were replaced.
//!
//! # Writing
//!
//! [`write_f32`] (caches, stems) and [`write()`] (final output: 16-bit with optional TPDF dither, 24-bit,
//! 32-bit float) write to a freshly created temp file in the destination directory and rename it into
//! place (see `fsutil`: the temp file is created exclusively at an unpredictable name, never through a
//! symlink).

use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};

use crate::fsutil;
use crate::quantize::Quantizer;
use crate::{AudioBuffer, DspError};

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Tail of the KSDATAFORMAT_SUBTYPE_* GUIDs (`xxxxxxxx-0000-0010-8000-00aa00389b71`), after the 2-byte
/// format tag and two zero bytes.
const SUBFORMAT_GUID_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];
/// Probing reads at most this many leading bytes of a file (enough for any sane header).
const PROBE_PREFIX: usize = 1 << 20;

/// How samples are stored in a WAV file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WavSampleFormat {
    /// Integer PCM (8-bit unsigned, or signed 16/24/32-bit).
    Int,
    /// IEEE float (32 or 64-bit).
    Float,
}

/// Header information of a WAV file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WavInfo {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: usize,
    /// Number of complete frames in the data chunk.
    pub frames: u64,
    /// Integer or float samples.
    pub sample_format: WavSampleFormat,
    /// Container size of one sample in bits (8, 16, 24, 32 or 64).
    pub bits_per_sample: u16,
    /// Byte offset of the first sample.
    pub data_offset: u64,
    /// Bytes of sample data used (a whole number of frames).
    pub data_len: u64,
    /// Speaker positions of a WAVE_FORMAT_EXTENSIBLE file (`dwChannelMask`; `None` for other files and
    /// for a zero mask).
    pub channel_mask: Option<u32>,
}

/// Sample encodings [`write()`] supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WavEncoding {
    /// 16-bit integer PCM; `dither` adds fixed-seed TPDF dither before rounding.
    Int16 {
        /// Apply TPDF dither (DESIGN.md §4.8).
        dither: bool,
    },
    /// 24-bit integer PCM (rounded, never dithered).
    Int24,
    /// 32-bit IEEE float (lossless for f32 buffers).
    Float32,
}

impl WavEncoding {
    /// Bits per sample of the encoding.
    pub fn bits_per_sample(self) -> u16 {
        match self {
            WavEncoding::Int16 { .. } => 16,
            WavEncoding::Int24 => 24,
            WavEncoding::Float32 => 32,
        }
    }
}

/// Write a 32-bit float WAV file (atomic: temp file + rename). Missing parent directories are created.
///
/// Non-finite samples are written as 0. Errors if the buffer has no channels, ragged channels or a zero
/// sample rate.
pub fn write_f32(path: &Path, buffer: &AudioBuffer) -> Result<(), DspError> {
    write(path, buffer, WavEncoding::Float32).map(|_| ())
}

/// Write a WAV file with the given encoding (atomic: temp file + rename). Returns the file size in
/// bytes. Missing parent directories are created.
///
/// Integer encodings clamp samples to `[-1, 1)` before quantization (DESIGN.md §4.8).
pub fn write(path: &Path, buffer: &AudioBuffer, encoding: WavEncoding) -> Result<u64, DspError> {
    let spec = spec_for(buffer, encoding)?;
    let what = || path.display().to_string();
    fsutil::write_atomic(path, |file| {
        let mut writer = WavWriter::new(file, spec).map_err(|e| hound_error(path, e))?;
        write_samples(&mut writer, buffer, encoding)
            .map_err(|e| DspError::encode(what(), e.to_string()))?;
        writer.finalize().map_err(|e| hound_error(path, e))
    })
}

/// Encode a WAV file into memory (same layout as [`write()`]).
pub fn encode_to_vec(buffer: &AudioBuffer, encoding: WavEncoding) -> Result<Vec<u8>, DspError> {
    let spec = spec_for(buffer, encoding)?;
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = WavWriter::new(&mut cursor, spec)
            .map_err(|e| DspError::encode("in-memory WAV", e.to_string()))?;
        write_samples(&mut writer, buffer, encoding)
            .map_err(|e| DspError::encode("in-memory WAV", e.to_string()))?;
        writer
            .finalize()
            .map_err(|e| DspError::encode("in-memory WAV", e.to_string()))?;
    }
    Ok(cursor.into_inner())
}

fn spec_for(buffer: &AudioBuffer, encoding: WavEncoding) -> Result<WavSpec, DspError> {
    validate_buffer(buffer)?;
    let channels = u16::try_from(buffer.num_channels()).map_err(|_| {
        DspError::InvalidInput(format!(
            "{} channels is too many for WAV",
            buffer.num_channels()
        ))
    })?;
    Ok(WavSpec {
        channels,
        sample_rate: buffer.sample_rate,
        bits_per_sample: encoding.bits_per_sample(),
        sample_format: match encoding {
            WavEncoding::Float32 => SampleFormat::Float,
            _ => SampleFormat::Int,
        },
    })
}

/// Check the invariants every writer relies on.
pub(crate) fn validate_buffer(buffer: &AudioBuffer) -> Result<(), DspError> {
    if buffer.num_channels() == 0 {
        return Err(DspError::InvalidInput("buffer has no channels".into()));
    }
    if buffer.sample_rate == 0 {
        return Err(DspError::InvalidInput("sample rate is 0".into()));
    }
    let frames = buffer.frames();
    if buffer.channels.iter().any(|c| c.len() != frames) {
        return Err(DspError::InvalidInput(
            "channels have different lengths".into(),
        ));
    }
    Ok(())
}

fn write_samples<W: std::io::Write + std::io::Seek>(
    writer: &mut WavWriter<W>,
    buffer: &AudioBuffer,
    encoding: WavEncoding,
) -> Result<(), hound::Error> {
    let frames = buffer.frames();
    match encoding {
        WavEncoding::Float32 => {
            for i in 0..frames {
                for ch in &buffer.channels {
                    let s = ch[i];
                    writer.write_sample(if s.is_finite() { s } else { 0.0 })?;
                }
            }
        }
        WavEncoding::Int16 { dither } => {
            // hound's fast 16-bit writer buffers `num_samples` samples; feed it bounded chunks.
            const CHUNK_FRAMES: usize = 16_384;
            let mut q = Quantizer::new(16, dither);
            let mut start = 0;
            while start < frames {
                let end = (start + CHUNK_FRAMES).min(frames);
                let mut w16 = writer.get_i16_writer(((end - start) * buffer.num_channels()) as u32);
                for i in start..end {
                    for ch in &buffer.channels {
                        // The quantizer clamps to the i16 range.
                        w16.write_sample(q.quantize(ch[i]) as i16);
                    }
                }
                w16.flush()?;
                start = end;
            }
        }
        WavEncoding::Int24 => {
            let mut q = Quantizer::new(24, false);
            for i in 0..frames {
                for ch in &buffer.channels {
                    writer.write_sample(q.quantize(ch[i]))?;
                }
            }
        }
    }
    Ok(())
}

fn hound_error(path: &Path, e: hound::Error) -> DspError {
    match e {
        hound::Error::IoError(source) => DspError::io(path, source),
        other => DspError::encode(path.display().to_string(), other.to_string()),
    }
}

/// Read any PCM WAV file (8/16/24/32-bit int, 32/64-bit float, WAVE_FORMAT_EXTENSIBLE), tolerating
/// unknown chunks and streaming-style size fields (see the module docs). Non-finite samples come back
/// as 0.
pub fn read(path: &Path) -> Result<AudioBuffer, DspError> {
    read_repaired(path).map(|(buffer, _)| buffer)
}

/// [`read`], also returning how many non-finite samples (NaN, ±∞) were replaced with 0 (for an
/// `audio_repaired` diagnostic).
pub fn read_repaired(path: &Path) -> Result<(AudioBuffer, u64), DspError> {
    let bytes = std::fs::read(path).map_err(|e| DspError::io(path, e))?;
    parse_repaired(&bytes).map_err(|e| relabel(e, path))
}

/// Parse an in-memory WAV file (see [`read`]).
pub fn parse(bytes: &[u8]) -> Result<AudioBuffer, DspError> {
    parse_repaired(bytes).map(|(buffer, _)| buffer)
}

/// [`parse`], also returning how many non-finite samples were replaced with 0.
pub fn parse_repaired(bytes: &[u8]) -> Result<(AudioBuffer, u64), DspError> {
    parse_detailed(bytes)
        .map(|p| (p.buffer, p.repaired))
        .map_err(|e| DspError::decode("WAV data", e.message()))
}

/// A parsed WAV file.
pub(crate) struct ParsedWav {
    /// The samples (non-finite values replaced with 0).
    pub buffer: AudioBuffer,
    /// The header.
    pub info: WavInfo,
    /// Non-finite samples replaced with 0.
    pub repaired: u64,
}

/// [`parse`] with the header and the precise reason on failure.
pub(crate) fn parse_detailed(bytes: &[u8]) -> Result<ParsedWav, HeaderError> {
    let info = parse_header(bytes, bytes.len() as u64)?;
    let (buffer, repaired) = decode_samples(bytes, &info);
    Ok(ParsedWav {
        buffer,
        info,
        repaired,
    })
}

/// Header information of a WAV file without decoding its samples (reads at most the first MiB unless
/// the header is unusually laid out).
pub fn probe(path: &Path) -> Result<WavInfo, DspError> {
    use std::io::Read;
    let what = || path.display().to_string();
    let file = std::fs::File::open(path).map_err(|e| DspError::io(path, e))?;
    let total = file.metadata().map_err(|e| DspError::io(path, e))?.len();
    let mut prefix = Vec::with_capacity(PROBE_PREFIX.min(total as usize));
    file.take(PROBE_PREFIX as u64)
        .read_to_end(&mut prefix)
        .map_err(|e| DspError::io(path, e))?;
    match parse_header(&prefix, total) {
        Ok(info) => Ok(info),
        Err(HeaderError::NeedMore) => {
            let all = std::fs::read(path).map_err(|e| DspError::io(path, e))?;
            parse_header(&all, all.len() as u64).map_err(|e| DspError::decode(what(), e.message()))
        }
        Err(e) => Err(DspError::decode(what(), e.message())),
    }
}

/// True when `bytes` start like a RIFF/RF64/BW64 WAVE file.
pub fn is_wav(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && matches!(&bytes[0..4], b"RIFF" | b"RF64" | b"BW64")
        && &bytes[8..12] == b"WAVE"
}

fn relabel(e: DspError, path: &Path) -> DspError {
    match e {
        DspError::Decode { message, .. } => DspError::decode(path.display().to_string(), message),
        other => other,
    }
}

/// Why a header could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderError {
    /// The header extends beyond the bytes provided (only possible when probing a prefix).
    NeedMore,
    /// The file is not a WAV file, or one we cannot decode (the message says why).
    Invalid(String),
    /// A valid WAV file whose sample encoding is not plain PCM/float (e.g. ADPCM, µ-law): another
    /// decoder may handle it.
    UnsupportedEncoding(String),
}

impl HeaderError {
    /// Human-readable description.
    pub(crate) fn message(&self) -> String {
        match self {
            HeaderError::NeedMore => "file is truncated inside the WAV header".into(),
            HeaderError::Invalid(m) | HeaderError::UnsupportedEncoding(m) => m.clone(),
        }
    }
}

struct Fmt {
    sample_format: WavSampleFormat,
    channels: u16,
    sample_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
    channel_mask: Option<u32>,
}

fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn parse_fmt(body: &[u8]) -> Result<Fmt, HeaderError> {
    let short =
        || HeaderError::Invalid(format!("`fmt ` chunk is too short ({} bytes)", body.len()));
    let tag = le_u16(body, 0).ok_or_else(short)?;
    let channels = le_u16(body, 2).ok_or_else(short)?;
    let sample_rate = le_u32(body, 4).ok_or_else(short)?;
    let block_align = le_u16(body, 12).ok_or_else(short)?;
    let bits_per_sample = le_u16(body, 14).ok_or_else(short)?;
    // cbSize(2) validBits(2) channelMask(4) subFormat GUID(16) after the 16-byte basic fmt.
    let channel_mask = (tag == WAVE_FORMAT_EXTENSIBLE)
        .then(|| le_u32(body, 20))
        .flatten()
        .filter(|&m| m != 0);
    let effective_tag = if tag == WAVE_FORMAT_EXTENSIBLE {
        let guid = body.get(24..40).ok_or_else(|| {
            HeaderError::Invalid(
                "WAVE_FORMAT_EXTENSIBLE `fmt ` chunk has no sub-format GUID".into(),
            )
        })?;
        if guid[2..4] != [0, 0] || guid[4..16] != SUBFORMAT_GUID_TAIL {
            return Err(HeaderError::UnsupportedEncoding(
                "unknown WAVE_FORMAT_EXTENSIBLE sub-format".into(),
            ));
        }
        u16::from_le_bytes([guid[0], guid[1]])
    } else {
        tag
    };
    let sample_format = match effective_tag {
        WAVE_FORMAT_PCM => WavSampleFormat::Int,
        WAVE_FORMAT_IEEE_FLOAT => WavSampleFormat::Float,
        other => {
            return Err(HeaderError::UnsupportedEncoding(format!(
                "WAV format tag 0x{other:04x} is not PCM or IEEE float"
            )));
        }
    };
    if channels == 0 {
        return Err(HeaderError::Invalid(
            "WAV header declares 0 channels".into(),
        ));
    }
    if sample_rate == 0 {
        return Err(HeaderError::Invalid(
            "WAV header declares a sample rate of 0".into(),
        ));
    }
    Ok(Fmt {
        sample_format,
        channels,
        sample_rate,
        block_align,
        bits_per_sample,
        channel_mask,
    })
}

/// Walk the RIFF chunks of `bytes` (a prefix of a file that is `total_len` bytes long).
pub(crate) fn parse_header(bytes: &[u8], total_len: u64) -> Result<WavInfo, HeaderError> {
    if bytes.len() < 12 {
        return Err(if total_len >= 12 && bytes.len() < total_len as usize {
            HeaderError::NeedMore
        } else {
            HeaderError::Invalid("too short to be a WAV file".into())
        });
    }
    if &bytes[0..4] == b"RIFX" {
        return Err(HeaderError::UnsupportedEncoding(
            "big-endian RIFX WAV files are not supported".into(),
        ));
    }
    if !is_wav(bytes) {
        return Err(HeaderError::Invalid("not a RIFF/WAVE file".into()));
    }
    let mut pos: u64 = 12;
    let mut fmt: Option<Fmt> = None;
    let mut data: Option<(u64, u64)> = None;
    let mut ds64_data_size: Option<u64> = None;
    while pos + 8 <= total_len {
        let at = pos as usize;
        let (Some(id), Some(size)) = (bytes.get(at..at + 4), le_u32(bytes, at + 4)) else {
            return Err(HeaderError::NeedMore);
        };
        let body_start = pos + 8;
        let remaining = total_len - body_start;
        match id {
            b"fmt " => {
                let start = body_start as usize;
                let end = start + (size as usize).min(64);
                let body = match bytes.get(start..end) {
                    Some(b) => b,
                    None if (bytes.len() as u64) < total_len => return Err(HeaderError::NeedMore),
                    None => bytes.get(start..).unwrap_or_default(),
                };
                fmt = Some(parse_fmt(body)?);
            }
            b"ds64" => {
                let start = body_start as usize;
                ds64_data_size = le_u64(bytes, start + 8);
                if ds64_data_size.is_none() && (bytes.len() as u64) < total_len {
                    return Err(HeaderError::NeedMore);
                }
            }
            b"data" => {
                let declared = match (size, ds64_data_size) {
                    (u32::MAX, Some(big)) => big,
                    (s, _) => u64::from(s),
                };
                let unknown = size == 0 || size == u32::MAX || declared > remaining;
                let len = if unknown { remaining } else { declared };
                data = Some((body_start, len));
                if fmt.is_some() || len == remaining {
                    break;
                }
            }
            _ => {}
        }
        // Chunks are padded to an even size.
        pos = body_start + u64::from(size) + u64::from(size & 1);
    }
    let fmt = fmt.ok_or_else(|| {
        if (bytes.len() as u64) < total_len {
            HeaderError::NeedMore
        } else {
            HeaderError::Invalid("WAV file has no `fmt ` chunk".into())
        }
    })?;
    let (data_offset, raw_len) =
        data.ok_or_else(|| HeaderError::Invalid("WAV file has no `data` chunk".into()))?;

    let channels = usize::from(fmt.channels);
    let min_bytes = usize::from(fmt.bits_per_sample).div_ceil(8);
    let align = usize::from(fmt.block_align);
    // The container size per sample comes from block_align when it is consistent; some writers put
    // garbage there, in which case the declared bit depth decides.
    let container =
        if align >= channels && align % channels == 0 && align / channels >= min_bytes.max(1) {
            align / channels
        } else {
            min_bytes
        };
    let supported = match fmt.sample_format {
        WavSampleFormat::Int => matches!(container, 1..=4),
        WavSampleFormat::Float => matches!(container, 4 | 8),
    };
    if !supported || min_bytes == 0 {
        return Err(HeaderError::UnsupportedEncoding(format!(
            "{}-bit {} samples in {}-byte containers are not supported",
            fmt.bits_per_sample,
            match fmt.sample_format {
                WavSampleFormat::Int => "integer",
                WavSampleFormat::Float => "float",
            },
            container
        )));
    }
    let frame_bytes = (container * channels) as u64;
    let frames = raw_len / frame_bytes;
    Ok(WavInfo {
        sample_rate: fmt.sample_rate,
        channels,
        frames,
        sample_format: fmt.sample_format,
        bits_per_sample: (container * 8) as u16,
        data_offset,
        data_len: frames * frame_bytes,
        channel_mask: fmt.channel_mask,
    })
}

/// Convert the sample data described by `info` to planar f32, replacing non-finite float samples with
/// 0. Returns the buffer and the number of replaced samples.
fn decode_samples(bytes: &[u8], info: &WavInfo) -> (AudioBuffer, u64) {
    let channels = info.channels;
    let frames = info.frames as usize;
    let start = info.data_offset as usize;
    let data = &bytes[start..start + info.data_len as usize];
    let width = usize::from(info.bits_per_sample / 8);
    let mut planes: Vec<Vec<f32>> = (0..channels).map(|_| Vec::with_capacity(frames)).collect();
    let convert: fn(&[u8]) -> f32 = match (info.sample_format, width) {
        (WavSampleFormat::Int, 1) => |b| (f32::from(b[0]) - 128.0) / 128.0,
        (WavSampleFormat::Int, 2) => |b| f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
        (WavSampleFormat::Int, 3) => {
            |b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
        }
        (WavSampleFormat::Int, _) => {
            |b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0
        }
        (WavSampleFormat::Float, 4) => |b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        (WavSampleFormat::Float, _) => {
            |b| f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32
        }
    };
    for frame in data.chunks_exact(width * channels) {
        for (plane, sample) in planes.iter_mut().zip(frame.chunks_exact(width)) {
            plane.push(convert(sample));
        }
    }
    let mut buffer = AudioBuffer::new(info.sample_rate, planes);
    // Integer PCM is always finite; float data may hold NaN/∞ (a corrupt or tampered file).
    let repaired = match info.sample_format {
        WavSampleFormat::Float => buffer.replace_non_finite(),
        WavSampleFormat::Int => 0,
    };
    (buffer, repaired)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a WAV file by hand: `fmt` body + extra chunks before `data`.
    fn wav_bytes(
        fmt: &[u8],
        before_data: &[(&[u8; 4], Vec<u8>)],
        data_size: u32,
        data: &[u8],
        magic: &[u8; 4],
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(magic);
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
        v.extend_from_slice(fmt);
        for (id, body) in before_data {
            v.extend_from_slice(*id);
            v.extend_from_slice(&(body.len() as u32).to_le_bytes());
            v.extend_from_slice(body);
            if body.len() % 2 == 1 {
                v.push(0);
            }
        }
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_size.to_le_bytes());
        v.extend_from_slice(data);
        let riff = (v.len() - 8) as u32;
        v[4..8].copy_from_slice(&riff.to_le_bytes());
        v
    }

    fn fmt_body(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let block = channels * bits.div_ceil(8);
        let mut f = Vec::new();
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&channels.to_le_bytes());
        f.extend_from_slice(&rate.to_le_bytes());
        f.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
        f.extend_from_slice(&block.to_le_bytes());
        f.extend_from_slice(&bits.to_le_bytes());
        f
    }

    fn extensible_fmt(sub: u16, channels: u16, rate: u32, bits: u16, valid: u16) -> Vec<u8> {
        let mut f = fmt_body(WAVE_FORMAT_EXTENSIBLE, channels, rate, bits);
        f.extend_from_slice(&22u16.to_le_bytes());
        f.extend_from_slice(&valid.to_le_bytes());
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(&sub.to_le_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(&SUBFORMAT_GUID_TAIL);
        f
    }

    fn s16(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn reads_every_pcm_variant() {
        // 8-bit unsigned
        let b = wav_bytes(&fmt_body(1, 1, 8000, 8), &[], 3, &[0, 128, 255], b"RIFF");
        assert_eq!(
            parse(&b).unwrap().channels,
            vec![vec![-1.0, 0.0, 127.0 / 128.0]]
        );
        // 16-bit stereo
        let b = wav_bytes(
            &fmt_body(1, 2, 8000, 16),
            &[],
            8,
            &s16(&[16384, -16384, 32767, -32768]),
            b"RIFF",
        );
        let buf = parse(&b).unwrap();
        assert_eq!(
            buf.channels,
            vec![vec![0.5, 32767.0 / 32768.0], vec![-0.5, -1.0]]
        );
        // 24-bit
        let data: Vec<u8> = [0x40_0000i32, -0x40_0000, -0x80_0000]
            .iter()
            .flat_map(|v| v.to_le_bytes()[..3].to_vec())
            .collect();
        let b = wav_bytes(&fmt_body(1, 1, 8000, 24), &[], 9, &data, b"RIFF");
        assert_eq!(parse(&b).unwrap().channels, vec![vec![0.5, -0.5, -1.0]]);
        // 32-bit int
        let data: Vec<u8> = [i32::MIN, 1 << 30]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let b = wav_bytes(&fmt_body(1, 1, 8000, 32), &[], 8, &data, b"RIFF");
        assert_eq!(parse(&b).unwrap().channels, vec![vec![-1.0, 0.5]]);
        // 32-bit float
        let data: Vec<u8> = [0.25f32, -1.5]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let b = wav_bytes(&fmt_body(3, 1, 8000, 32), &[], 8, &data, b"RIFF");
        assert_eq!(parse(&b).unwrap().channels, vec![vec![0.25, -1.5]]);
        // 64-bit float
        let data: Vec<u8> = [0.125f64, -0.75]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let b = wav_bytes(&fmt_body(3, 1, 8000, 64), &[], 16, &data, b"RIFF");
        assert_eq!(parse(&b).unwrap().channels, vec![vec![0.125, -0.75]]);
        // extensible float and extensible 24-in-32 PCM
        let data: Vec<u8> = [0.5f32, 0.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let b = wav_bytes(&extensible_fmt(3, 2, 48000, 32, 32), &[], 8, &data, b"RIFF");
        assert_eq!(parse(&b).unwrap().channels, vec![vec![0.5], vec![0.25]]);
        let data: Vec<u8> = [0x4000_0000i32]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let b = wav_bytes(&extensible_fmt(1, 1, 48000, 32, 24), &[], 4, &data, b"RIFF");
        assert_eq!(parse(&b).unwrap().channels, vec![vec![0.5]]);
    }

    #[test]
    fn tolerates_unknown_chunks_and_streaming_sizes() {
        let samples = s16(&[1000, -1000, 2000]);
        let expected: Vec<f32> = [1000, -1000, 2000]
            .iter()
            .map(|&v: &i16| f32::from(v) / 32768.0)
            .collect();
        let chunks: Vec<(&[u8; 4], Vec<u8>)> = vec![
            (b"FLLR", vec![0; 4044]),
            (b"LIST", b"INFOISFT\x05\0\0\0odd!!".to_vec()),
            (b"JUNK", vec![1, 2, 3]),
        ];
        for (size, magic) in [
            (6u32, b"RIFF"),
            (0, b"RIFF"),
            (u32::MAX, b"RIFF"),
            (1_000_000, b"RIFF"),
            (u32::MAX, b"RF64"),
        ] {
            let b = wav_bytes(&fmt_body(1, 1, 24000, 16), &chunks, size, &samples, magic);
            let buf = parse(&b).unwrap_or_else(|e| panic!("size {size}: {e}"));
            assert_eq!(buf.sample_rate, 24000);
            assert_eq!(buf.channels, vec![expected.clone()], "data size {size}");
        }
        // A trailing partial frame is dropped.
        let mut odd = samples.clone();
        odd.push(7);
        let b = wav_bytes(&fmt_body(1, 1, 24000, 16), &[], u32::MAX, &odd, b"RIFF");
        assert_eq!(parse(&b).unwrap().frames(), 3);
        // A chunk after a correctly sized data chunk is ignored.
        let mut b = wav_bytes(&fmt_body(1, 1, 24000, 16), &[], 6, &samples, b"RIFF");
        b.extend_from_slice(b"LIST\x04\0\0\0abcd");
        assert_eq!(parse(&b).unwrap().channels, vec![expected]);
    }

    #[test]
    fn fmt_after_data_is_found() {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        v.extend_from_slice(b"data\x04\0\0\0");
        v.extend_from_slice(&s16(&[16384, -16384]));
        let f = fmt_body(1, 1, 16000, 16);
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&(f.len() as u32).to_le_bytes());
        v.extend_from_slice(&f);
        assert_eq!(parse(&v).unwrap().channels, vec![vec![0.5, -0.5]]);
    }

    #[test]
    fn rejects_garbage_and_unsupported_encodings() {
        assert!(parse(b"").is_err());
        assert!(parse(b"RIFF\0\0\0\0WAVX").is_err());
        assert!(parse(b"RIFF\0\0\0\0WAVE").is_err(), "no fmt");
        let b = wav_bytes(&fmt_body(1, 0, 8000, 16), &[], 0, &[], b"RIFF");
        assert!(parse(&b).is_err(), "zero channels");
        let b = wav_bytes(&fmt_body(1, 1, 0, 16), &[], 0, &[], b"RIFF");
        assert!(parse(&b).is_err(), "zero rate");
        let adpcm = wav_bytes(&fmt_body(2, 1, 8000, 4), &[], 4, &[0; 4], b"RIFF");
        assert!(matches!(
            parse_header(&adpcm, adpcm.len() as u64),
            Err(HeaderError::UnsupportedEncoding(_))
        ));
        let truncated_fmt = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0";
        assert!(parse(truncated_fmt).is_err());
    }

    #[test]
    fn malformed_headers_never_panic() {
        let chunks: Vec<(&[u8; 4], Vec<u8>)> =
            vec![(b"LIST", vec![7; 9]), (b"ds64", vec![0xFF; 28])];
        let good = wav_bytes(
            &extensible_fmt(1, 2, 44_100, 24, 24),
            &chunks,
            12,
            &[1; 12],
            b"RF64",
        );
        for cut in 0..good.len() {
            let _ = parse(&good[..cut]);
            let _ = parse_header(&good[..cut], good.len() as u64);
        }
        let mut state = 0x1234_5678u32;
        for _ in 0..2000 {
            let mut bad = good.clone();
            for _ in 0..4 {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let at = 12 + (state as usize) % (bad.len() - 12);
                bad[at] = (state >> 8) as u8;
            }
            let _ = parse(&bad);
        }
    }

    #[test]
    fn empty_data_chunk() {
        let b = wav_bytes(&fmt_body(1, 2, 8000, 16), &[], 0, &[], b"RIFF");
        let buf = parse(&b).unwrap();
        assert_eq!(buf.num_channels(), 2);
        assert_eq!(buf.frames(), 0);
    }

    #[test]
    fn write_read_roundtrip_all_encodings() {
        let dir = tempfile::tempdir().unwrap();
        let n = 1000;
        let left: Vec<f32> = (0..n).map(|i| ((i as f32) * 0.05).sin() * 0.8).collect();
        let right: Vec<f32> = left.iter().map(|s| -s * 0.5).collect();
        let buf = AudioBuffer::new(44_100, vec![left, right]);

        let p = dir.path().join("f32.wav");
        write_f32(&p, &buf).unwrap();
        assert_eq!(read(&p).unwrap(), buf, "float is lossless");

        let p = dir.path().join("s24.wav");
        write(&p, &buf, WavEncoding::Int24).unwrap();
        let back = read(&p).unwrap();
        let err = max_err(&buf, &back);
        assert!(err <= 0.5 / 8_388_608.0 + 1e-9, "24-bit error {err}");

        let p = dir.path().join("s16.wav");
        let bytes = write(&p, &buf, WavEncoding::Int16 { dither: false }).unwrap();
        assert_eq!(bytes, std::fs::metadata(&p).unwrap().len());
        let back = read(&p).unwrap();
        assert!(max_err(&buf, &back) <= 0.5 / 32768.0 + 1e-9);

        let p2 = dir.path().join("s16d.wav");
        write(&p2, &buf, WavEncoding::Int16 { dither: true }).unwrap();
        let dithered = read(&p2).unwrap();
        assert!(max_err(&buf, &dithered) <= 1.5 / 32768.0 + 1e-9);
        let p3 = dir.path().join("s16d2.wav");
        write(&p3, &buf, WavEncoding::Int16 { dither: true }).unwrap();
        assert_eq!(
            std::fs::read(&p2).unwrap(),
            std::fs::read(&p3).unwrap(),
            "deterministic dither"
        );

        // in-memory encoding matches the file
        assert_eq!(
            encode_to_vec(&buf, WavEncoding::Int24).unwrap(),
            std::fs::read(dir.path().join("s24.wav")).unwrap()
        );
        // probe agrees
        let info = probe(&p).unwrap();
        assert_eq!(
            (
                info.frames,
                info.sample_rate,
                info.channels,
                info.bits_per_sample
            ),
            (n as u64, 44_100, 2, 16)
        );
    }

    #[test]
    fn writer_rejects_invalid_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.wav");
        assert!(
            write_f32(
                &p,
                &AudioBuffer {
                    sample_rate: 48_000,
                    channels: vec![]
                }
            )
            .is_err()
        );
        assert!(
            write_f32(
                &p,
                &AudioBuffer {
                    sample_rate: 0,
                    channels: vec![vec![0.0]]
                }
            )
            .is_err()
        );
        assert!(
            write_f32(
                &p,
                &AudioBuffer {
                    sample_rate: 8000,
                    channels: vec![vec![0.0], vec![]]
                }
            )
            .is_err()
        );
        assert!(!p.exists());
        // empty but valid
        write_f32(&p, &AudioBuffer::silent(8000, 1, 0)).unwrap();
        assert_eq!(read(&p).unwrap().frames(), 0);
    }

    fn max_err(a: &AudioBuffer, b: &AudioBuffer) -> f32 {
        assert_eq!(a.num_channels(), b.num_channels());
        assert_eq!(a.frames(), b.frames());
        a.channels
            .iter()
            .zip(&b.channels)
            .flat_map(|(x, y)| x.iter().zip(y).map(|(p, q)| (p - q).abs()))
            .fold(0.0, f32::max)
    }
}
