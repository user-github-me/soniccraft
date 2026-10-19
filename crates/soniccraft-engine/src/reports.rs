//! Report pieces shared by plans and renders: items, tracks and ducking from a program and its layout, plus
//! the ducking parameters both use.

use std::ops::Range;

use soniccraft_core::ir::{DuckingConfig, NodeKind, Program, TrackIdx};
use soniccraft_dsp::duck::DuckParams;
use soniccraft_layout::{Interval, Layout, PlacementKind};

use crate::report::{DuckingReport, DurationSource, ItemKind, ItemReport, Region, TrackReport};

/// Gains at or below this many dB are reported as this value (a fader at 0 is −∞ dB, which JSON cannot
/// represent).
pub(crate) const SILENT_GAIN_DB: f64 = -120.0;

/// The report's duration source for a layout duration source.
pub(crate) fn duration_source(source: soniccraft_layout::DurationSource) -> DurationSource {
    use soniccraft_layout::DurationSource as L;
    match source {
        L::Exact => DurationSource::Exact,
        L::Cache => DurationSource::Cache,
        L::Estimate => DurationSource::Estimate,
        L::Source => DurationSource::Source,
        L::Fixed => DurationSource::Fixed,
    }
}

/// Every placed node, sorted by start (ties: declaration order).
pub(crate) fn items(program: &Program, layout: &Layout) -> Vec<ItemReport> {
    let mut placed: Vec<(i64, usize, ItemReport)> = Vec::with_capacity(layout.placements.len());
    for placement in &layout.placements {
        let Some(node) = program.nodes.get(placement.node) else {
            continue;
        };
        let track_id = program
            .tracks
            .get(placement.track)
            .map(|t| t.id.clone())
            .unwrap_or_default();
        let start_ms = layout.ms(placement.start);
        let end_ms = layout.ms(placement.end());
        let mut item = ItemReport {
            id: node.id.clone(),
            kind: match placement.kind {
                PlacementKind::Cue => ItemKind::Cue,
                PlacementKind::Pause => ItemKind::Pause,
                PlacementKind::Clip => ItemKind::Clip,
            },
            track_id,
            start_ms,
            end_ms,
            duration_ms: end_ms - start_ms,
            duration_source: duration_source(placement.duration_source),
            speaker: None,
            provider: None,
            voice_id: None,
            text: None,
            source: None,
            pushed_from_ms: layout.pushed_from_ms(placement.node),
            open_ended: placement.open_ended,
            looped: placement.looped,
        };
        match &node.kind {
            NodeKind::Cue(cue) => {
                item.speaker = cue.speaker.clone();
                item.provider = Some(cue.voice.provider.clone());
                item.voice_id = cue.voice.voice_id.clone();
                item.text = Some(cue.text.clone());
            }
            NodeKind::Clip(clip) => item.source = Some(clip.source.clone()),
            NodeKind::Pause { .. } => {}
        }
        placed.push((placement.start, placement.node, item));
    }
    placed.sort_by_key(|(start, node, _)| (*start, *node));
    placed.into_iter().map(|(_, _, item)| item).collect()
}

/// Every track, in declaration order.
pub(crate) fn tracks(program: &Program) -> Vec<TrackReport> {
    program
        .tracks
        .iter()
        .map(|t| TrackReport {
            id: t.id.clone(),
            kind: t.kind.as_str().to_string(),
            gain_db: gain_db(t.gain),
            pan: t.pan,
            mute: t.mute,
            ducked_by: t
                .ducking
                .as_ref()
                .map(|d| {
                    d.triggers
                        .iter()
                        .filter_map(|&i| program.tracks.get(i).map(|t| t.id.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// A linear gain in dB, rounded to 0.01 dB, never below [`SILENT_GAIN_DB`].
fn gain_db(gain: f64) -> f64 {
    if !gain.is_finite() || gain <= 0.0 {
        return SILENT_GAIN_DB;
    }
    let db = (20.0 * gain.log10()).max(SILENT_GAIN_DB);
    let rounded = (db * 100.0).round() / 100.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

/// Ducking parameters of a track's `ducking` block.
pub(crate) fn duck_params(config: &DuckingConfig) -> DuckParams {
    DuckParams {
        reduction_db: config.reduction_db,
        attack_ms: config.attack_ms,
        release_ms: config.release_ms,
        hold_ms: config.hold_ms,
        lookahead_ms: config.lookahead_ms,
        threshold_db: config.threshold_db,
    }
}

/// The trigger tracks that actually drive a duck: muted triggers produce no sound and are excluded
/// (`muted_trigger`).
pub(crate) fn active_triggers(program: &Program, config: &DuckingConfig) -> Vec<TrackIdx> {
    config
        .triggers
        .iter()
        .copied()
        .filter(|&t| program.tracks.get(t).is_some_and(|t| !t.mute))
        .collect()
}

/// Frame intervals of the audible items (cues and clips) of `triggers`, for cue-mode ducking.
pub(crate) fn trigger_intervals(layout: &Layout, triggers: &[TrackIdx]) -> Vec<Range<usize>> {
    triggers
        .iter()
        .flat_map(|&t| layout.track_intervals(t))
        .map(|iv| clamp_frame(iv.start)..clamp_frame(iv.end))
        .filter(|r| !r.is_empty())
        .collect()
}

fn clamp_frame(frame: i64) -> usize {
    usize::try_from(frame.max(0)).unwrap_or(usize::MAX)
}

/// The report of one ducked track.
pub(crate) fn ducking_report(
    program: &Program,
    layout: &Layout,
    track: TrackIdx,
    regions: &[Range<usize>],
) -> Option<DuckingReport> {
    let t = program.tracks.get(track)?;
    let config = t.ducking.as_ref()?;
    Some(DuckingReport {
        track_id: t.id.clone(),
        mode: config.mode.as_str().to_string(),
        reduction_db: config.reduction_db.abs(),
        triggers: active_triggers(program, config)
            .into_iter()
            .filter_map(|i| program.tracks.get(i).map(|t| t.id.clone()))
            .collect(),
        regions: regions
            .iter()
            .map(|r| Region {
                start_ms: layout.ms(frame_i64(r.start)),
                end_ms: layout.ms(frame_i64(r.end)),
            })
            .collect(),
    })
}

fn frame_i64(frame: usize) -> i64 {
    i64::try_from(frame).unwrap_or(i64::MAX)
}

/// Frame ranges as layout intervals (for the plan diagram).
pub(crate) fn to_intervals(regions: &[Range<usize>]) -> Vec<Interval> {
    regions
        .iter()
        .map(|r| Interval::new(frame_i64(r.start), frame_i64(r.end)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_in_db() {
        assert_eq!(gain_db(1.0), 0.0);
        assert_eq!(gain_db(0.65), -3.74);
        assert_eq!(gain_db(0.0), SILENT_GAIN_DB);
        assert_eq!(gain_db(f64::NAN), SILENT_GAIN_DB);
        assert_eq!(gain_db(1e-9), SILENT_GAIN_DB);
        assert_eq!(gain_db(2.0), 6.02);
    }
}
