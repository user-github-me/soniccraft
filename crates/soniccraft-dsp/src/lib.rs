//! SonicCraft DSP: native audio decoding, resampling, analysis, clip rendering, ducking, loudness
//! measurement, true-peak limiting, mastering and encoding. Everything operates on planar f32
//! [`AudioBuffer`]s and is deterministic: no wall-clock, no unseeded randomness (16-bit dither uses a
//! fixed-seed PRNG), and byte-identical wav/flac/mp3 output for identical input.
//!
//! See docs/DESIGN.md §4 for the exact algorithms. The engine orchestrates; this crate provides the
//! building blocks:
//!
//! | step | module |
//! |---|---|
//! | decode files / TTS bytes (symphonia, tolerant WAV reader, ffmpeg fallback) | [`decode`], [`wav`] |
//! | mix sources wider than stereo down (ITU-R BS.775) | [`downmix`] |
//! | resample with exact length and alignment (§4.1.2) | [`resample`] |
//! | silence trim, speech regions, levels (§4.1.3, §5.4) | [`analysis`] |
//! | source loudness normalization, EBU R128 measurement (§4.2) | [`loudness`] |
//! | clip rendering, fades, loops, pan, mixing, de-click (§4.3, §4.4, §4.6.2) | [`render`] |
//! | ducking detector, hold, lookahead, gain envelope; dry-run regions (§4.5) | [`duck`] |
//! | true-peak limiter (§4.7) and the mastering loop (§4.6.3–4) | [`limiter`], [`master`] |
//! | wav / mp3 / flac / ogg / opus / m4a output (§4.8) | [`encode`] |
//! | measuring a delivered (lossy) file | [`loudness::measure_file`] |
//! | preview waveform (§10.2) | [`waveform`] |
//!
//! # Robustness and resources
//!
//! Decoded audio is always finite (NaN/±∞ become silence and are counted), mono or stereo (wider
//! sources are mixed down with ITU-R BS.775) and at a sample rate within 1–768 kHz (others are
//! rejected); the resampler never panics and needs memory independent of the ratio. Mastering and
//! limiting work in place and stream their measurements, so they need no memory proportional to the
//! mix beyond the mix itself; the long operations take a cancellation callback
//! ([`master::master_with_cancel`], [`limiter::limit_in_place`], [`loudness::measure_with_cancel`]).
//! Temp files are created exclusively under unpredictable names and never through a symlink.
//!
//! # Licensing
//!
//! Native MP3 encoding links LAME (LGPL-3.0) statically through `mp3lame-encoder`. It is behind the
//! default-on cargo feature `mp3`; building with `--no-default-features` removes LAME, and MP3 output
//! then goes through ffmpeg (when available). Binary distributions that include the `mp3` feature
//! must ship the LGPL-3.0 text and allow relinking (satisfied by the public source). See
//! docs/DESIGN.md §11.

#![warn(missing_docs)]

pub mod analysis;
pub mod buffer;
pub mod decode;
pub mod downmix;
pub mod duck;
pub mod encode;
pub mod error;
pub mod ffmpeg;
pub mod limiter;
pub mod loudness;
pub mod master;
pub mod render;
pub mod resample;
pub mod wav;
pub mod waveform;

mod fsutil;
mod mp4;
mod quantize;

pub use buffer::AudioBuffer;
pub use error::DspError;
