//! Encoding the master (DESIGN.md §4.8).
//!
//! | format | encoder | notes |
//! |---|---|---|
//! | wav | hound | 16-bit (TPDF dither when `dither`), 24-bit (rounded), 32-bit float |
//! | mp3 | LAME via mp3lame-encoder (cargo feature `mp3`, default on); ffmpeg/libmp3lame without it | CBR, mono/joint-stereo, LAME/Info tag for gapless playback |
//! | flac | flacenc | 16-bit (dither when `dither`) or 24-bit |
//! | ogg | ffmpeg libvorbis (native `vorbis` as a fallback) | |
//! | opus | ffmpeg libopus (native `opus` as a fallback) | always 48 kHz (ffmpeg resamples) |
//! | m4a | ffmpeg native AAC | `+faststart` |
//!
//! Integer PCM clamps samples to `[-1, 1)` before quantization; 16-bit uses fixed-seed TPDF dither when
//! requested, so encodes are reproducible. wav, flac and mp3 output is byte-identical for identical
//! input. ffmpeg-based encoders read a 32-bit float WAV from stdin and run with
//! `-map_metadata -1 -fflags +bitexact -flags:a +bitexact` so no version strings or metadata leak into
//! the file. Every output is written to a temp file next to the destination and renamed into place.
//!
//! # MP3 gapless playback
//!
//! LAME delays the audio by 576 samples and pads the end to a whole frame; its decoder adds another
//! 529. The encoder writes LAME's Info/LAME tag frame (overwriting the zero-filled placeholder LAME
//! emits at the start of the stream) with the exact delay and padding, so tag-aware decoders (ffmpeg,
//! symphonia, most players) reproduce the input sample-exactly, with zero offset and the original
//! length. Decoders that ignore the tag see 1105 samples of extra leading silence. The tag has to fit
//! in one CBR frame, so bitrates below [`mp3_min_gapless_bitrate`] (64 kbps at 44.1/48 kHz, less at
//! lower rates) are raised to it; [`EncodeSummary::bitrate_bps`] reports the bitrate used.

use std::io::Write;
use std::path::Path;

use crate::ffmpeg::{self, Ffmpeg};
use crate::fsutil;
use crate::quantize::Quantizer;
use crate::wav::{self, WavEncoding};
use crate::{AudioBuffer, DspError};

/// Output format and its parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncodeFormat {
    /// WAV: `bit_depth` 16 or 24 (integer) or 32 (float).
    Wav {
        /// 16, 24 or 32.
        bit_depth: u16,
    },
    /// MP3 CBR at `bitrate_bps` (snapped to the nearest bitrate valid for the sample rate).
    Mp3 {
        /// Bits per second.
        bitrate_bps: u32,
    },
    /// FLAC: `bit_depth` 16 or 24.
    Flac {
        /// 16 or 24.
        bit_depth: u16,
    },
    /// Ogg Vorbis (ffmpeg).
    Ogg {
        /// Bits per second (nominal).
        bitrate_bps: u32,
    },
    /// Ogg Opus at 48 kHz (ffmpeg).
    Opus {
        /// Bits per second.
        bitrate_bps: u32,
    },
    /// M4A / AAC-LC (ffmpeg).
    M4a {
        /// Bits per second.
        bitrate_bps: u32,
    },
}

impl EncodeFormat {
    /// Lower-case format name (`wav`, `mp3`, ...).
    pub fn name(&self) -> &'static str {
        match self {
            EncodeFormat::Wav { .. } => "wav",
            EncodeFormat::Mp3 { .. } => "mp3",
            EncodeFormat::Flac { .. } => "flac",
            EncodeFormat::Ogg { .. } => "ogg",
            EncodeFormat::Opus { .. } => "opus",
            EncodeFormat::M4a { .. } => "m4a",
        }
    }

    /// Whether encoding this format needs ffmpeg in this build (ogg, opus, m4a; mp3 without the
    /// `mp3` feature).
    pub fn requires_ffmpeg(&self) -> bool {
        match self {
            EncodeFormat::Wav { .. } | EncodeFormat::Flac { .. } => false,
            EncodeFormat::Mp3 { .. } => !cfg!(feature = "mp3"),
            EncodeFormat::Ogg { .. } | EncodeFormat::Opus { .. } | EncodeFormat::M4a { .. } => true,
        }
    }
}

/// Encoder settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Format and parameters.
    pub format: EncodeFormat,
    /// TPDF dither when quantizing to 16-bit integer PCM (wav/flac).
    pub dither: bool,
    /// How to find ffmpeg for the formats that need it.
    pub ffmpeg: Ffmpeg,
}

impl EncodeOptions {
    /// Options for `format` with dither on and ffmpeg looked up on `PATH`.
    pub fn new(format: EncodeFormat) -> Self {
        EncodeOptions {
            format,
            dither: true,
            ffmpeg: Ffmpeg::Auto,
        }
    }
}

/// What was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeSummary {
    /// Size of the output file in bytes.
    pub bytes: u64,
    /// Sample rate of the encoded stream (48000 for opus).
    pub sample_rate: u32,
    /// Channel count.
    pub channels: usize,
    /// Bit depth for wav/flac.
    pub bit_depth: Option<u16>,
    /// Bitrate actually used for lossy formats, in bits per second.
    pub bitrate_bps: Option<u32>,
    /// The encoder that produced the file (`hound`, `lame`, `flacenc`, `ffmpeg/libvorbis`, ...).
    pub encoder: String,
}

/// Sample rates MP3 supports (MPEG-1, MPEG-2 and MPEG-2.5 layer III).
pub const MP3_SAMPLE_RATES: [u32; 9] = [
    8000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000,
];

const MP3_KBPS_MPEG1: [u32; 14] = [
    32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
];
const MP3_KBPS_MPEG2: [u32; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
const MP3_KBPS_MPEG25: [u32; 8] = [8, 16, 24, 32, 40, 48, 56, 64];

/// Valid CBR bitrates (kbps) for an MP3 sample rate, as LAME 3.100's `bitrate_table`: MPEG-1 rates
/// (32/44.1/48 kHz) allow 32–320, MPEG-2 (16/22.05/24 kHz) 8–160 and MPEG-2.5 (8/11.025/12 kHz)
/// 8–64. Empty for unsupported sample rates.
pub fn mp3_bitrates_kbps(sample_rate: u32) -> &'static [u32] {
    match sample_rate {
        32_000 | 44_100 | 48_000 => &MP3_KBPS_MPEG1,
        16_000 | 22_050 | 24_000 => &MP3_KBPS_MPEG2,
        8000 | 11_025 | 12_000 => &MP3_KBPS_MPEG25,
        _ => &[],
    }
}

/// The valid MP3 bitrate (bits per second) nearest to `bitrate_bps` for `sample_rate` (ties go to the
/// lower bitrate); `None` for unsupported sample rates.
pub fn nearest_mp3_bitrate(sample_rate: u32, bitrate_bps: u32) -> Option<u32> {
    let wanted = f64::from(bitrate_bps) / 1000.0;
    mp3_bitrates_kbps(sample_rate)
        .iter()
        .copied()
        .min_by(|a, b| {
            let (da, db) = (
                (f64::from(*a) - wanted).abs(),
                (f64::from(*b) - wanted).abs(),
            );
            da.total_cmp(&db).then(a.cmp(b))
        })
        .map(|k| k * 1000)
}

/// Size of LAME's Xing/Info + LAME extension header (`LAMEHEADERSIZE` in LAME 3.100's VbrTag.c).
const LAME_TAG_BYTES: u32 = 156;
/// LAME refuses to write the tag into frames larger than this.
const LAME_MAX_FRAME_BYTES: u32 = 2880;

/// Whether LAME can embed its Info/LAME tag (needed for gapless decoding) in a CBR frame at
/// `kbps`: the frame (`144000·kbps/sr` bytes for MPEG-1, `72000·kbps/sr` for MPEG-2/2.5) must hold
/// the side info and the 156-byte tag (LAME 3.100 `InitVbrTag`).
fn mp3_tag_fits(sample_rate: u32, channels: usize, kbps: u32) -> bool {
    let mpeg1 = sample_rate >= 32_000;
    let side_info = match (mpeg1, channels) {
        (true, 1) => 4 + 17,
        (true, _) => 4 + 32,
        (false, 1) => 4 + 9,
        (false, _) => 4 + 17,
    };
    let frame =
        (if mpeg1 { 144_000u64 } else { 72_000 }) * u64::from(kbps) / u64::from(sample_rate.max(1));
    frame >= u64::from(side_info + LAME_TAG_BYTES) && frame <= u64::from(LAME_MAX_FRAME_BYTES)
}

/// The lowest valid MP3 bitrate (bits per second) whose CBR frames can carry the gapless Info/LAME
/// tag at `sample_rate` with `channels` (e.g. 64 kbps at 48 kHz, 40 kbps at 16 kHz mono); `None` for
/// unsupported sample rates. Lower bitrates are raised to this by the encoder.
pub fn mp3_min_gapless_bitrate(sample_rate: u32, channels: usize) -> Option<u32> {
    mp3_bitrates_kbps(sample_rate)
        .iter()
        .copied()
        .find(|&k| mp3_tag_fits(sample_rate, channels, k))
        .map(|k| k * 1000)
}

/// Encode `buffer` to `path` (atomic: temp file + rename; parent directories are created).
///
/// Errors: invalid buffers or parameters ([`DspError::InvalidInput`]), a missing ffmpeg for formats
/// that need it ([`DspError::FfmpegNotFound`]), encoder failures ([`DspError::Encode`]) and I/O errors.
pub fn encode_file(
    buffer: &AudioBuffer,
    path: &Path,
    options: &EncodeOptions,
) -> Result<EncodeSummary, DspError> {
    wav::validate_buffer(buffer)?;
    let channels = buffer.num_channels();
    let sample_rate = buffer.sample_rate;
    match options.format {
        EncodeFormat::Wav { bit_depth } => {
            let encoding = wav_encoding(bit_depth, options.dither)?;
            let bytes = wav::write(path, buffer, encoding)?;
            Ok(EncodeSummary {
                bytes,
                sample_rate,
                channels,
                bit_depth: Some(bit_depth),
                bitrate_bps: None,
                encoder: "hound".into(),
            })
        }
        EncodeFormat::Flac { bit_depth } => {
            let data = encode_flac(buffer, bit_depth, options.dither)?;
            let bytes = fsutil::write_bytes_atomic(path, &data)?;
            Ok(EncodeSummary {
                bytes,
                sample_rate,
                channels,
                bit_depth: Some(bit_depth),
                bitrate_bps: None,
                encoder: "flacenc".into(),
            })
        }
        EncodeFormat::Mp3 { bitrate_bps } => {
            let bitrate = mp3_settings(buffer, bitrate_bps)?;
            #[cfg(feature = "mp3")]
            {
                let data = encode_mp3(buffer, bitrate)?;
                let bytes = fsutil::write_bytes_atomic(path, &data)?;
                Ok(EncodeSummary {
                    bytes,
                    sample_rate,
                    channels,
                    bit_depth: None,
                    bitrate_bps: Some(bitrate),
                    encoder: "lame".into(),
                })
            }
            #[cfg(not(feature = "mp3"))]
            {
                encode_ffmpeg(buffer, path, options, &FFMPEG_MP3, bitrate)
            }
        }
        EncodeFormat::Ogg { bitrate_bps } => encode_ffmpeg(
            buffer,
            path,
            options,
            &FFMPEG_OGG,
            lossy_bitrate(bitrate_bps)?,
        ),
        EncodeFormat::Opus { bitrate_bps } => encode_ffmpeg(
            buffer,
            path,
            options,
            &FFMPEG_OPUS,
            lossy_bitrate(bitrate_bps)?,
        ),
        EncodeFormat::M4a { bitrate_bps } => encode_ffmpeg(
            buffer,
            path,
            options,
            &FFMPEG_M4A,
            lossy_bitrate(bitrate_bps)?,
        ),
    }
}

/// Encode a natively supported format (wav, flac, mp3 with the `mp3` feature) into memory.
///
/// Errors with [`DspError::InvalidInput`] for formats that need ffmpeg.
pub fn encode_to_vec(buffer: &AudioBuffer, options: &EncodeOptions) -> Result<Vec<u8>, DspError> {
    wav::validate_buffer(buffer)?;
    match options.format {
        EncodeFormat::Wav { bit_depth } => {
            wav::encode_to_vec(buffer, wav_encoding(bit_depth, options.dither)?)
        }
        EncodeFormat::Flac { bit_depth } => encode_flac(buffer, bit_depth, options.dither),
        #[cfg(feature = "mp3")]
        EncodeFormat::Mp3 { bitrate_bps } => {
            let bitrate = mp3_settings(buffer, bitrate_bps)?;
            encode_mp3(buffer, bitrate)
        }
        other => Err(DspError::InvalidInput(format!(
            "{} can only be encoded to a file in this build (it needs ffmpeg)",
            other.name()
        ))),
    }
}

fn wav_encoding(bit_depth: u16, dither: bool) -> Result<WavEncoding, DspError> {
    match bit_depth {
        16 => Ok(WavEncoding::Int16 { dither }),
        24 => Ok(WavEncoding::Int24),
        32 => Ok(WavEncoding::Float32),
        other => Err(DspError::InvalidInput(format!(
            "WAV bit depth must be 16, 24 or 32, not {other}"
        ))),
    }
}

fn lossy_bitrate(bitrate_bps: u32) -> Result<u32, DspError> {
    if bitrate_bps == 0 {
        return Err(DspError::InvalidInput("bitrate must be positive".into()));
    }
    Ok(bitrate_bps)
}

/// Validate an MP3 encode and return the bitrate to use (bps).
fn mp3_settings(buffer: &AudioBuffer, bitrate_bps: u32) -> Result<u32, DspError> {
    if !(1..=2).contains(&buffer.num_channels()) {
        return Err(DspError::InvalidInput(format!(
            "MP3 supports 1 or 2 channels, not {}",
            buffer.num_channels()
        )));
    }
    lossy_bitrate(bitrate_bps)?;
    let unsupported = || {
        DspError::InvalidInput(format!(
            "MP3 does not support {} Hz (use one of {})",
            buffer.sample_rate,
            MP3_SAMPLE_RATES
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    };
    let nearest = nearest_mp3_bitrate(buffer.sample_rate, bitrate_bps).ok_or_else(unsupported)?;
    let floor =
        mp3_min_gapless_bitrate(buffer.sample_rate, buffer.num_channels()).unwrap_or(nearest);
    Ok(nearest.max(floor))
}

#[cfg(feature = "mp3")]
fn encode_mp3(buffer: &AudioBuffer, bitrate_bps: u32) -> Result<Vec<u8>, DspError> {
    use mp3lame_encoder::{
        Builder, DualPcm, FlushGap, Mode, MonoPcm, Quality, VbrMode, max_required_buffer_size,
    };
    use std::num::NonZeroU32;

    const CHUNK: usize = 1152 * 8;
    let what = "MP3";
    let err =
        |stage: &str, e: &dyn std::fmt::Display| DspError::encode(what, format!("{stage}: {e}"));
    let channels = buffer.num_channels();
    let kbps = bitrate_bps / 1000;

    let mut builder =
        Builder::new().ok_or_else(|| DspError::encode(what, "LAME initialization failed"))?;
    builder
        .set_num_channels(channels as u8)
        .map_err(|e| err("channels", &e))?;
    builder
        .set_sample_rate(buffer.sample_rate)
        .map_err(|e| err("sample rate", &e))?;
    // Pin the output rate: LAME otherwise resamples at low bitrates.
    builder
        .set_output_sample_rate(NonZeroU32::new(buffer.sample_rate))
        .map_err(|e| err("output sample rate", &e))?;
    // mp3lame-encoder's `Bitrate` enum lacks 56 and 144 kbps; set the validated value directly.
    // SAFETY: `as_ptr` returns the builder's live `lame_global_flags`; it is not freed or retained.
    let status = unsafe {
        mp3lame_encoder::ffi::lame_set_brate(builder.as_ptr(), kbps as std::os::raw::c_int)
    };
    if status != 0 {
        return Err(DspError::encode(what, format!("LAME rejected {kbps} kbps")));
    }
    builder
        .set_mode(if channels == 1 {
            Mode::Mono
        } else {
            Mode::JointStereo
        })
        .map_err(|e| err("mode", &e))?;
    builder
        .set_quality(Quality::NearBest)
        .map_err(|e| err("quality", &e))?;
    builder
        .set_vbr_mode(VbrMode::Off)
        .map_err(|e| err("CBR", &e))?;
    builder
        .set_to_write_vbr_tag(true)
        .map_err(|e| err("Info tag", &e))?;
    let mut encoder = builder.build().map_err(|e| err("configuration", &e))?;

    let frames = buffer.frames();
    let mut out: Vec<u8> =
        Vec::with_capacity(max_required_buffer_size(frames.min(CHUNK)) + frames / 8);
    let mut pos = 0;
    while pos < frames {
        let end = (pos + CHUNK).min(frames);
        // encode_to_vec writes into spare capacity only, and LAME treats 0 spare bytes as "unlimited":
        // always reserve the worst case first.
        out.reserve(max_required_buffer_size(end - pos));
        if channels == 1 {
            encoder.encode_to_vec(MonoPcm(&buffer.channels[0][pos..end]), &mut out)
        } else {
            encoder.encode_to_vec(
                DualPcm {
                    left: &buffer.channels[0][pos..end],
                    right: &buffer.channels[1][pos..end],
                },
                &mut out,
            )
        }
        .map_err(|e| err("encoding", &e))?;
        pos = end;
    }
    out.reserve(7200);
    // FlushGap encodes the buffered tail (FlushNoGap would drop it).
    encoder
        .flush_to_vec::<FlushGap>(&mut out)
        .map_err(|e| err("flush", &e))?;

    // Overwrite LAME's zero-filled placeholder frame with the real Info/LAME tag (delay + padding).
    if encoder.is_lame_tag_written() {
        let mut tag = Vec::with_capacity(encoder.lame_tag_size().max(1));
        if let Some(len) = encoder.lame_tag_encode_to_vec(&mut tag) {
            let at = encoder.id3v2_tag_size();
            let len = len.get();
            match out.get_mut(at..at + len) {
                Some(slot) => slot.copy_from_slice(&tag[..len]),
                None => {
                    return Err(DspError::encode(
                        what,
                        "LAME tag frame is missing from the stream",
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// A [`flacenc::source::Source`] that quantizes a planar f32 buffer block by block (no full-length
/// integer copy).
struct FlacSource<'a> {
    buffer: &'a AudioBuffer,
    bits: usize,
    quantizer: Quantizer,
    pos: usize,
    scratch: Vec<i32>,
}

impl flacenc::source::Source for FlacSource<'_> {
    fn channels(&self) -> usize {
        self.buffer.num_channels()
    }

    fn bits_per_sample(&self) -> usize {
        self.bits
    }

    fn sample_rate(&self) -> usize {
        self.buffer.sample_rate as usize
    }

    fn read_samples<F: flacenc::source::Fill>(
        &mut self,
        block_size: usize,
        dest: &mut F,
    ) -> Result<usize, flacenc::error::SourceError> {
        let frames = self.buffer.frames();
        let n = block_size.min(frames - self.pos);
        self.scratch.clear();
        for i in self.pos..self.pos + n {
            for ch in &self.buffer.channels {
                self.scratch.push(self.quantizer.quantize(ch[i]));
            }
        }
        self.pos += n;
        if n > 0 {
            dest.fill_interleaved(&self.scratch)?;
        }
        Ok(n)
    }

    fn len_hint(&self) -> Option<usize> {
        Some(self.buffer.frames())
    }
}

fn encode_flac(buffer: &AudioBuffer, bit_depth: u16, dither: bool) -> Result<Vec<u8>, DspError> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    if bit_depth != 16 && bit_depth != 24 {
        return Err(DspError::InvalidInput(format!(
            "FLAC bit depth must be 16 or 24, not {bit_depth}"
        )));
    }
    if buffer.num_channels() > 8 {
        return Err(DspError::InvalidInput(format!(
            "FLAC supports up to 8 channels, not {}",
            buffer.num_channels()
        )));
    }
    let what = "FLAC";
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, e)| DspError::encode(what, format!("configuration: {e}")))?;
    let block_size = config.block_size;
    let source = FlacSource {
        buffer,
        bits: usize::from(bit_depth),
        quantizer: Quantizer::new(u32::from(bit_depth), dither && bit_depth == 16),
        pos: 0,
        scratch: Vec::with_capacity(block_size * buffer.num_channels()),
    };
    // flacenc errors hold `Rc`s (!Send): stringify them right away.
    let mut stream = flacenc::encode_with_fixed_block_size(&config, source, block_size)
        .map_err(|e| DspError::encode(what, e.to_string()))?;
    // flacenc records the short final block as the minimum block size, which makes symphonia treat the
    // stream as variable-blocksize and fail to open it: restore min = max.
    stream
        .stream_info_mut()
        .set_block_sizes(block_size, block_size)
        .map_err(|e| DspError::encode(what, e.to_string()))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|e| DspError::encode(what, e.to_string()))?;
    Ok(sink.into_inner())
}

/// How a format is produced with ffmpeg.
struct FfmpegTarget {
    /// Encoders to try in order, and whether each needs `-strict experimental`.
    codecs: &'static [(&'static str, bool)],
    /// ffmpeg muxer name (the temp file's extension says nothing).
    muxer: &'static str,
    /// Extra output options.
    extra: &'static [&'static str],
    /// Sample rate the format always uses.
    sample_rate: Option<u32>,
}

const FFMPEG_OGG: FfmpegTarget = FfmpegTarget {
    codecs: &[("libvorbis", false), ("vorbis", true)],
    muxer: "ogg",
    extra: &[],
    sample_rate: None,
};

const FFMPEG_OPUS: FfmpegTarget = FfmpegTarget {
    codecs: &[("libopus", false), ("opus", true)],
    muxer: "opus",
    extra: &[],
    sample_rate: Some(48_000),
};

const FFMPEG_M4A: FfmpegTarget = FfmpegTarget {
    codecs: &[("aac", false)],
    muxer: "ipod",
    extra: &["-movflags", "+faststart"],
    sample_rate: None,
};

#[cfg(not(feature = "mp3"))]
const FFMPEG_MP3: FfmpegTarget = FfmpegTarget {
    codecs: &[("libmp3lame", false)],
    muxer: "mp3",
    extra: &[],
    sample_rate: None,
};

/// Encode through ffmpeg: a 32-bit float WAV on stdin, the muxer writing the temp file directly.
fn encode_ffmpeg(
    buffer: &AudioBuffer,
    path: &Path,
    options: &EncodeOptions,
    target: &FfmpegTarget,
    bitrate_bps: u32,
) -> Result<EncodeSummary, DspError> {
    let format = options.format.name();
    let binary = options
        .ffmpeg
        .resolve()
        .ok_or_else(|| DspError::FfmpegNotFound(format!("{format} encoding")))?;
    let feed = |w: &mut dyn Write| write_float_wav(w, buffer);
    let mut failures = Vec::new();
    for &(codec, experimental) in target.codecs {
        let result = fsutil::write_atomic_path(path, |temp| {
            let mut args = ffmpeg::args([
                "-f",
                "wav",
                "-i",
                "pipe:0",
                "-map_metadata",
                "-1",
                "-fflags",
                "+bitexact",
                "-flags:a",
                "+bitexact",
                "-c:a",
                codec,
            ]);
            if experimental {
                args.extend(ffmpeg::args(["-strict", "experimental"]));
            }
            args.extend(ffmpeg::args(["-b:a".to_string(), bitrate_bps.to_string()]));
            if let Some(rate) = target.sample_rate {
                args.extend(ffmpeg::args(["-ar".to_string(), rate.to_string()]));
            }
            args.extend(ffmpeg::args(target.extra.iter().copied()));
            args.extend(ffmpeg::args(["-f", target.muxer, "-y"]));
            args.push(ffmpeg::file_url(temp));
            ffmpeg::run(&binary, &args, Some(&feed))
                .map(|_| ())
                .map_err(|m| DspError::encode(path.display().to_string(), m))
        });
        match result {
            Ok(bytes) => {
                return Ok(EncodeSummary {
                    bytes,
                    sample_rate: target.sample_rate.unwrap_or(buffer.sample_rate),
                    channels: buffer.num_channels(),
                    bit_depth: None,
                    bitrate_bps: Some(bitrate_bps),
                    encoder: format!("ffmpeg/{codec}"),
                });
            }
            Err(DspError::Encode { message, .. }) => failures.push(format!("{codec}: {message}")),
            Err(other) => return Err(other),
        }
    }
    Err(DspError::encode(
        path.display().to_string(),
        failures.join("; "),
    ))
}

/// Stream `buffer` as a 32-bit IEEE float WAV (sizes `0xFFFFFFFF` when they do not fit in 32 bits).
fn write_float_wav(w: &mut dyn Write, buffer: &AudioBuffer) -> std::io::Result<()> {
    let channels = buffer.num_channels() as u16;
    let rate = buffer.sample_rate;
    let block_align = u32::from(channels) * 4;
    let data_len = buffer.frames() as u64 * u64::from(block_align);
    let (riff_size, data_size) = match u32::try_from(data_len + 36) {
        Ok(riff) => (riff, data_len as u32),
        Err(_) => (u32::MAX, u32::MAX),
    };
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&riff_size.to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    header.extend_from_slice(&channels.to_le_bytes());
    header.extend_from_slice(&rate.to_le_bytes());
    header.extend_from_slice(&(rate * block_align).to_le_bytes());
    header.extend_from_slice(&(block_align as u16).to_le_bytes());
    header.extend_from_slice(&32u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data_size.to_le_bytes());
    w.write_all(&header)?;
    const CHUNK: usize = 8192;
    let mut bytes = Vec::with_capacity(CHUNK * block_align as usize);
    let frames = buffer.frames();
    let mut pos = 0;
    while pos < frames {
        let end = (pos + CHUNK).min(frames);
        bytes.clear();
        for i in pos..end {
            for ch in &buffer.channels {
                let s = ch[i];
                bytes.extend_from_slice(&(if s.is_finite() { s } else { 0.0 }).to_le_bytes());
            }
        }
        w.write_all(&bytes)?;
        pos = end;
    }
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_bitrate_tables() {
        assert_eq!(nearest_mp3_bitrate(48_000, 192_000), Some(192_000));
        assert_eq!(nearest_mp3_bitrate(48_000, 200_000), Some(192_000));
        assert_eq!(nearest_mp3_bitrate(44_100, 999_000), Some(320_000));
        assert_eq!(nearest_mp3_bitrate(44_100, 8_000), Some(32_000));
        assert_eq!(nearest_mp3_bitrate(24_000, 192_000), Some(160_000));
        assert_eq!(
            nearest_mp3_bitrate(22_050, 52_000),
            Some(48_000),
            "ties go down"
        );
        assert_eq!(nearest_mp3_bitrate(8000, 56_000), Some(56_000));
        assert_eq!(nearest_mp3_bitrate(96_000, 192_000), None);
        for rate in MP3_SAMPLE_RATES {
            assert!(!mp3_bitrates_kbps(rate).is_empty());
        }
    }

    #[test]
    fn gapless_bitrate_floor() {
        assert_eq!(mp3_min_gapless_bitrate(48_000, 1), Some(64_000));
        assert_eq!(mp3_min_gapless_bitrate(48_000, 2), Some(64_000));
        assert_eq!(mp3_min_gapless_bitrate(44_100, 2), Some(64_000));
        assert_eq!(mp3_min_gapless_bitrate(32_000, 2), Some(48_000));
        assert_eq!(mp3_min_gapless_bitrate(24_000, 1), Some(64_000));
        assert_eq!(mp3_min_gapless_bitrate(22_050, 2), Some(56_000));
        assert_eq!(mp3_min_gapless_bitrate(16_000, 1), Some(40_000));
        assert_eq!(mp3_min_gapless_bitrate(8000, 1), Some(24_000));
        assert_eq!(mp3_min_gapless_bitrate(96_000, 1), None);
        let buf = AudioBuffer::silent(16_000, 1, 10);
        assert_eq!(mp3_settings(&buf, 32_000).unwrap(), 40_000);
        assert_eq!(mp3_settings(&buf, 128_000).unwrap(), 128_000);
    }

    #[test]
    fn float_wav_header_is_parseable() {
        let buf = AudioBuffer::new(
            22_050,
            vec![vec![0.5, -0.25, 1.5], vec![0.0, f32::NAN, -1.0]],
        );
        let mut bytes = Vec::new();
        write_float_wav(&mut bytes, &buf).unwrap();
        assert_eq!(bytes.len(), 44 + 3 * 2 * 4);
        let back = wav::parse(&bytes).unwrap();
        assert_eq!(
            back.channels,
            vec![vec![0.5, -0.25, 1.5], vec![0.0, 0.0, -1.0]]
        );
        assert_eq!(back.sample_rate, 22_050);
    }

    #[test]
    fn format_metadata() {
        assert_eq!(
            EncodeFormat::Opus {
                bitrate_bps: 96_000
            }
            .name(),
            "opus"
        );
        assert!(EncodeFormat::M4a { bitrate_bps: 1 }.requires_ffmpeg());
        assert!(!EncodeFormat::Flac { bit_depth: 16 }.requires_ffmpeg());
        assert_eq!(
            EncodeFormat::Mp3 { bitrate_bps: 1 }.requires_ffmpeg(),
            !cfg!(feature = "mp3")
        );
    }

    #[test]
    fn invalid_parameters_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.out");
        let buf = AudioBuffer::silent(48_000, 2, 100);
        let opts = |format| EncodeOptions {
            format,
            dither: true,
            ffmpeg: Ffmpeg::Disabled,
        };
        assert!(matches!(
            encode_file(&buf, &p, &opts(EncodeFormat::Wav { bit_depth: 8 })),
            Err(DspError::InvalidInput(_))
        ));
        assert!(matches!(
            encode_file(&buf, &p, &opts(EncodeFormat::Flac { bit_depth: 32 })),
            Err(DspError::InvalidInput(_))
        ));
        let odd_rate = AudioBuffer::silent(96_000, 2, 100);
        assert!(matches!(
            encode_file(
                &odd_rate,
                &p,
                &opts(EncodeFormat::Mp3 {
                    bitrate_bps: 192_000
                })
            ),
            Err(DspError::InvalidInput(_))
        ));
        let surround = AudioBuffer::silent(48_000, 6, 100);
        assert!(matches!(
            encode_file(
                &surround,
                &p,
                &opts(EncodeFormat::Mp3 {
                    bitrate_bps: 192_000
                })
            ),
            Err(DspError::InvalidInput(_))
        ));
        assert!(matches!(
            encode_file(
                &buf,
                &p,
                &opts(EncodeFormat::Ogg {
                    bitrate_bps: 160_000
                })
            ),
            Err(DspError::FfmpegNotFound(_))
        ));
        assert!(
            encode_to_vec(
                &buf,
                &opts(EncodeFormat::M4a {
                    bitrate_bps: 160_000
                })
            )
            .is_err()
        );
        assert!(!p.exists());
    }
}
