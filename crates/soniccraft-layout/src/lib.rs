//! SonicCraft layout: sample-accurate timeline geometry (scheduling, the speech-overlap policy, clip lengths,
//! fades, content end and total length) and the ASCII plan diagram.
//!
//! [`layout`] is a pure, deterministic function of a lowered [`soniccraft_core::Program`] and a duration
//! table (one [`NodeDuration`] per node). The engine calls it twice: during dry runs with estimated or cached
//! cue durations, and during renders with exact ones. Everything is computed in integer frames at the
//! output sample rate, `frames(ms) = round(ms × sample_rate / 1000)` (DESIGN.md §3).
//!
//! What the layout decides (DESIGN.md §3):
//! - **Sequential semantics**: a per-track cursor starts at the track start; pauses advance it, implicit
//!   items start at it, and after every item it moves to the item's end (for clips: minus the next clip's
//!   `crossfade_ms`).
//! - **Anchors**: `after_cue` / `with_cue` / `before_cue` on cues, clips and tracks, with signed offsets.
//!   Negative starts are clamped to 0 (`negative_start`). A track anchor resolves to the start of the
//!   track's first declared item or the end of its last declared item (`track_anchor_out_of_order` when
//!   another item starts earlier or ends later). The start and the end of a node are separate scheduling
//!   events, so a bed's `end_relative` may wait for items that are anchored to its start.
//! - **Scheduling order**: among ready nodes, explicit starts first, then the smallest tentative start, then
//!   declaration order.
//! - **Overlap policy** (`timing.speech_overlap`): `push` moves implicit cues past already scheduled speech to
//!   `end(other) + gap` (`cue_pushed`), re-running up to [`MAX_SCHEDULING_PASSES`] times with explicit-cue
//!   intervals pre-seeded (an explicit cue never pushes an implicit cue it depends on; the best pass is kept);
//!   `warn`/`error` report overlaps (`speech_overlap`); `allow` reports nothing. Pushes cost a few logarithmic
//!   queries, whatever the number of simultaneously colliding cues.
//! - **Clip lengths**: natural, `duration_ms`, `end_at_ms`/`end_relative`, loops and open-ended beds
//!   (`clip_shorter_than_requested`, `invalid_value`).
//! - **Fades**: crossfades give both clips equal-power fades of the crossfade length (`max(own, crossfade)`),
//!   fades that do not fit are scaled proportionally (`fades_scaled`). Only a clip that follows the previous
//!   clip of its track sequentially crossfades (`field_not_applicable` otherwise).
//! - **Timeline end**: `content_end`, the tail (`output.tail_ms`, else the longest fade-out of open-ended
//!   clips), `output.duration_ms` (`content_truncated`), open-ended clips ending at the total
//!   (`no_fade_out_on_bed`); a total of 0 (`invalid_value`) or above [`soniccraft_core::MAX_TIME_MS`]
//!   (`timeline_too_long`) is an error.
//!
//! Every diagnostic says whether it depends on estimated durations ([`Layout::estimate_dependent`]), so a dry
//! run can tell what exact durations might change and a render can stop on the rest before synthesizing.
//!
//! The layout is defensive: it never panics or loops on malformed IR. Dependency cycles (normally rejected by
//! lowering) are reported as `dependency_cycle` and broken by placing the items involved sequentially.
//!
//! The [`diagram`] module renders a [`Layout`] as the ASCII plan diagram of the engine's `PlanReport`.
//! With the `test-support` cargo feature, `test_support` provides a builder for IR programs and duration
//! tables.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod diagram;
mod schedule;
#[cfg(feature = "test-support")]
pub mod test_support;
mod text;
mod types;

use soniccraft_core::Program;

pub use diagram::{DiagramOptions, DuckedTrack, render_diagram};
pub use types::{
    DurationSource, Interval, Layout, NodeDuration, Placement, PlacementKind, TrackSpan,
    merge_intervals,
};

/// Maximum number of scheduling passes used by the `push` overlap policy (DESIGN.md §3.4).
pub const MAX_SCHEDULING_PASSES: u32 = schedule::MAX_PASSES;

/// Lay out a lowered program.
///
/// `durations` has one entry per node (index = [`soniccraft_core::ir::NodeId`]): cue audio frames after
/// silence trimming, the *whole* source length of clips (the layout subtracts `trim_start_ms`), and anything
/// for pauses (their length comes from the IR). Missing entries count as zero-length estimates; extra entries
/// are ignored.
///
/// Never fails and never panics: every problem is a diagnostic in [`Layout::diagnostics`], with its
/// estimate dependence in [`Layout::estimate_dependent`]. The result is deterministic: the same inputs always
/// give an identical layout, diagnostics included.
#[must_use]
pub fn layout(program: &Program, durations: &[NodeDuration]) -> Layout {
    schedule::run(program, durations, schedule::Settings::default())
}
