//! Phase 1: static, pass-independent facts about every node, in frames.

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::codes;
use soniccraft_core::ir::{ClipIr, NodeId, NodeKind, Program, StartRule, TrackIdx, ms_to_frames};

use super::rules::{Length, Rules, Start, Target};
use super::{Diags, Names};
use crate::text::{floor_ms, secs};
use crate::types::{DurationSource, NodeDuration};

/// What a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Cue,
    Pause,
    Clip,
}

/// Static facts about one node. All lengths are frames at the output sample rate.
#[derive(Debug, Clone)]
pub(super) struct NodeFact {
    pub(super) kind: Kind,
    /// The track whose member list contains the node (None for nodes that no track lists).
    pub(super) track: Option<TrackIdx>,
    /// Position in that member list.
    pub(super) pos: usize,
    /// Cue: audio length. Pause: pause length. Clip: natural length (source − trim start, ≥ 0).
    pub(super) len: i64,
    /// The duration table's source for this node (pauses: fixed).
    pub(super) table_source: DurationSource,
    /// `len` comes from an estimated duration-table entry (a cue's estimated length, or a clip whose source
    /// length is unknown).
    pub(super) len_est: bool,
    /// Clips: the source length is known (its table entry is not an estimate).
    pub(super) source_known: bool,
    /// Clips: whole source length.
    pub(super) source_frames: i64,
    /// Clips: `trim_start_ms`.
    pub(super) trim_start: i64,
    /// Clips: this clip's `crossfade_ms` when it follows another clip of its track sequentially, else 0.
    pub(super) crossfade_in: i64,
    /// Clips: the next clip's `crossfade_in`, else 0.
    pub(super) crossfade_out: i64,
    /// Cues: push gap (sum of the directly preceding pauses, or `timing.min_gap_ms`).
    pub(super) gap: i64,
    /// Cues: the push gap in milliseconds, for hints.
    pub(super) gap_ms: f64,
    /// Cues: `allow_overlap`.
    pub(super) allow_overlap: bool,
    /// Clips: `loop`.
    pub(super) looped: bool,
}

/// Facts about the whole program.
pub(super) struct Facts {
    pub(super) sample_rate: u32,
    /// Per track: valid node ids in order (out-of-range and repeated entries dropped).
    pub(super) members: Vec<Vec<NodeId>>,
    /// Per node, indexed by [`NodeId`].
    pub(super) nodes: Vec<NodeFact>,
}

impl Facts {
    pub(super) fn new(
        program: &Program,
        durations: &[NodeDuration],
        names: Names<'_>,
        diags: &mut Diags,
    ) -> Self {
        let sample_rate = program.output.sample_rate;
        let n = program.nodes.len();
        let frames = |ms: f64| ms_to_frames(ms, sample_rate);

        let mut members: Vec<Vec<NodeId>> = Vec::with_capacity(program.tracks.len());
        let mut owner: Vec<Option<(TrackIdx, usize)>> = vec![None; n];
        for (t, track) in program.tracks.iter().enumerate() {
            let mut list = Vec::with_capacity(track.nodes.len());
            for &v in &track.nodes {
                if v < n && owner[v].is_none() {
                    owner[v] = Some((t, list.len()));
                    list.push(v);
                }
            }
            members.push(list);
        }

        let mut nodes: Vec<NodeFact> = program
            .nodes
            .iter()
            .enumerate()
            .map(|(v, node)| {
                let entry = durations
                    .get(v)
                    .copied()
                    .unwrap_or(NodeDuration::estimate(0));
                let table_frames = i64::try_from(entry.frames).unwrap_or(i64::MAX);
                let mut fact = NodeFact {
                    kind: Kind::Cue,
                    track: owner[v].map(|(t, _)| t),
                    pos: owner[v].map_or(0, |(_, p)| p),
                    len: 0,
                    table_source: entry.source,
                    len_est: entry.source.is_estimate(),
                    source_known: entry.source != DurationSource::Estimate,
                    source_frames: 0,
                    trim_start: 0,
                    crossfade_in: 0,
                    crossfade_out: 0,
                    gap: 0,
                    gap_ms: 0.0,
                    allow_overlap: false,
                    looped: false,
                };
                match &node.kind {
                    NodeKind::Cue(cue) => {
                        fact.len = table_frames;
                        fact.allow_overlap = cue.allow_overlap;
                    }
                    NodeKind::Pause { duration_ms } => {
                        fact.kind = Kind::Pause;
                        fact.len = frames(*duration_ms).max(0);
                        fact.table_source = DurationSource::Fixed;
                        fact.len_est = false;
                        fact.source_known = true;
                    }
                    NodeKind::Clip(clip) => {
                        fact.kind = Kind::Clip;
                        fact.looped = clip.looped;
                        fact.source_frames = table_frames;
                        fact.trim_start = frames(clip.trim_start_ms).max(0);
                        fact.len = (table_frames - fact.trim_start).max(0);
                        if fact.source_known {
                            check_clip_source(names, v, clip, &fact, sample_rate, diags);
                        }
                    }
                }
                fact
            })
            .collect();

        // Crossfades: a sequential clip's `crossfade_ms` overlaps it with the previous clip of its track
        // (DESIGN.md §2.11, §3.2). A clip with its own start does not follow the previous clip, so there is no
        // overlap to crossfade: the field is ignored with a warning (core does the same for the first clip).
        for list in &members {
            for pair in list.windows(2) {
                let (prev, next) = (pair[0], pair[1]);
                if nodes[prev].kind != Kind::Clip {
                    continue;
                }
                let NodeKind::Clip(clip) = &program.nodes[next].kind else {
                    continue;
                };
                let crossfade = frames(clip.crossfade_ms).max(0);
                if crossfade == 0 {
                    continue;
                }
                let start_field = match program.nodes[next].start {
                    StartRule::Sequential => None,
                    StartRule::Absolute { .. } => Some("start_at_ms"),
                    StartRule::Relative(_) => Some("start_relative"),
                };
                if let Some(field) = start_field {
                    diags.push(
                        Diagnostic::warning(
                            codes::FIELD_NOT_APPLICABLE,
                            format!(
                                "`crossfade_ms` has no effect on clip `{}`: it has its own start (`{field}`), and only \
                                 a clip that follows the previous clip of its track sequentially crossfades with it; \
                                 it was ignored",
                                names.id(next)
                            ),
                        )
                        .with_path(names.field(next, "crossfade_ms"))
                        .with_hint(format!(
                            "remove `crossfade_ms` from `{}`, or remove its `{field}` so that it starts \
                             `crossfade_ms` before the end of `{}`; use `fade_in_ms` to fade it in where it is",
                            names.id(next),
                            names.id(prev)
                        ))
                        .with_related([names.id(next), names.id(prev)]),
                    );
                    continue;
                }
                nodes[next].crossfade_in = crossfade;
                nodes[prev].crossfade_out = crossfade;
            }
        }

        // Push gaps: the pauses directly preceding a cue, or `timing.min_gap_ms` when there are none.
        let min_gap_ms = program.timing.min_gap_ms.max(0.0);
        for list in &members {
            for (pos, &v) in list.iter().enumerate() {
                if nodes[v].kind != Kind::Cue {
                    continue;
                }
                let (mut gap, mut gap_ms, mut pauses) = (0_i64, 0.0_f64, 0_usize);
                for &u in list[..pos].iter().rev() {
                    let NodeKind::Pause { duration_ms } = &program.nodes[u].kind else {
                        break;
                    };
                    gap = gap.saturating_add(nodes[u].len);
                    gap_ms += duration_ms.max(0.0);
                    pauses += 1;
                }
                if pauses == 0 {
                    gap = frames(min_gap_ms).max(0);
                    gap_ms = min_gap_ms;
                }
                nodes[v].gap = gap;
                nodes[v].gap_ms = gap_ms;
            }
        }

        Facts {
            sample_rate,
            members,
            nodes,
        }
    }

    /// `round(ms × sample_rate / 1000)`.
    pub(super) fn frames(&self, ms: f64) -> i64 {
        ms_to_frames(ms, self.sample_rate)
    }

    /// A looping clip without a length: it runs until the timeline end.
    pub(super) fn is_open_ended(&self, rules: &Rules, v: NodeId) -> bool {
        self.nodes[v].kind == Kind::Clip
            && self.nodes[v].looped
            && rules.length[v] == Length::Natural
    }

    /// The node whose end an `after_cue` anchor on `target` reads, if any.
    pub(super) fn end_node(&self, target: Target) -> Option<NodeId> {
        match target {
            Target::Node(w) => Some(w),
            Target::Track(t) => self.members[t].last().copied(),
            Target::Invalid => None,
        }
    }

    /// True when [`Facts::independent_length`] depends on an estimated duration.
    pub(super) fn independent_length_est(&self, rules: &Rules, v: NodeId) -> bool {
        let fact = &self.nodes[v];
        match fact.kind {
            Kind::Cue | Kind::Pause => fact.len_est,
            Kind::Clip => match &rules.length[v] {
                Length::Intrinsic | Length::NaturalFinite | Length::Natural => fact.len_est,
                // A requested length is honoured as is while the source length is unknown, but the source
                // may turn out shorter and cut it (unless the clip loops).
                Length::Duration(_) => fact.len_est && !fact.looped,
                // No independent length (`before_cue` conflicts with these; see `checks`).
                Length::EndAbsolute(_) | Length::EndRelative(_) => false,
            },
        }
    }

    /// The length of `v` when it does not depend on `v`'s start (needed by `before_cue`).
    pub(super) fn independent_length(&self, rules: &Rules, v: NodeId) -> Option<i64> {
        let fact = &self.nodes[v];
        match fact.kind {
            Kind::Cue | Kind::Pause => Some(fact.len),
            Kind::Clip => match &rules.length[v] {
                Length::Intrinsic | Length::NaturalFinite => Some(fact.len),
                Length::Natural => (!fact.looped).then_some(fact.len),
                Length::Duration(d) => Some(if fact.looped || !fact.source_known {
                    *d
                } else {
                    (*d).min(fact.len)
                }),
                Length::EndAbsolute(_) | Length::EndRelative(_) => None,
            },
        }
    }

    /// Pauses always, and every node without an explicit start, follow the track cursor.
    pub(super) fn is_sequential(&self, rules: &Rules, v: NodeId) -> bool {
        self.nodes[v].kind == Kind::Pause || rules.start[v] == Start::Sequential
    }
}

/// Errors about a clip's source range that do not depend on placement.
fn check_clip_source(
    names: Names<'_>,
    v: NodeId,
    clip: &ClipIr,
    fact: &NodeFact,
    sample_rate: u32,
    diags: &mut Diags,
) {
    if fact.trim_start > fact.source_frames {
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!(
                    "`trim_start_ms` of clip `{}` ({} s) is beyond the end of its source `{}` ({} s)",
                    names.id(v),
                    secs(fact.trim_start, sample_rate),
                    clip.source,
                    secs(fact.source_frames, sample_rate),
                ),
            )
            .with_path(names.field(v, "trim_start_ms"))
            .with_hint(format!(
                "use a `trim_start_ms` below {} (the source length in ms)",
                floor_ms(fact.source_frames, sample_rate)
            ))
            .with_related([names.id(v)]),
        );
    } else if fact.looped && fact.len == 0 {
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!(
                    "clip `{}` loops, but its source `{}` has no audio after `trim_start_ms`, so there is nothing \
                     to loop",
                    names.id(v),
                    clip.source
                ),
            )
            .with_path(names.field(v, "loop"))
            .with_hint("lower `trim_start_ms`, use a longer source, or set `loop: false`")
            .with_related([names.id(v)]),
        );
    }
}
