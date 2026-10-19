//! The render memory budget: an estimate of the peak memory a render needs, and the check that keeps a
//! render from starting when it would not fit (`timeline_too_long`).
//!
//! The estimate follows the buffers the render pipeline actually holds at its peak:
//!
//! - the **master**, the only full-length buffer: `channels × frames` f32 samples. The mix renders every
//!   track window by window straight into it (stems are streamed to their files), and mastering, the
//!   codec compensation and the preview waveform work on it in place;
//! - during the mix, the **processed speech** (one mono f32 buffer per cue at the output rate) and the
//!   **decoded clip sources** (each distinct file once, at the output rate);
//! - the **sidechain detector** (one mono f32 sample per frame) exists only before the master is
//!   allocated, so it never adds to the peak;
//! - the **activity flags** of ducked tracks (one byte per millisecond each);
//! - while encoding: the master plus what the encoder keeps (FLAC builds the whole stream in memory,
//!   about 11 bytes per sample; WAV and the ffmpeg formats stream; MP3 keeps its compressed output);
//! - a fixed 64 MB for everything else.
//!
//! Measured peaks agree within a few percent (release build, stereo 48 kHz, GB of 2^30 bytes): a 60-minute
//! bed rendered to WAV peaks at 1.31 GB (estimate 1.35 GB), 20 minutes to 24-bit FLAC at 1.60 GB
//! (estimate 1.67 GB).
//!
//! The budget is 8 GB (of 2^30 bytes) unless `SONICCRAFT_MAX_RENDER_GB` (read through the engine's
//! environment) sets another positive number. A render above it stops before any TTS request (checked
//! with the dry-run lengths) and again after the exact layout; validate and plan warn.

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::{codes, human_duration};
use soniccraft_core::ir::{OutputFormat, Program};
use soniccraft_layout::{Layout, PlacementKind};
use soniccraft_tts::EnvSource;

/// Environment variable that sets the budget, in gigabytes (2^30 bytes).
pub(crate) const BUDGET_ENV: &str = "SONICCRAFT_MAX_RENDER_GB";

/// The default budget in gigabytes.
pub(crate) const DEFAULT_BUDGET_GB: f64 = 8.0;

const GB: f64 = (1u64 << 30) as f64;

/// Bytes of one f32 sample.
const SAMPLE: u64 = 4;

/// Extra bytes per output sample while the FLAC encoder holds the stream (its encoded frames and the
/// output bytes; measured with flacenc 0.5.1: about 9 for 16-bit and 11 for 24-bit stereo).
const FLAC_BYTES_PER_SAMPLE: u64 = 11;

/// What every render holds besides its audio (program, layout, runtime, I/O and window buffers).
const BASELINE: u64 = 64 << 20;

/// What a render may use, from the environment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Budget {
    /// The budget in bytes.
    pub bytes: u64,
    /// The budget in gigabytes, as configured.
    pub gb: f64,
}

impl Budget {
    /// The budget `env` configures, and a warning when `SONICCRAFT_MAX_RENDER_GB` is set but not a
    /// positive number (the default applies then).
    pub(crate) fn from_env(env: &EnvSource) -> (Budget, Option<Diagnostic>) {
        let default = Budget::gigabytes(DEFAULT_BUDGET_GB);
        let Some(raw) = env.get(BUDGET_ENV) else {
            return (default, None);
        };
        match raw.trim().parse::<f64>() {
            Ok(gb) if gb.is_finite() && gb > 0.0 && gb < 1e9 => (Budget::gigabytes(gb), None),
            _ => (
                default,
                Some(
                    Diagnostic::warning(
                        codes::INVALID_VALUE,
                        format!(
                            "{BUDGET_ENV}=`{}` is not a positive number of gigabytes; the default render \
                             memory budget of {DEFAULT_BUDGET_GB} GB applies",
                            raw.trim()
                        ),
                    )
                    .with_hint(format!(
                        "Set {BUDGET_ENV} to a number such as `16` (gigabytes of memory a render may use), or \
                         unset it"
                    )),
                ),
            ),
        }
    }

    fn gigabytes(gb: f64) -> Budget {
        Budget {
            bytes: (gb * GB).min(u64::MAX as f64) as u64,
            gb,
        }
    }
}

/// Peak memory in bytes a render of `program` with `layout` needs (see the module docs).
/// `source_samples` is the sum over distinct clip sources of frames × channels at the output rate.
pub(crate) fn peak_bytes(program: &Program, layout: &Layout, source_samples: u64) -> u64 {
    let frames = layout.total_frames;
    let channels = u64::from(program.output.channels.max(1));
    let master = frames.saturating_mul(channels).saturating_mul(SAMPLE);
    let speech: u64 = layout
        .placements
        .iter()
        .filter(|p| p.kind == PlacementKind::Cue)
        .map(|p| p.length)
        .fold(0u64, u64::saturating_add)
        .saturating_mul(SAMPLE);
    let sources = source_samples.saturating_mul(SAMPLE);
    let ducked = program
        .tracks
        .iter()
        .filter(|t| t.ducking.is_some())
        .count() as u64;
    let blocks = frames.saturating_mul(1000) / u64::from(program.output.sample_rate.max(1));
    let activity = blocks.saturating_mul(ducked);
    let encoder = match program.output.format {
        OutputFormat::Flac => frames
            .saturating_mul(channels)
            .saturating_mul(FLAC_BYTES_PER_SAMPLE),
        // The compressed stream (at most 320 kbps, reserved at one byte per eight frames).
        OutputFormat::Mp3 => frames / 8,
        _ => 0,
    };
    // The mix (master + speech + sources) and the encode (master + encoder) are not simultaneous.
    master
        .saturating_add(speech.saturating_add(sources).max(encoder))
        .saturating_add(activity)
        .saturating_add(BASELINE)
}

/// `timeline_too_long` when `peak` exceeds `budget`: an error for renders, a warning for dry runs (whose
/// lengths are estimates).
pub(crate) fn check(
    program: &Program,
    layout: &Layout,
    peak: u64,
    budget: Budget,
    dry_run: bool,
) -> Option<Diagnostic> {
    if peak <= budget.bytes {
        return None;
    }
    let needed = peak as f64 / GB;
    let output = &program.output;
    let layout_desc = format!(
        "{} of {} {} Hz audio",
        human_duration(layout.total_ms()),
        if output.channels == 1 {
            "mono"
        } else {
            "stereo"
        },
        output.sample_rate
    );
    let message = if dry_run {
        format!(
            "rendering this timeline would need about {needed:.1} GB of memory ({layout_desc}), more than the \
             render budget of {} GB; the render will stop before synthesizing anything",
            fmt_gb(budget.gb)
        )
    } else {
        format!(
            "rendering this timeline needs about {needed:.1} GB of memory ({layout_desc}), more than the render \
             budget of {} GB",
            fmt_gb(budget.gb)
        )
    };
    let raise = (needed * 1.1).ceil().max(budget.gb.ceil() + 1.0);
    let hint = format!(
        "Split the timeline into several shorter timelines (e.g. one per chapter) and render them separately, or \
         raise the budget with {BUDGET_ENV}={raise} in soniccraft's environment if this machine has that much \
         free memory"
    );
    let d = if dry_run {
        Diagnostic::warning(codes::TIMELINE_TOO_LONG, message)
    } else {
        Diagnostic::error(codes::TIMELINE_TOO_LONG, message)
    };
    Some(d.with_path(length_path(program, layout)).with_hint(hint))
}

/// Where the length of the timeline comes from: `output.duration_ms` when set, else the item that ends
/// last.
fn length_path(program: &Program, layout: &Layout) -> String {
    if program.output.duration_ms.is_some() {
        return "output.duration_ms".to_string();
    }
    layout
        .placements
        .iter()
        .filter(|p| !p.open_ended)
        .max_by_key(|p| (p.end(), std::cmp::Reverse(p.node)))
        .and_then(|p| program.nodes.get(p.node))
        .map_or_else(|| "output".to_string(), |n| n.path.clone())
}

/// `8`, `0.5` or `0.001`.
fn fmt_gb(gb: f64) -> String {
    let s = format!("{gb:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_comes_from_the_environment() {
        let (budget, warning) = Budget::from_env(&EnvSource::fixed([("X", "1")]));
        assert_eq!(budget.bytes, 8 << 30);
        assert!(warning.is_none());
        let (budget, warning) = Budget::from_env(&EnvSource::fixed([(BUDGET_ENV, "0.5")]));
        assert_eq!(budget.bytes, 1 << 29);
        assert!(warning.is_none());
        for bad in ["zero", "-1", "0", "inf", "NaN", "8GB"] {
            let (budget, warning) = Budget::from_env(&EnvSource::fixed([(BUDGET_ENV, bad)]));
            assert_eq!(budget.bytes, 8 << 30, "{bad}");
            let warning = warning.unwrap_or_else(|| panic!("{bad}"));
            assert_eq!(warning.code, codes::INVALID_VALUE);
            assert!(warning.hint.as_deref().unwrap().contains(BUDGET_ENV));
        }
        assert_eq!(fmt_gb(8.0), "8");
        assert_eq!(fmt_gb(0.5), "0.5");
        assert_eq!(fmt_gb(0.001), "0.001");
    }
}
