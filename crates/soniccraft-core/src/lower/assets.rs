//! The audio files a timeline refers to (DESIGN.md §2.11), collected from the raw document so the engine can
//! check that they exist even when the document has errors and no [`crate::Program`] is built.

use serde::Serialize;

use crate::model::{Timeline, TrackKindName};
use crate::parse::SpanIndex;

/// An audio file named by a `source` field (the single-clip shorthand of a track, or an entry of `clips`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetRef {
    /// The `source` value as written (surrounding whitespace removed), relative to the base directory.
    pub source: String,
    /// Diagnostic path of the `source` field, e.g. `tracks[1].clips[0].source` or `tracks[0].source`.
    pub path: String,
    /// The clip's id as lowering names it: explicit, automatic (`<track>.clip<n>`), or the track id for the
    /// single-clip shorthand.
    pub id: String,
    /// 1-based line of the value in the source document, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// 1-based column of the value, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// Every non-empty string `source` of the tracks lowering treats as audio tracks (music, sfx, and tracks
/// without a valid `type` that have no `sequence`), in document order: a track's shorthand `source`, then
/// its `clips`. Speech tracks are skipped (a `source` there is `field_not_applicable` and never played).
pub(crate) fn collect(timeline: &Timeline, spans: &SpanIndex) -> Vec<AssetRef> {
    let mut out = Vec::new();
    for (i, track) in timeline.tracks.iter().enumerate() {
        let speech = match track.kind.as_deref().and_then(TrackKindName::parse) {
            Some(kind) => kind == TrackKindName::Speech,
            None => track.sequence.is_some(),
        };
        if speech {
            continue;
        }
        let track_id = track
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("track{}", i + 1));
        let mut push = |source: Option<&str>, path: String, id: String| {
            let Some(source) = source.map(str::trim).filter(|s| !s.is_empty()) else {
                return;
            };
            let position = spans.get(&path).or_else(|| spans.locate(&path));
            out.push(AssetRef {
                source: source.to_string(),
                id,
                line: position.map(|(line, _)| line),
                column: position.map(|(_, column)| column),
                path,
            });
        };
        push(
            track.source.as_deref(),
            format!("tracks[{i}].source"),
            track_id.clone(),
        );
        for (j, clip) in track.clips.iter().flatten().enumerate() {
            let id = clip
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("{track_id}.clip{}", j + 1));
            push(
                clip.source.as_deref(),
                format!("tracks[{i}].clips[{j}].source"),
                id,
            );
        }
    }
    out
}
