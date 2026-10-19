//! Decoding audio files and in-memory audio into [`AudioBuffer`]s.
//!
//! Backends, tried in order:
//!
//! 1. **WAV** (`RIFF`/`RF64`/`BW64` magic): the tolerant reader in [`crate::wav`] (handles `FLLR`/`LIST`
//!    chunks and streaming `0`/`0xFFFFFFFF` sizes exactly).
//! 2. **symphonia 0.6** for wav, mp3, flac, ogg/vorbis, m4a/aac, aiff. symphonia ignores MP4 edit
//!    lists, so AAC in MP4 would come out 1024+ frames late and too long: the `elst` box is read
//!    separately and applied. The first audio track is decoded to planar f32 with gapless trimming;
//!    corrupt packets are skipped, and a stream that ends early is kept (with a warning).
//! 3. **ffmpeg** (when enabled and found): anything symphonia rejects, e.g. Opus (symphonia 0.6.1 has no
//!    Opus decoder). ffmpeg converts to a 32-bit float WAV on a pipe, which is read as it arrives (its
//!    header with the WAV parser, then block by block), so the WAV bytes are never held next to the
//!    decoded audio.
//!
//! The same symphonia and ffmpeg readers also stream a file block by block without keeping it, for
//! measuring a delivered file ([`crate::loudness::measure_file`]).
//!
//! Whatever the backend, the result is normalized the same way:
//!
//! * **Sample rate**: files outside [`MIN_SAMPLE_RATE`]–[`MAX_SAMPLE_RATE`] are rejected (a WAV header
//!   can claim any rate, and a resampler for a 4 MHz or 1 Hz "rate" needs gigabytes).
//! * **Non-finite samples** (NaN, ±∞ in float data) are replaced with silence and counted in
//!   [`DecodeInfo::repaired_samples`] (`audio_repaired`).
//! * **More than two channels** are mixed down to stereo with ITU-R BS.775 ([`crate::downmix`]),
//!   using the file's channel layout when it has one; [`DecodeInfo::downmix`] says so
//!   (`channels_downmixed`). [`ProbeInfo::channels`] still reports the file's channel count.
//!
//! # MPEG audio without a length header
//!
//! An MP3 with a Xing/Info/VBRI header (every LAME or ffmpeg encode) announces its exact frame count
//! and gapless delay/padding. Without one, symphonia's MP3 reader estimates the length from the size of
//! the first frames and the file size, and then cuts every packet past that estimate, so a VBR file can
//! lose half its audio. MPEG audio is therefore read as a stream (unseekable source): the header is
//! still honoured, but a headerless file is decoded to its real end, and [`probe_file_with`] counts it
//! by decoding. ADTS (raw AAC) likewise only has a size-based estimate. When a full decode differs
//! from such an estimate, [`DecodeInfo::notes`] says so.

use std::io::Cursor;
use std::path::Path;

use symphonia::core::audio::{ChannelLabel, Channels};
use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::well_known::{
    FORMAT_ID_ADTS, FORMAT_ID_ISOMP4, FORMAT_ID_MP1, FORMAT_ID_MP2, FORMAT_ID_MP3,
};
use symphonia::core::formats::{FormatId, FormatOptions, FormatReader, Track, TrackType};
use symphonia::core::io::{
    MediaSource, MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource,
};
use symphonia::core::meta::MetadataOptions;

use crate::downmix::{self, Downmix, Layout};
use crate::ffmpeg::{self, Ffmpeg};
use crate::fsutil::ScratchFile;
use crate::wav;
use crate::{AudioBuffer, DspError};

/// Lowest sample rate a source may have (Hz).
pub const MIN_SAMPLE_RATE: u32 = 1000;
/// Highest sample rate a source may have (Hz).
pub const MAX_SAMPLE_RATE: u32 = 768_000;

/// Options for [`decode_file_with`], [`decode_bytes_with`] and [`probe_file_with`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecodeOptions {
    /// Whether and where to find ffmpeg for the fallback decoder.
    pub ffmpeg: Ffmpeg,
}

/// Which decoder produced the audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodeBackend {
    /// The built-in tolerant WAV reader.
    Wav,
    /// symphonia.
    Symphonia,
    /// The ffmpeg fallback.
    Ffmpeg,
}

/// Details about a decode, for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeInfo {
    /// The decoder that produced the audio.
    pub backend: DecodeBackend,
    /// Container short name (`wav`, `mp3`, `isomp4`, `ogg`, ...; `ffmpeg` for the fallback).
    pub container: String,
    /// Codec short name (`pcm`, `mp3`, `aac`, `vorbis`, `flac`, ...).
    pub codec: String,
    /// Playable frame count announced by the container, if any (never a size-based estimate).
    pub expected_frames: Option<u64>,
    /// Packets skipped because they could not be decoded.
    pub skipped_packets: usize,
    /// Non-fatal problems (truncated stream, skipped packets, why a backend was bypassed, ...).
    pub warnings: Vec<String>,
    /// Non-finite samples (NaN, ±∞) that were replaced with silence (`audio_repaired`).
    pub repaired_samples: u64,
    /// Set when the source had more than two channels and was mixed down to stereo
    /// (`channels_downmixed`).
    pub downmix: Option<Downmix>,
    /// Remarks that are not problems, e.g. that a headerless VBR MP3 is longer than its size-based
    /// length estimate.
    pub notes: Vec<String>,
}

impl DecodeInfo {
    fn new(backend: DecodeBackend, container: &str, codec: &str) -> Self {
        DecodeInfo {
            backend,
            container: container.into(),
            codec: codec.into(),
            expected_frames: None,
            skipped_packets: 0,
            warnings: Vec::new(),
            repaired_samples: 0,
            downmix: None,
            notes: Vec::new(),
        }
    }
}

/// Decoded audio plus [`DecodeInfo`].
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    /// The audio at its native sample rate, mono or stereo (wider sources are downmixed).
    pub buffer: AudioBuffer,
    /// How it was decoded.
    pub info: DecodeInfo,
}

/// `(frames, sample_rate, channels)` of an audio file, see [`probe_file_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProbeInfo {
    /// Playable frames at the native sample rate.
    pub frames: u64,
    /// Native sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count of the file (decoding mixes more than two down to stereo).
    pub channels: usize,
}

/// The error for a source sample rate outside [`MIN_SAMPLE_RATE`]–[`MAX_SAMPLE_RATE`], if it is.
fn check_rate(rate: u32, what: &str) -> Result<(), DspError> {
    if (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&rate) {
        return Ok(());
    }
    // No tool names here: the engine chooses its hint from the words of this message.
    Err(DspError::decode(
        what,
        format!(
            "unsupported sample rate {rate} Hz (audio sources must be between {MIN_SAMPLE_RATE} and \
             {MAX_SAMPLE_RATE} Hz); resample the file to a standard rate such as 48000 Hz"
        ),
    ))
}

/// Decode an audio file (wav, mp3, flac, ogg/vorbis, m4a/aac, aiff). Falls back to ffmpeg (if on PATH)
/// when symphonia cannot decode the file.
pub fn decode_file(path: &Path) -> Result<AudioBuffer, DspError> {
    decode_file_with(path, &DecodeOptions::default()).map(|d| d.buffer)
}

/// Decode in-memory audio. `extension_hint` (e.g. "mp3", "wav") helps format probing.
pub fn decode_bytes(bytes: Vec<u8>, extension_hint: Option<&str>) -> Result<AudioBuffer, DspError> {
    decode_bytes_with(&bytes, extension_hint, &DecodeOptions::default()).map(|d| d.buffer)
}

/// Decode an audio file with explicit options, returning decode details.
pub fn decode_file_with(path: &Path, options: &DecodeOptions) -> Result<Decoded, DspError> {
    let bytes = std::fs::read(path).map_err(|e| DspError::io(path, e))?;
    let ext = path.extension().and_then(|e| e.to_str());
    decode_impl(
        &bytes,
        ext,
        &path.display().to_string(),
        options,
        FfmpegInput::Path(path),
    )
}

/// Decode in-memory audio with explicit options, returning decode details.
pub fn decode_bytes_with(
    bytes: &[u8],
    extension_hint: Option<&str>,
    options: &DecodeOptions,
) -> Result<Decoded, DspError> {
    let what = match extension_hint {
        Some(ext) => format!("in-memory {ext} audio ({} bytes)", bytes.len()),
        None => format!("in-memory audio ({} bytes)", bytes.len()),
    };
    decode_impl(bytes, extension_hint, &what, options, FfmpegInput::Bytes)
}

/// Where the ffmpeg fallback reads from.
enum FfmpegInput<'a> {
    /// The original file.
    Path(&'a Path),
    /// The in-memory bytes (written to a scratch file first, since some containers need seeking).
    Bytes,
}

/// Why symphonia did not produce audio.
enum SymphoniaFailure {
    /// It cannot read the data (the ffmpeg fallback gets its chance).
    Unreadable(String),
    /// An error no other backend would avoid (an unsupported sample rate, a cancellation, a sink's
    /// error).
    Fatal(DspError),
}

/// Raw decoder output before the common normalization ([`finish`]).
struct RawDecode {
    buffer: AudioBuffer,
    layout: Layout,
    info: DecodeInfo,
}

/// The normalization every backend gets: non-finite samples → 0 (counted), wide layouts → stereo.
fn finish(raw: RawDecode) -> Decoded {
    let RawDecode {
        mut buffer,
        layout,
        mut info,
    } = raw;
    info.repaired_samples += buffer.replace_non_finite();
    if let Some((stereo, downmix)) = downmix::to_stereo_with(&buffer, &layout) {
        buffer = stereo;
        info.downmix = Some(downmix);
    }
    Decoded { buffer, info }
}

fn wav_layout(mask: Option<u32>) -> Layout {
    mask.map_or(Layout::Unknown, |m| Layout::Mask(u64::from(m)))
}

fn decode_impl(
    bytes: &[u8],
    ext: Option<&str>,
    what: &str,
    options: &DecodeOptions,
    ffmpeg_input: FfmpegInput<'_>,
) -> Result<Decoded, DspError> {
    let mut bypass_notes = Vec::new();
    if wav::is_wav(bytes) {
        // Reject an implausible rate before decoding (and before any other backend tries).
        if let Ok(header) = wav::parse_header(bytes, bytes.len() as u64) {
            check_rate(header.sample_rate, what)?;
        }
        match wav::parse_detailed(bytes) {
            Ok(parsed) => {
                let mut info = DecodeInfo::new(DecodeBackend::Wav, "wav", "pcm");
                info.expected_frames = Some(parsed.buffer.frames() as u64);
                info.repaired_samples = parsed.repaired;
                return Ok(finish(RawDecode {
                    buffer: parsed.buffer,
                    layout: wav_layout(parsed.info.channel_mask),
                    info,
                }));
            }
            // ADPCM, µ-law, odd headers, ...: let symphonia (then ffmpeg) try.
            Err(e) => bypass_notes.push(format!("built-in WAV reader: {}", e.message())),
        }
    }

    let symphonia_error = match guard(|| Ok(decode_symphonia(bytes, ext, what))) {
        Ok(Ok(mut raw)) => {
            raw.info.warnings.splice(0..0, bypass_notes);
            return Ok(finish(raw));
        }
        Ok(Err(SymphoniaFailure::Fatal(error))) => return Err(error),
        Ok(Err(SymphoniaFailure::Unreadable(message))) | Err(message) => message,
    };

    let Some(binary) = options.ffmpeg.resolve() else {
        return Err(DspError::decode(what, symphonia_error));
    };
    let scratch;
    let input_path = match ffmpeg_input {
        FfmpegInput::Path(p) => p,
        FfmpegInput::Bytes => {
            scratch = ScratchFile::with_contents(bytes, ext)?;
            scratch.path()
        }
    };
    match decode_ffmpeg(&binary, input_path, what)? {
        Ok((buffer, header, repaired)) => {
            let mut info = DecodeInfo::new(DecodeBackend::Ffmpeg, "ffmpeg", "ffmpeg");
            info.warnings = bypass_notes;
            info.warnings.push(format!(
                "decoded with ffmpeg because symphonia failed: {symphonia_error}"
            ));
            info.repaired_samples = repaired;
            Ok(finish(RawDecode {
                buffer,
                layout: wav_layout(header.channel_mask),
                info,
            }))
        }
        Err(ffmpeg_error) => Err(DspError::decode(
            what,
            format!("{symphonia_error}; ffmpeg fallback also failed: {ffmpeg_error}"),
        )),
    }
}

/// The ffmpeg arguments that decode `input` (first audio stream) to a 32-bit float WAV on stdout.
pub(crate) fn ffmpeg_decode_args(input: &Path) -> Vec<std::ffi::OsString> {
    let mut args = ffmpeg::args(["-i"]);
    args.push(ffmpeg::file_url(input));
    args.extend(ffmpeg::args([
        "-map",
        "0:a:0",
        "-vn",
        "-sn",
        "-dn",
        "-map_metadata",
        "-1",
        "-c:a",
        "pcm_f32le",
        "-f",
        "wav",
        "pipe:1",
    ]));
    args
}

/// Frames per channel handed to a [`StreamSink`] at a time by the ffmpeg reader.
const PIPE_BLOCK: usize = 8192;
/// The WAV header ffmpeg writes to a pipe is a few dozen bytes; give up on anything this large.
const MAX_PIPE_HEADER: usize = 1 << 20;

/// Why the ffmpeg pipe reader stopped.
enum PipeError {
    /// ffmpeg's output is not the expected WAV (the caller reports it with ffmpeg's failure).
    Unreadable(String),
    /// An error no backend would avoid (an unsupported sample rate, the sink's own error).
    Fatal(DspError),
}

/// Reads the 32-bit float WAV that ffmpeg writes to a pipe as it arrives: the header (its sizes are
/// unknown on a pipe), then whole frames, handed to a [`StreamSink`] in planar blocks with non-finite
/// samples replaced by 0.
struct PipeWav<'s> {
    what: &'s str,
    /// Bytes received before the header was complete.
    head: Vec<u8>,
    info: Option<wav::WavInfo>,
    /// Sample bytes the `data` chunk still holds (unbounded for a streaming header).
    remaining: u64,
    /// The start of a frame split across two chunks.
    partial: Vec<u8>,
    block: Vec<Vec<f32>>,
    /// Non-finite samples replaced with 0.
    repaired: u64,
    frames: u64,
}

impl<'s> PipeWav<'s> {
    fn new(what: &'s str) -> Self {
        PipeWav {
            what,
            head: Vec::new(),
            info: None,
            remaining: 0,
            partial: Vec::new(),
            block: Vec::new(),
            repaired: 0,
            frames: 0,
        }
    }

    /// Take the next chunk of ffmpeg's output.
    fn feed(&mut self, bytes: &[u8], sink: &mut dyn StreamSink) -> Result<(), PipeError> {
        if self.info.is_some() {
            return self.samples(bytes, sink);
        }
        self.head.extend_from_slice(bytes);
        let info = match wav::parse_header(&self.head, u64::MAX) {
            Ok(info) => info,
            Err(wav::HeaderError::NeedMore) if self.head.len() < MAX_PIPE_HEADER => return Ok(()),
            Err(e) => {
                return Err(PipeError::Unreadable(format!(
                    "could not read ffmpeg output: {}",
                    e.message()
                )));
            }
        };
        if info.sample_format != wav::WavSampleFormat::Float || info.bits_per_sample != 32 {
            return Err(PipeError::Unreadable(
                "ffmpeg did not produce 32-bit float audio".into(),
            ));
        }
        check_rate(info.sample_rate, self.what).map_err(PipeError::Fatal)?;
        sink.start(info.sample_rate, info.channels)
            .map_err(PipeError::Fatal)?;
        self.remaining = info.data_len;
        self.block = vec![Vec::with_capacity(PIPE_BLOCK); info.channels];
        let head = std::mem::take(&mut self.head);
        let start = usize::try_from(info.data_offset).map_or(head.len(), |s| s.min(head.len()));
        self.info = Some(info);
        self.samples(&head[start..], sink)
    }

    /// Sample bytes: complete frames go to the block, a trailing partial frame waits for the next chunk.
    fn samples(&mut self, bytes: &[u8], sink: &mut dyn StreamSink) -> Result<(), PipeError> {
        let frame_bytes = 4 * self.block.len();
        let take = usize::try_from(self.remaining).map_or(bytes.len(), |r| r.min(bytes.len()));
        self.remaining -= take as u64;
        let mut bytes = &bytes[..take];
        if !self.partial.is_empty() {
            let n = (frame_bytes - self.partial.len()).min(bytes.len());
            self.partial.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.partial.len() < frame_bytes {
                return Ok(());
            }
            let frame = std::mem::take(&mut self.partial);
            self.push(&frame, sink)?;
        }
        let mut frames = bytes.chunks_exact(frame_bytes);
        for frame in &mut frames {
            self.push(frame, sink)?;
        }
        self.partial.extend_from_slice(frames.remainder());
        Ok(())
    }

    fn push(&mut self, frame: &[u8], sink: &mut dyn StreamSink) -> Result<(), PipeError> {
        let (samples, _) = frame.as_chunks::<4>();
        for (plane, &sample) in self.block.iter_mut().zip(samples) {
            plane.push(f32::from_le_bytes(sample));
        }
        if self.block[0].len() >= PIPE_BLOCK {
            self.flush(sink)?;
        }
        Ok(())
    }

    fn flush(&mut self, sink: &mut dyn StreamSink) -> Result<(), PipeError> {
        let n = self.block.first().map_or(0, Vec::len);
        if n == 0 {
            return Ok(());
        }
        for plane in &mut self.block {
            for s in plane.iter_mut().filter(|s| !s.is_finite()) {
                *s = 0.0;
                self.repaired += 1;
            }
        }
        let planes: Vec<&[f32]> = self.block.iter().map(Vec::as_slice).collect();
        sink.frames(&planes).map_err(PipeError::Fatal)?;
        self.frames += n as u64;
        for plane in &mut self.block {
            plane.clear();
        }
        Ok(())
    }
}

/// What ffmpeg decoded (see [`stream_ffmpeg`]).
struct PipeDecode {
    info: wav::WavInfo,
    frames: u64,
    /// Non-finite samples replaced with 0.
    repaired: u64,
}

/// Decode `input` with ffmpeg (its 32-bit float WAV read from the pipe as it arrives) into `sink`,
/// polling `cancel` between chunks. The inner error is a plain message (ffmpeg failed or produced no
/// audio); the outer one is fatal (an unsupported sample rate, a cancellation, the sink's error).
fn stream_ffmpeg(
    binary: &Path,
    input: &Path,
    what: &str,
    sink: &mut dyn StreamSink,
    cancel: &dyn Fn() -> bool,
) -> Result<Result<PipeDecode, String>, DspError> {
    let mut reader = PipeWav::new(what);
    let ran = ffmpeg::run_streaming(binary, &ffmpeg_decode_args(input), &mut |chunk| {
        if cancel() {
            return Err(PipeError::Fatal(DspError::Cancelled));
        }
        reader.feed(chunk, sink)
    });
    let ran = match ran {
        Ok(ran) => ran,
        Err(PipeError::Fatal(e)) => return Err(e),
        Err(PipeError::Unreadable(message)) => Err(message),
    };
    if let Err(message) = ran {
        return Ok(Err(message));
    }
    match reader.flush(sink) {
        Ok(()) => {}
        Err(PipeError::Fatal(e)) => return Err(e),
        Err(PipeError::Unreadable(message)) => return Ok(Err(message)),
    }
    Ok(match reader.info {
        Some(info) if reader.frames > 0 => Ok(PipeDecode {
            info,
            frames: reader.frames,
            repaired: reader.repaired,
        }),
        _ => Err("ffmpeg produced no audio".into()),
    })
}

/// A [`StreamSink`] that keeps everything (the full decode through ffmpeg).
#[derive(Default)]
struct Collect {
    planes: Vec<Vec<f32>>,
}

impl StreamSink for Collect {
    fn start(&mut self, _sample_rate: u32, channels: usize) -> Result<(), DspError> {
        self.planes = vec![Vec::new(); channels];
        Ok(())
    }

    fn frames(&mut self, planes: &[&[f32]]) -> Result<(), DspError> {
        for (plane, samples) in self.planes.iter_mut().zip(planes) {
            plane.extend_from_slice(samples);
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.planes.clear();
    }
}

/// Run ffmpeg on `input` and read its 32-bit float WAV output (streamed from the pipe, so the encoded
/// bytes are never held next to the decoded audio).
fn decode_ffmpeg(
    binary: &Path,
    input: &Path,
    what: &str,
) -> Result<Result<(AudioBuffer, wav::WavInfo, u64), String>, DspError> {
    let mut collect = Collect::default();
    Ok(
        stream_ffmpeg(binary, input, what, &mut collect, &|| false)?.map(|decoded| {
            (
                AudioBuffer::new(decoded.info.sample_rate, collect.planes),
                decoded.info,
                decoded.repaired,
            )
        }),
    )
}

/// Run a third-party decoder step, turning a panic on malformed input into an error so that a bad
/// file can never take the process down (the ffmpeg fallback still gets its chance).
fn guard<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        let detail = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        Err(format!("the decoder crashed on this input ({detail})"))
    })
}

/// The first audio track with a known codec (falling back to the container's default audio track).
fn audio_track(format: &dyn FormatReader) -> Option<Track> {
    format
        .first_track_known_codec(TrackType::Audio)
        .or_else(|| format.default_track(TrackType::Audio))
        .cloned()
}

/// MPEG audio elementary streams (see the module docs).
fn is_mpeg_audio(format: FormatId) -> bool {
    matches!(format, FORMAT_ID_MP1 | FORMAT_ID_MP2 | FORMAT_ID_MP3)
}

/// Open `source` with symphonia's probe.
fn open_format<'a>(
    source: Box<dyn MediaSource + 'a>,
    ext: Option<&str>,
) -> Result<Box<dyn FormatReader + 'a>, String> {
    let mss = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = ext {
        hint.with_extension(ext);
    }
    symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("unrecognized or unsupported format ({e})"))
}

/// A format reader for the media `make` opens. MPEG audio is re-opened (with a second `make()`) as an
/// unseekable stream so that a size-based length estimate never cuts the decode short; the estimate
/// (if any) is returned alongside.
fn open_reader<'a, S: MediaSource + 'a>(
    make: impl Fn() -> std::io::Result<S>,
    ext: Option<&str>,
) -> Result<(Box<dyn FormatReader + 'a>, Option<u64>), String> {
    let source = make().map_err(|e| format!("cannot read the file ({e})"))?;
    let format = open_format(Box::new(source), ext)?;
    let id = format.format_info().format;
    let num_frames = audio_track(format.as_ref()).and_then(|t| t.num_frames);
    if is_mpeg_audio(id) {
        if let Ok(source) = make()
            && let Ok(stream) = open_format(Box::new(ReadOnlySource::new(source)), ext)
            && stream.format_info().format == id
        {
            // Without a Xing/Info/VBRI header the stream reader knows no length: the seekable one's
            // number was an estimate.
            let exact = audio_track(stream.as_ref()).and_then(|t| t.num_frames);
            return Ok((stream, num_frames.filter(|_| exact.is_none())));
        }
    } else if id == FORMAT_ID_ADTS {
        return Ok((format, num_frames));
    }
    Ok((format, None))
}

/// [`open_reader`] for in-memory bytes.
fn open_bytes<'a>(
    bytes: &'a [u8],
    ext: Option<&str>,
) -> Result<(Box<dyn FormatReader + 'a>, Option<u64>), String> {
    open_reader(|| Ok(Cursor::new(bytes)), ext)
}

/// The layout of symphonia's channel description.
fn symphonia_layout(channels: &Channels) -> Layout {
    match channels {
        Channels::Positioned(positions) => Layout::Mask(positions.bits()),
        Channels::Custom(labels) => Layout::Positions(
            labels
                .iter()
                .map(|label| match label {
                    ChannelLabel::Positioned(p) if p.bits().count_ones() == 1 => {
                        Some(p.bits().trailing_zeros())
                    }
                    _ => None,
                })
                .collect(),
        ),
        _ => Layout::Unknown,
    }
}

/// symphonia's reader and decoder for the audio track of a stream.
struct SymphoniaStream<'a> {
    format: Box<dyn FormatReader + 'a>,
    decoder: Box<dyn AudioDecoder>,
    track: Track,
    params: AudioCodecParameters,
    format_id: FormatId,
    container: String,
    codec: String,
}

/// Receives each decoded packet: its sample rate and planar samples (see [`SymphoniaStream::decode`]).
type Emit<'e> = dyn FnMut(u32, &mut [Vec<f32>]) -> Result<(), DspError> + 'e;

/// What [`SymphoniaStream::decode`] saw.
#[derive(Debug, Default)]
struct Packets {
    /// Sample rate, channel count and layout of the first decoded packet (`None`: nothing decoded).
    first: Option<(u32, usize, Layout)>,
    /// Frames decoded (and handed on).
    decoded: u64,
    skipped_packets: usize,
    first_skip_reason: Option<String>,
    warnings: Vec<String>,
    /// Frames the demuxer cut off packet ends (gapless padding, or audio past the announced length).
    trimmed_tail: u64,
}

impl<'a> SymphoniaStream<'a> {
    fn new(format: Box<dyn FormatReader + 'a>, what: &str) -> Result<Self, SymphoniaFailure> {
        use SymphoniaFailure::{Fatal, Unreadable};
        let track =
            audio_track(format.as_ref()).ok_or(Unreadable("no audio track found".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or(Unreadable("the audio track has no codec parameters".into()))?
            .clone();
        if let Some(rate) = params.sample_rate {
            check_rate(rate, what).map_err(Fatal)?;
        }
        let decoder: Box<dyn AudioDecoder> = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| Unreadable(format!("unsupported codec ({e})")))?;
        let format_id = format.format_info().format;
        let container = format.format_info().short_name.to_string();
        let codec = decoder.codec_info().short_name.to_string();
        Ok(SymphoniaStream {
            format,
            decoder,
            track,
            params,
            format_id,
            container,
            codec,
        })
    }

    fn is_mp4_aac(&self) -> bool {
        self.format_id == FORMAT_ID_ISOMP4 && self.params.codec == CODEC_ID_AAC
    }

    /// Decode every packet of the track in order, handing each decoded one to `emit` with its sample
    /// rate, as planar samples (one `Vec` per channel, all the same length). Undecodable packets and
    /// packets whose rate or channel count differs from the first decoded one are skipped (and
    /// counted); a stream that ends early keeps what it had (with a warning).
    fn decode(&mut self, what: &str, emit: &mut Emit<'_>) -> Result<Packets, SymphoniaFailure> {
        use SymphoniaFailure::{Fatal, Unreadable};
        let mut out = Packets::default();
        let mut scratch: Vec<Vec<f32>> = Vec::new();
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                // Chained Ogg / track list change: keep what we have.
                Err(SymError::ResetRequired) => {
                    out.warnings.push(
                        "the stream changed its track layout; decoding stopped there".to_string(),
                    );
                    break;
                }
                // A container that promises more data than the file holds: keep what we have.
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    out.warnings.push(format!("demuxing stopped early: {e}"));
                    break;
                }
            };
            if packet.track_id != self.track.id {
                continue;
            }
            if out.first.is_some() {
                out.trimmed_tail += packet.trim_end.get();
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let n = buf.frames();
                    if n == 0 {
                        continue; // e.g. a fully trimmed priming packet
                    }
                    let rate = buf.spec().rate();
                    let nch = buf.num_planes();
                    match &out.first {
                        None => {
                            check_rate(rate, what).map_err(Fatal)?;
                            out.first = Some((rate, nch, symphonia_layout(buf.spec().channels())));
                        }
                        Some((r, c, _)) if *r != rate || *c != nch => {
                            out.skipped_packets += 1;
                            out.first_skip_reason.get_or_insert_with(|| {
                                format!("packet layout changed mid-stream ({nch} ch @ {rate} Hz)")
                            });
                            continue;
                        }
                        Some(_) => {}
                    }
                    scratch.resize_with(nch, Vec::new);
                    for plane in &mut scratch {
                        plane.clear();
                        plane.resize(n, 0.0);
                    }
                    {
                        let mut dst: Vec<&mut [f32]> =
                            scratch.iter_mut().map(Vec::as_mut_slice).collect();
                        buf.copy_to_slice_planar::<f32, _>(dst.as_mut_slice());
                    }
                    out.decoded += n as u64;
                    emit(rate, &mut scratch).map_err(Fatal)?;
                }
                Err(SymError::DecodeError(msg)) => {
                    out.skipped_packets += 1;
                    out.first_skip_reason.get_or_insert_with(|| msg.to_string());
                }
                Err(SymError::IoError(e)) => {
                    out.skipped_packets += 1;
                    out.first_skip_reason.get_or_insert_with(|| e.to_string());
                }
                Err(e) => return Err(Unreadable(format!("decoding failed: {e}"))),
            }
        }
        if out.skipped_packets > 0 {
            out.warnings.push(format!(
                "skipped {} undecodable packet(s) (first: {})",
                out.skipped_packets,
                out.first_skip_reason.as_deref().unwrap_or("unknown")
            ));
        }
        Ok(out)
    }
}

/// Decode with symphonia. Errors are plain messages (the caller adds context / tries ffmpeg), except
/// for a sample rate no backend could use.
fn decode_symphonia(
    bytes: &[u8],
    ext: Option<&str>,
    what: &str,
) -> Result<RawDecode, SymphoniaFailure> {
    use SymphoniaFailure::Unreadable;
    let (format, estimated_frames) = open_bytes(bytes, ext).map_err(Unreadable)?;
    let mut stream = SymphoniaStream::new(format, what)?;
    let mut planes: Vec<Vec<f32>> = Vec::new();
    let packets = stream.decode(what, &mut |_, block| {
        if planes.is_empty() {
            planes = vec![Vec::new(); block.len()];
        }
        for (plane, samples) in planes.iter_mut().zip(block.iter()) {
            plane.extend_from_slice(samples);
        }
        Ok(())
    })?;
    let is_mp4_aac = stream.is_mp4_aac();
    let SymphoniaStream {
        track,
        params,
        format_id,
        container,
        codec,
        ..
    } = stream;
    let mut warnings = packets.warnings;
    let mut info = DecodeInfo::new(DecodeBackend::Symphonia, &container, &codec);
    info.skipped_packets = packets.skipped_packets;
    let channels_hint = params
        .channels
        .as_ref()
        .map(|c| c.count())
        .filter(|&c| c > 0);
    let announced = if format_id == FORMAT_ID_ADTS {
        None // only a size-based estimate
    } else {
        track.num_frames
    };
    let Some((sample_rate, _, layout)) = packets.first else {
        // A valid but empty stream: return an empty buffer when the layout is known.
        return match (params.sample_rate.unwrap_or(0), channels_hint) {
            (rate, Some(ch)) if rate > 0 && packets.skipped_packets == 0 => {
                info.expected_frames = announced;
                info.warnings = warnings;
                Ok(RawDecode {
                    buffer: AudioBuffer::silent(rate, ch, 0),
                    layout: params
                        .channels
                        .as_ref()
                        .map_or(Layout::Unknown, symphonia_layout),
                    info,
                })
            }
            _ => Err(Unreadable(format!(
                "no audio could be decoded{}",
                packets
                    .first_skip_reason
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ))),
        };
    };

    let mut expected_frames = announced;
    if is_mp4_aac {
        // symphonia ignores the MP4 edit list: apply it (skip priming, cut padding).
        if let Some(edit) = crate::mp4::audio_edit(bytes) {
            let decoded = planes[0].len() as u64;
            let (skip, keep) = edit.frames(sample_rate);
            if skip < decoded {
                let end = keep.map_or(decoded, |k| (skip + k).min(decoded));
                for p in planes.iter_mut() {
                    p.truncate(end as usize);
                    p.drain(..skip as usize);
                }
                expected_frames = keep.or(expected_frames.map(|f| f.saturating_sub(skip)));
            } else {
                warnings.push(format!(
                    "ignored an MP4 edit list that skips {skip} of {decoded} frames"
                ));
            }
        }
    }
    let got = planes[0].len() as u64;
    if let Some(expected) = expected_frames {
        if got < expected {
            warnings.push(format!(
                "stream ended early: decoded {got} of {expected} frames (the file may be truncated or corrupt)"
            ));
        } else if got > expected && !is_mp4_aac {
            warnings.push(format!(
                "decoded {got} frames but the container announced {expected}"
            ));
        }
    }
    if is_mpeg_audio(format_id) {
        // The encoder padding is at most about one MPEG frame; more means the stream goes on past
        // the length its header announces (e.g. two files joined), and that audio was cut.
        let padding = u64::from(track.padding.unwrap_or(0));
        let dropped = packets.trimmed_tail.saturating_sub(padding);
        if dropped > 1152 {
            warnings.push(format!(
                "the stream continues past the length its Xing/Info header announces; {dropped} frames \
                 ({:.0} ms) after the announced end were not decoded",
                dropped as f64 * 1000.0 / f64::from(sample_rate.max(1))
            ));
        }
    }
    if let Some(estimate) = estimated_frames
        && got.abs_diff(estimate) > estimate / 100 + 1152
    {
        let secs = |f: u64| f as f64 / f64::from(sample_rate.max(1));
        info.notes.push(format!(
            "the {container} stream has no length header, so its length is only known after a full \
             decode: {got} frames ({:.2} s), while its size-based estimate is {estimate} frames ({:.2} \
             s); players that estimate may show a different duration",
            secs(got),
            secs(estimate)
        ));
    }
    info.expected_frames = expected_frames;
    info.warnings = warnings;
    Ok(RawDecode {
        buffer: AudioBuffer::new(sample_rate, planes),
        layout,
        info,
    })
}

/// Receives decoded audio block by block (see [`stream_file`]).
pub(crate) trait StreamSink {
    /// The stream's layout, before its first frames.
    fn start(&mut self, sample_rate: u32, channels: usize) -> Result<(), DspError>;
    /// The next frames: one slice per channel, all the same length, every sample finite.
    fn frames(&mut self, planes: &[&[f32]]) -> Result<(), DspError>;
    /// Forget everything received (a backend failed and the next one starts over).
    fn reset(&mut self);
}

/// What [`stream_file`] decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamSummary {
    pub(crate) backend: DecodeBackend,
    pub(crate) sample_rate: u32,
    pub(crate) channels: usize,
    /// Frames handed to the sink.
    pub(crate) frames: u64,
}

/// Decode the audio file at `path` into `sink` block by block, keeping none of it: symphonia (MPEG audio
/// read to its real end, the MP4 edit list applied, exactly as [`decode_file_with`] reads them), else
/// ffmpeg (per `ffmpeg`) through a pipe. Unlike the full decode, layouts wider than stereo are passed on
/// as they are. Non-finite samples become 0. `cancel` is polled between blocks.
///
/// Errors: I/O, undecodable files and unsupported sample rates ([`DspError::Decode`]), the sink's own
/// errors and [`DspError::Cancelled`].
pub(crate) fn stream_file(
    path: &Path,
    ffmpeg: &Ffmpeg,
    sink: &mut dyn StreamSink,
    cancel: &dyn Fn() -> bool,
) -> Result<StreamSummary, DspError> {
    std::fs::metadata(path).map_err(|e| DspError::io(path, e))?;
    let what = path.display().to_string();
    let ext = path.extension().and_then(|e| e.to_str());
    let symphonia_error = match guard(|| Ok(stream_symphonia(path, ext, &what, sink, cancel))) {
        Ok(Ok(summary)) => return Ok(summary),
        Ok(Err(SymphoniaFailure::Fatal(error))) => return Err(error),
        Ok(Err(SymphoniaFailure::Unreadable(message))) | Err(message) => message,
    };
    sink.reset();
    let Some(binary) = ffmpeg.resolve() else {
        return Err(DspError::decode(what, symphonia_error));
    };
    match stream_ffmpeg(&binary, path, &what, sink, cancel)? {
        Ok(decoded) => Ok(StreamSummary {
            backend: DecodeBackend::Ffmpeg,
            sample_rate: decoded.info.sample_rate,
            channels: decoded.info.channels,
            frames: decoded.frames,
        }),
        Err(ffmpeg_error) => Err(DspError::decode(
            what,
            format!("{symphonia_error}; ffmpeg fallback also failed: {ffmpeg_error}"),
        )),
    }
}

/// The symphonia part of [`stream_file`].
fn stream_symphonia(
    path: &Path,
    ext: Option<&str>,
    what: &str,
    sink: &mut dyn StreamSink,
    cancel: &dyn Fn() -> bool,
) -> Result<StreamSummary, SymphoniaFailure> {
    use SymphoniaFailure::{Fatal, Unreadable};
    let open = || -> Result<SymphoniaStream<'static>, SymphoniaFailure> {
        let (format, _) = open_reader(|| std::fs::File::open(path), ext).map_err(Unreadable)?;
        SymphoniaStream::new(format, what)
    };
    let mut stream = open()?;
    // symphonia ignores the MP4 edit list: skip the priming and cut the padding on the fly.
    let mut edit = if stream.is_mp4_aac() {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| crate::mp4::audio_edit(&bytes))
    } else {
        None
    };
    loop {
        let mut position = 0u64;
        let mut emitted = 0u64;
        let mut layout: Option<(u32, usize)> = None;
        let packets = stream.decode(what, &mut |rate, block| {
            if cancel() {
                return Err(DspError::Cancelled);
            }
            let n = block.first().map_or(0, Vec::len) as u64;
            let (skip, keep) = edit.map_or((0, None), |e| e.frames(rate));
            let from = skip.saturating_sub(position).min(n);
            let to = keep.map_or(n, |k| {
                skip.saturating_add(k).saturating_sub(position).min(n)
            });
            position += n;
            if from >= to {
                return Ok(());
            }
            if layout.is_none() {
                sink.start(rate, block.len())?;
                layout = Some((rate, block.len()));
            }
            let (from, to) = (from as usize, to as usize);
            for plane in block.iter_mut() {
                for s in plane[from..to].iter_mut().filter(|s| !s.is_finite()) {
                    *s = 0.0;
                }
            }
            let planes: Vec<&[f32]> = block.iter().map(|p| &p[from..to]).collect();
            sink.frames(&planes)?;
            emitted += (to - from) as u64;
            Ok(())
        })?;
        if edit.is_some() && emitted == 0 && packets.decoded > 0 {
            // An edit list that skips everything is ignored, as by the full decode: start over.
            edit = None;
            sink.reset();
            stream = open()?;
            continue;
        }
        let (sample_rate, channels) = match layout {
            Some(layout) => layout,
            None => {
                // Nothing decoded: an empty stream when its layout is known.
                let rate = stream.params.sample_rate.unwrap_or(0);
                let channels = stream.params.channels.as_ref().map_or(0, |c| c.count());
                if rate == 0 || channels == 0 || packets.skipped_packets > 0 {
                    return Err(Unreadable(format!(
                        "no audio could be decoded{}",
                        packets
                            .first_skip_reason
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default()
                    )));
                }
                sink.start(rate, channels).map_err(Fatal)?;
                (rate, channels)
            }
        };
        return Ok(StreamSummary {
            backend: DecodeBackend::Symphonia,
            sample_rate,
            channels,
            frames: emitted,
        });
    }
}

/// Convert raw signed 16-bit little-endian interleaved PCM to a buffer.
///
/// A trailing partial frame is dropped. Panics if `num_channels` is 0.
pub fn pcm_s16le_to_buffer(bytes: &[u8], sample_rate: u32, num_channels: usize) -> AudioBuffer {
    assert!(num_channels > 0, "num_channels must be > 0");
    let (pairs, _) = bytes.as_chunks::<2>();
    let samples: Vec<f32> = pairs
        .iter()
        .map(|&b| f32::from(i16::from_le_bytes(b)) / 32768.0)
        .collect();
    AudioBuffer::from_interleaved(sample_rate, num_channels, &samples)
}

/// Convert raw 32-bit float little-endian interleaved PCM to a buffer.
///
/// A trailing partial frame is dropped. Non-finite samples come back as they are (call
/// [`AudioBuffer::replace_non_finite`]). Panics if `num_channels` is 0.
pub fn pcm_f32le_to_buffer(bytes: &[u8], sample_rate: u32, num_channels: usize) -> AudioBuffer {
    assert!(num_channels > 0, "num_channels must be > 0");
    let (quads, _) = bytes.as_chunks::<4>();
    let samples: Vec<f32> = quads.iter().map(|&b| f32::from_le_bytes(b)).collect();
    AudioBuffer::from_interleaved(sample_rate, num_channels, &samples)
}

/// Return `(frames, sample_rate, channels)` of an audio file, decoding as little as possible.
pub fn probe_file(path: &Path) -> Result<(u64, u32, usize), DspError> {
    probe_file_with(path, &DecodeOptions::default()).map(|p| (p.frames, p.sample_rate, p.channels))
}

/// Return the playable length and layout of an audio file, decoding as little as possible.
///
/// WAV headers are parsed directly; other formats use the container's frame count when it is known
/// (for MP4/AAC, the edit list's duration), and are fully decoded otherwise. Files symphonia cannot open
/// are decoded with ffmpeg (if enabled) and counted.
///
/// The count is exact, and equal to what the decoders return: WAV, AIFF, FLAC, Ogg Vorbis, M4A and MP3
/// with an Info/Xing/VBRI header (every LAME/ffmpeg encode) announce it; a headerless MP3 and ADTS
/// AAC are decoded and counted. Errors with [`DspError::Decode`] for a sample rate outside
/// [`MIN_SAMPLE_RATE`]–[`MAX_SAMPLE_RATE`].
pub fn probe_file_with(path: &Path, options: &DecodeOptions) -> Result<ProbeInfo, DspError> {
    let what = path.display().to_string();
    let mut head = [0u8; 12];
    let head_len = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| DspError::io(path, e))?;
        let mut n = 0;
        while n < head.len() {
            match f.read(&mut head[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(DspError::io(path, e)),
            }
        }
        n
    };
    if wav::is_wav(&head[..head_len])
        && let Ok(info) = wav::probe(path)
    {
        check_rate(info.sample_rate, &what)?;
        return Ok(ProbeInfo {
            frames: info.frames,
            sample_rate: info.sample_rate,
            channels: info.channels,
        });
    }

    if let Ok(Some(info)) = guard(|| Ok(probe_symphonia_header(path))) {
        check_rate(info.sample_rate, &what)?;
        return Ok(info);
    }
    // Unknown length (or symphonia cannot open it): decode and count.
    let decoded = decode_file_with(path, options)?;
    if decoded.buffer.sample_rate == 0 {
        return Err(DspError::decode(
            what,
            "could not determine the sample rate",
        ));
    }
    Ok(ProbeInfo {
        frames: decoded.buffer.frames() as u64,
        sample_rate: decoded.buffer.sample_rate,
        channels: decoded
            .info
            .downmix
            .as_ref()
            .map_or(decoded.buffer.num_channels(), |d| d.source_channels),
    })
}

/// Container-level length and layout without decoding, when symphonia knows them exactly.
fn probe_symphonia_header(path: &Path) -> Option<ProbeInfo> {
    let ext = path.extension().and_then(|e| e.to_str());
    let file = std::fs::File::open(path).ok()?;
    let mut format = open_format(Box::new(file), ext).ok()?;
    let id = format.format_info().format;
    if id == FORMAT_ID_ADTS {
        return None; // only a size-based estimate: decode and count
    }
    if is_mpeg_audio(id) {
        // Only a Xing/Info/VBRI header gives an exact length (see the module docs).
        let file = std::fs::File::open(path).ok()?;
        format = open_format(Box::new(ReadOnlySource::new(file)), ext).ok()?;
        if format.format_info().format != id {
            return None;
        }
    }
    let track = audio_track(format.as_ref())?;
    let params = track.codec_params.as_ref()?.audio()?;
    // Only trust the header when a decoder exists (otherwise the full decode path reports the error).
    symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .ok()?;
    let sample_rate = params.sample_rate.filter(|&r| r > 0)?;
    let channels = params
        .channels
        .as_ref()
        .map(|c| c.count())
        .filter(|&c| c > 0)?;
    let frames = if id == FORMAT_ID_ISOMP4 && params.codec == CODEC_ID_AAC {
        // The container's count includes the encoder priming; the edit list has the real length.
        let bytes = std::fs::read(path).ok()?;
        let edit = crate::mp4::audio_edit(&bytes)?;
        edit.frames(sample_rate).1?
    } else {
        track.num_frames?
    };
    Some(ProbeInfo {
        frames,
        sample_rate,
        channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_pcm_helpers() {
        let bytes: Vec<u8> = [16384i16, -16384, 32767, 0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let buf = pcm_s16le_to_buffer(&bytes, 24_000, 2);
        assert_eq!(
            buf.channels,
            vec![vec![0.5, 32767.0 / 32768.0], vec![-0.5, 0.0]]
        );
        let bytes: Vec<u8> = [0.25f32, -0.5, 1.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let buf = pcm_f32le_to_buffer(&bytes[..10], 48_000, 1);
        assert_eq!(
            buf.channels,
            vec![vec![0.25, -0.5]],
            "partial sample dropped"
        );
    }

    #[test]
    fn rejects_non_audio() {
        let err = decode_bytes_with(
            b"this is not audio at all, just text",
            None,
            &DecodeOptions {
                ffmpeg: Ffmpeg::Disabled,
            },
        )
        .unwrap_err();
        assert!(matches!(err, DspError::Decode { .. }), "{err}");
        let err = decode_bytes_with(
            &[],
            Some("mp3"),
            &DecodeOptions {
                ffmpeg: Ffmpeg::Disabled,
            },
        )
        .unwrap_err();
        assert!(matches!(err, DspError::Decode { .. }), "{err}");
    }

    #[test]
    fn missing_file_is_io_error() {
        let err = decode_file(Path::new("/definitely/not/here.wav")).unwrap_err();
        assert!(matches!(err, DspError::Io { .. }), "{err}");
        assert!(matches!(
            probe_file(Path::new("/definitely/not/here.mp3")),
            Err(DspError::Io { .. })
        ));
    }
}
