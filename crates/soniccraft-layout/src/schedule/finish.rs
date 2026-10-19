//! Phase 6: everything that needs the final placement of all nodes: overlap diagnostics, content end, tail,
//! total, open-ended lengths, fades, truncation, track spans and track-anchor checks (DESIGN.md §2.13,
//! §3.3–§3.5).

use std::ops::ControlFlow;

use soniccraft_core::Diagnostic;
use soniccraft_core::MAX_TIME_MS;
use soniccraft_core::diagnostics::{codes, path};
use soniccraft_core::ir::{AnchorMode, ClipIr, FadeCurve, NodeId, NodeKind, OverlapPolicy};

use super::facts::Kind;
use super::passes::{Est, Pass, Scheduler, Slot};
use super::rules::{Anchor, Length, Reach, Start, Target};
use super::{Diags, MAX_REPEATED, TRUNCATION_LIST_MAX, mode_name};
use crate::text::{ceil_ms, count, floor_ms, human_length, ms_num, secs, span};
use crate::types::{DurationSource, Layout, Placement, PlacementKind, TrackSpan};

/// An anchor on a track, for `track_anchor_out_of_order`.
struct TrackAnchor<'a> {
    /// Who is anchored, for messages: "`cue`", "the end of `clip`", "track `t`".
    owner: &'a str,
    owner_id: &'a str,
    /// Path of the `start_relative`/`end_relative` field.
    field: String,
    /// Which edge of the owner the anchor places: "start" or "end".
    edge: &'a str,
}

/// The total length of the timeline and whether it depends on estimates.
#[derive(Debug, Clone, Copy)]
struct Total {
    frames: i64,
    est: bool,
}

impl Scheduler<'_> {
    pub(super) fn finish(&self, pass: Pass, passes: u32, mut diags: Diags) -> Layout {
        let sr = self.sample_rate();
        let n = self.node_count();
        let slots: Vec<Slot> = pass
            .slots
            .iter()
            .map(|s| {
                s.unwrap_or(Slot {
                    start: 0,
                    len: Some(0),
                    pushed_from: None,
                    from_source: false,
                })
            })
            .collect();
        let est = &pass.est;

        if self.policy != OverlapPolicy::Allow {
            self.report_overlaps(&pass, &slots, &mut diags);
        }

        // Content end: the latest end of a finite node. It depends on estimates when any finite end does (a
        // longer estimated cue could end later); `certain_end` is what it is at least whatever estimates do.
        let (mut content_end, mut certain_end, mut content_est) = (0_i64, 0_i64, false);
        let mut last_item: Option<NodeId> = None;
        for v in 0..n {
            let Some(end) = slots[v].end() else { continue };
            if end > content_end {
                content_end = end;
                last_item = Some(v);
            }
            if est[v].end {
                content_est = true;
            } else {
                certain_end = certain_end.max(end);
            }
        }
        let open: Vec<NodeId> = (0..n).filter(|&v| slots[v].len.is_none()).collect();
        let output = &self.program.output;
        let tail = match output.tail_ms {
            Some(ms) => self.facts.frames(ms).max(0),
            None => open
                .iter()
                .map(|&v| self.fade_out_request(v))
                .max()
                .unwrap_or(0),
        };
        let mut total = match output.duration_ms {
            Some(ms) => Total {
                frames: self.facts.frames(ms).max(0),
                est: false,
            },
            None => Total {
                frames: content_end.saturating_add(tail),
                est: content_est,
            },
        };
        // A zero sample rate (malformed IR) has no time scale to measure the limit in.
        let limit = self.facts.frames(MAX_TIME_MS);
        if sr > 0 && total.frames > limit {
            let certain = output.duration_ms.is_some() || certain_end.saturating_add(tail) > limit;
            let diagnostic = self.too_long(total.frames, content_end, tail, last_item, &open);
            diags.push_if(diagnostic, !certain);
            total.frames = limit;
        }

        let placements: Vec<Placement> = (0..n)
            .map(|v| self.placement(v, &slots[v], est[v], total, &mut diags))
            .collect();

        if total.frames == 0 {
            diags.push_if(self.empty_timeline(&open), total.est);
        }
        for &v in &open {
            let start = placements[v].start;
            // A clip that starts at or after the end never plays, so it cannot stop abruptly: the
            // `content_truncated` warning below (or the empty-timeline error) covers it.
            if self.fade_out_request(v) == 0 && start < total.frames {
                diags.push(
                    Diagnostic::info(
                        codes::NO_FADE_OUT_ON_BED,
                        format!(
                            "open-ended clip `{}` loops until the timeline end but has no fade-out, so it stops \
                             abruptly at {} s",
                            self.names.id(v),
                            secs(total.frames, sr)
                        ),
                    )
                    .with_path(self.names.path(v))
                    .with_hint(format!(
                        "add `fade_out_ms: 2000` to `{}`; without `output.tail_ms` the timeline then gets a tail of \
                         that length so the fade happens after the last cue",
                        self.names.id(v)
                    ))
                    .with_related([self.names.id(v)]),
                );
            }
            // In an empty timeline the error above says it all.
            if start >= total.frames && total.frames > 0 {
                diags.push_if(
                    Diagnostic::warning(
                        codes::CONTENT_TRUNCATED,
                        format!(
                            "open-ended clip `{}` starts at {} s, at or after the timeline end ({} s), so it is silent",
                            self.names.id(v),
                            secs(start, sr),
                            secs(total.frames, sr)
                        ),
                    )
                    .with_path(self.names.path(v))
                    .with_hint(
                        "start the clip earlier, or extend the timeline with `output.tail_ms` or `output.duration_ms`",
                    )
                    .with_related([self.names.id(v)]),
                    est[v].start || total.est,
                );
            }
        }

        if output.duration_ms.is_some() {
            let cut: Vec<&Placement> = placements
                .iter()
                .filter(|p| {
                    p.kind != PlacementKind::Pause && !p.open_ended && p.end() > total.frames
                })
                .collect();
            if !cut.is_empty() {
                // Certain as soon as one cut item's end is.
                let estimated = cut.iter().all(|p| est[p.node].end);
                diags.push_if(
                    self.truncation(&cut, content_end, total.frames, tail),
                    estimated,
                );
            }
        }

        let track_spans = self
            .facts
            .members
            .iter()
            .enumerate()
            .map(|(t, list)| match (list.first(), list.last()) {
                (Some(&first), Some(&last)) => TrackSpan {
                    track: t,
                    start: placements[first].start,
                    end: placements[last].end(),
                    open_ended: placements[last].open_ended,
                },
                _ => {
                    let at = self.empty_track_start(t);
                    TrackSpan {
                        track: t,
                        start: at,
                        end: at,
                        open_ended: false,
                    }
                }
            })
            .collect();
        self.track_anchor_checks(&placements, est, total, &mut diags);

        let (diagnostics, estimate_dependent) = diags.into_parts();
        Layout {
            sample_rate: sr,
            total_frames: total.frames.unsigned_abs(),
            content_end_frames: content_end.unsigned_abs(),
            tail_frames: tail.unsigned_abs(),
            passes,
            placements,
            track_spans,
            diagnostics,
            estimate_dependent,
        }
    }

    /// `speech_overlap` for every remaining overlap (at most [`MAX_REPEATED`] listed one by one).
    fn report_overlaps(&self, pass: &Pass, slots: &[Slot], diags: &mut Diags) {
        let mut listed = 0_usize;
        let mut omitted = 0_usize;
        let mut omitted_certain = false;
        let mut reach = Reach::new(self.graph.deps.len());
        self.visit_overlaps(&pass.slots, |a, b, overlap| {
            let estimated = pass.interval_est(a) || pass.interval_est(b);
            if listed < MAX_REPEATED {
                let diagnostic = self.overlap_diagnostic(a, b, overlap, slots, pass, &mut reach);
                diags.push_if(diagnostic, estimated);
                listed += 1;
            } else {
                omitted += 1;
                omitted_certain |= !estimated;
            }
            ControlFlow::Continue(())
        });
        if omitted > 0 {
            let message = format!("{omitted} more speech overlaps are not listed individually");
            let summary = if self.policy == OverlapPolicy::Error {
                Diagnostic::error(codes::SPEECH_OVERLAP, message)
            } else {
                Diagnostic::warning(codes::SPEECH_OVERLAP, message)
            };
            diags.push_if(
                summary.with_hint(
                    "fix the overlaps listed above (or set `allow_overlap: true` for intentional crosstalk) and plan \
                     again to see the rest",
                ),
                !omitted_certain,
            );
        }
    }

    /// `max(fade_out_ms, next clip's crossfade)` before scaling (clips; 0 otherwise).
    fn fade_out_request(&self, v: NodeId) -> i64 {
        match &self.program.nodes[v].kind {
            NodeKind::Clip(clip) => self
                .facts
                .frames(clip.fade_out_ms)
                .max(0)
                .max(self.facts.nodes[v].crossfade_out),
            _ => 0,
        }
    }

    fn placement(
        &self,
        v: NodeId,
        slot: &Slot,
        est: Est,
        total: Total,
        diags: &mut Diags,
    ) -> Placement {
        let node = &self.program.nodes[v];
        let fact = &self.facts.nodes[v];
        let open_ended = slot.len.is_none();
        let len = slot
            .len
            .unwrap_or_else(|| total.frames.saturating_sub(slot.start))
            .max(0);
        let len_est = if open_ended {
            total.est || est.start
        } else {
            est.len
        };
        let (kind, duration_source) = match fact.kind {
            Kind::Cue => (PlacementKind::Cue, fact.table_source),
            Kind::Pause => (PlacementKind::Pause, DurationSource::Fixed),
            Kind::Clip if slot.from_source && !open_ended => {
                (PlacementKind::Clip, fact.table_source)
            }
            Kind::Clip => (PlacementKind::Clip, DurationSource::Fixed),
        };
        let mut placement = Placement {
            node: v,
            track: fact.track.unwrap_or(node.track),
            kind,
            start: slot.start,
            length: len.unsigned_abs(),
            open_ended,
            looped: fact.looped,
            pushed_from: slot.pushed_from,
            duration_source,
            trim_start: fact.trim_start.unsigned_abs(),
            fade_in: 0,
            fade_out: 0,
            fade_in_curve: FadeCurve::EqualPower,
            fade_out_curve: FadeCurve::EqualPower,
            loop_crossfade: 0,
        };
        if let NodeKind::Clip(clip) = &node.kind {
            self.apply_fades(v, clip, len, len_est, &mut placement, diags);
        }
        placement
    }

    /// Effective fades: `max(own, crossfade)` on each side (equal power when the crossfade dominates), scaled
    /// proportionally when they do not fit the clip; loop crossfades limited to half the looped segment.
    fn apply_fades(
        &self,
        v: NodeId,
        clip: &ClipIr,
        len: i64,
        len_est: bool,
        p: &mut Placement,
        diags: &mut Diags,
    ) {
        let sr = self.sample_rate();
        let fact = &self.facts.nodes[v];
        let own_in = self.facts.frames(clip.fade_in_ms).max(0);
        let own_out = self.facts.frames(clip.fade_out_ms).max(0);
        let fade_in = own_in.max(fact.crossfade_in);
        let fade_out = own_out.max(fact.crossfade_out);
        let curve = |crossfade: i64, own: i64| {
            if crossfade > 0 && crossfade >= own {
                FadeCurve::EqualPower
            } else {
                clip.fade_curve
            }
        };
        p.fade_in_curve = curve(fact.crossfade_in, own_in);
        p.fade_out_curve = curve(fact.crossfade_out, own_out);
        let (scaled_in, scaled_out) = scale_fades(fade_in, fade_out, len);
        if (scaled_in, scaled_out) != (fade_in, fade_out) && len > 0 {
            diags.push_if(
                Diagnostic::info(
                    codes::FADES_SCALED,
                    format!(
                        "fades of clip `{}` (in {} s + out {} s, crossfades included) are longer than the clip ({} s); \
                         both were scaled proportionally to {} s + {} s",
                        self.names.id(v),
                        secs(fade_in, sr),
                        secs(fade_out, sr),
                        secs(len, sr),
                        secs(scaled_in, sr),
                        secs(scaled_out, sr)
                    ),
                )
                .with_path(self.names.path(v))
                .with_hint(format!(
                    "shorten `fade_in_ms`/`fade_out_ms`/`crossfade_ms` of `{}` to {} ms in total, or lengthen the clip",
                    self.names.id(v),
                    floor_ms(len, sr)
                ))
                .with_related([self.names.id(v)]),
                len_est,
            );
        }
        p.fade_in = scaled_in.unsigned_abs();
        p.fade_out = scaled_out.unsigned_abs();

        if fact.looped {
            let requested = self.facts.frames(clip.loop_crossfade_ms).max(0);
            let limit = if fact.source_known {
                fact.len / 2
            } else {
                requested
            };
            if requested > limit {
                diags.push(
                    Diagnostic::info(
                        codes::FADES_SCALED,
                        format!(
                            "`loop_crossfade_ms` of clip `{}` ({} s) exceeds half of the looped segment ({} s); it \
                             was reduced to {} s",
                            self.names.id(v),
                            secs(requested, sr),
                            secs(fact.len, sr),
                            secs(limit, sr)
                        ),
                    )
                    .with_path(self.names.field(v, "loop_crossfade_ms"))
                    .with_hint(format!("use a `loop_crossfade_ms` of at most {}", floor_ms(limit, sr)))
                    .with_related([self.names.id(v)]),
                );
            }
            p.loop_crossfade = requested.min(limit).unsigned_abs();
        }
    }

    /// `speech_overlap`: both cues with their tracks and intervals, the overlap length, and a mechanical fix
    /// that removes the overlap when applied.
    fn overlap_diagnostic(
        &self,
        a: NodeId,
        b: NodeId,
        overlap: i64,
        slots: &[Slot],
        pass: &Pass,
        reach: &mut Reach,
    ) -> Diagnostic {
        let sr = self.sample_rate();
        let names = self.names;
        let (ia, ib) = (slots[a].interval(), slots[b].interval());
        let message = format!(
            "speech cue `{}` (track `{}`, {}) overlaps `{}` (track `{}`, {}) by {} s",
            names.id(b),
            names.track_of(b),
            span(ib.start, ib.end, sr),
            names.id(a),
            names.track_of(a),
            span(ia.start, ia.end, sr),
            secs(overlap, sr)
        );
        let later = &self.facts.nodes[b];
        // Anchoring the later cue after the earlier one is the natural fix, unless the earlier cue depends on
        // the later one (that would create a cycle): then end the earlier cue before the later one. When the
        // earlier cue is too long to fit before the later one's start, it would be clamped at 0 and still
        // overlap, so the later cue has to move too.
        let (fix_on, fix) = if self.depends_in(pass, reach, a, b) {
            let mut fix = format!(
                "set `start_relative: {{before_cue: {}, offset_ms: {}}}` on `{}`",
                names.id(b),
                ms_num(-later.gap_ms),
                names.id(a)
            );
            let needed = i64::try_from(ia.len())
                .unwrap_or(i64::MAX)
                .saturating_add(later.gap);
            if ib.start < needed {
                let at = ceil_ms(needed, sr);
                fix.push_str(&format!(
                    " and move `{}` to start at {at} ms or later (`start_at_ms: {at}`), so that `{}` ({} s) fits \
                     before it",
                    names.id(b),
                    names.id(a),
                    secs(ia.end - ia.start, sr)
                ));
            }
            (a, fix)
        } else {
            let fix = format!(
                "set `start_relative: {{after_cue: {}, offset_ms: {}}}` on `{}`",
                names.id(a),
                ms_num(later.gap_ms),
                names.id(b)
            );
            (b, fix)
        };
        let mut hint =
            format!("{fix}, or `allow_overlap: true` on either cue for intentional crosstalk");
        let implicit = [a, b].into_iter().find(|&v| self.is_implicit(v));
        let explicit = [a, b].into_iter().find(|&v| !self.is_implicit(v));
        if self.policy != OverlapPolicy::Push && implicit.is_some() {
            hint.push_str(
                "; `timing.speech_overlap: push` moves implicitly placed cues automatically",
            );
        } else if let (OverlapPolicy::Push, Some(i), Some(e)) = (self.policy, implicit, explicit)
            && self.depends_in(pass, reach, e, i)
        {
            hint.push_str(&format!(
                "; `push` leaves `{}` in place because `{}` is anchored to it (directly or through other items) and \
                 would move with it",
                names.id(i),
                names.id(e)
            ));
        }
        let diagnostic = if self.policy == OverlapPolicy::Error {
            Diagnostic::error(codes::SPEECH_OVERLAP, message)
        } else {
            Diagnostic::warning(codes::SPEECH_OVERLAP, message)
        };
        diagnostic
            .with_path(names.path(fix_on))
            .with_hint(hint)
            .with_related([names.id(a), names.id(b)])
    }

    /// `content_truncated` for content cut by `output.duration_ms`.
    fn truncation(
        &self,
        cut: &[&Placement],
        content_end: i64,
        total: i64,
        tail: i64,
    ) -> Diagnostic {
        let sr = self.sample_rate();
        let mut listed: Vec<String> = cut
            .iter()
            .take(TRUNCATION_LIST_MAX)
            .map(|p| {
                format!(
                    "`{}` ({})",
                    self.names.id(p.node),
                    span(p.start, p.end(), sr)
                )
            })
            .collect();
        if cut.len() > TRUNCATION_LIST_MAX {
            listed.push(format!("and {} more", cut.len() - TRUNCATION_LIST_MAX));
        }
        let keep = if self.program.output.tail_ms.is_some() || tail > 0 {
            format!(
                " (or remove it to end {} s after the content)",
                secs(tail, sr)
            )
        } else {
            " (or remove it to end with the content)".to_string()
        };
        Diagnostic::warning(
            codes::CONTENT_TRUNCATED,
            format!(
                "`output.duration_ms` ({} s) cuts content that ends at {} s; {} cut: {}",
                secs(total, sr),
                secs(content_end, sr),
                count(cut.len(), "item is", "items are"),
                listed.join(", ")
            ),
        )
        .with_path("output.duration_ms")
        .with_hint(format!(
            "set `output.duration_ms` to at least {} to keep all content{keep}",
            ceil_ms(content_end, sr)
        ))
        .with_related(cut.iter().map(|p| self.names.id(p.node).to_string()))
    }

    /// `timeline_too_long`: the laid-out length exceeds [`MAX_TIME_MS`]; names what makes it that long.
    fn too_long(
        &self,
        total: i64,
        content_end: i64,
        tail: i64,
        last: Option<NodeId>,
        open: &[NodeId],
    ) -> Diagnostic {
        let sr = self.sample_rate();
        let names = self.names;
        let limit = self.facts.frames(MAX_TIME_MS);
        let max_ms = ms_num(MAX_TIME_MS);
        let length = format!("{} ({} ms)", human_length(total, sr), ceil_ms(total, sr));
        let output = &self.program.output;
        if output.duration_ms.is_some() {
            return Diagnostic::error(
                codes::TIMELINE_TOO_LONG,
                format!(
                    "`output.duration_ms` makes the timeline {length} long; SonicCraft renders at most 24 h"
                ),
            )
            .with_path("output.duration_ms")
            .with_hint(format!("set `output.duration_ms` to at most {max_ms}"));
        }
        if let Some(item) = last
            && content_end > limit
        {
            return Diagnostic::error(
                codes::TIMELINE_TOO_LONG,
                format!(
                    "the timeline would be {length} long: its content ends at {} with `{}` (track `{}`); \
                     SonicCraft renders at most 24 h",
                    human_length(content_end, sr),
                    names.id(item),
                    names.track_of(item)
                ),
            )
            .with_path(names.path(item))
            .with_hint(format!(
                "make everything end within 24 h ({max_ms} ms): move `{}` earlier or shorten the content, and look \
                 for a `start_at_ms`, `offset_ms` or pause written in seconds or minutes but meant in milliseconds",
                names.id(item)
            ))
            .with_related([names.id(item)]);
        }
        // The content fits; the tail pushes the end past the limit.
        let room = ceil_ms(limit.saturating_sub(content_end), sr).max(0);
        let message = format!(
            "the timeline would be {length} long: its content ends at {}, and the tail after it adds {}; SonicCraft \
             renders at most 24 h",
            human_length(content_end, sr),
            human_length(tail, sr)
        );
        let bed = open
            .iter()
            .copied()
            .max_by_key(|&v| (self.fade_out_request(v), std::cmp::Reverse(v)));
        match (output.tail_ms, bed) {
            (None, Some(bed)) => Diagnostic::error(codes::TIMELINE_TOO_LONG, message)
                .with_path(names.field(bed, "fade_out_ms"))
                .with_hint(format!(
                    "the tail is the `fade_out_ms` of the open-ended clip `{}`: lower it to at most {room}, or set \
                     `output.tail_ms` to at most {room}",
                    names.id(bed)
                ))
                .with_related([names.id(bed)]),
            _ => Diagnostic::error(codes::TIMELINE_TOO_LONG, message)
                .with_path("output.tail_ms")
                .with_hint(format!("set `output.tail_ms` to at most {room}")),
        }
    }

    /// `invalid_value` for a timeline whose total length is 0 (rendering it is impossible).
    fn empty_timeline(&self, open: &[NodeId]) -> Diagnostic {
        let names = self.names;
        let output = &self.program.output;
        if output.duration_ms.is_some() {
            return Diagnostic::error(
                codes::INVALID_VALUE,
                "the timeline is empty: `output.duration_ms` is shorter than one sample frame",
            )
            .with_path("output.duration_ms")
            .with_hint("set `output.duration_ms` to the length the output should have, e.g. `output.duration_ms: 30000`");
        }
        let hint = "give the looping bed a `fade_out_ms`, or set `output.tail_ms` or `output.duration_ms`, or add \
                    finite content";
        match open.first() {
            Some(&bed) => Diagnostic::error(
                codes::INVALID_VALUE,
                format!(
                    "the timeline is empty: its total length is 0 ms. `{}` loops until the timeline end, which is the \
                     end of the finite content plus the tail, and both are 0 here",
                    names.id(bed)
                ),
            )
            .with_path(names.path(bed))
            .with_hint(hint)
            .with_related(open.iter().map(|&v| names.id(v).to_string())),
            None => Diagnostic::error(
                codes::INVALID_VALUE,
                "the timeline is empty: its total length is 0 ms (none of its cues, pauses and clips has a length)",
            )
            .with_path("output")
            .with_hint(
                "add finite content (a cue, a pause, or a clip with audio), or set `output.tail_ms` or \
                 `output.duration_ms`",
            ),
        }
    }

    /// `track_anchor_out_of_order` (DESIGN.md §2.13): an anchor on a track resolves to the start of the track's
    /// first declared item (`with_cue`, `before_cue`) or the end of its last declared item (`after_cue`); warn
    /// when another item of that track starts earlier, or ends later, than that one.
    fn track_anchor_checks(
        &self,
        placements: &[Placement],
        est: &[Est],
        total: Total,
        diags: &mut Diags,
    ) {
        let names = self.names;
        for v in 0..self.node_count() {
            if let Start::Relative(a) = &self.rules.start[v]
                && !self.is_implicit(v)
            {
                let owner = format!("`{}`", names.id(v));
                let field = names.field(v, "start_relative");
                let anchor = TrackAnchor {
                    owner: &owner,
                    owner_id: names.id(v),
                    field,
                    edge: "start",
                };
                self.check_track_anchor(anchor, a, placements, est, total, diags);
            }
            if let Length::EndRelative(a) = &self.rules.length[v] {
                let owner = format!("the end of `{}`", names.id(v));
                let field = names.field(v, "end_relative");
                let anchor = TrackAnchor {
                    owner: &owner,
                    owner_id: names.id(v),
                    field,
                    edge: "end",
                };
                self.check_track_anchor(anchor, a, placements, est, total, diags);
            }
        }
        for (t, list) in self.facts.members.iter().enumerate() {
            let Start::Relative(a) = &self.rules.track_start[t] else {
                continue;
            };
            // The track start only matters when the first item has no start of its own.
            if list.first().is_some_and(|&first| self.is_implicit(first)) {
                let owner = format!("track `{}`", names.track_id(t));
                let field = names.track_field(t, "start_relative");
                let anchor = TrackAnchor {
                    owner: &owner,
                    owner_id: names.track_id(t),
                    field,
                    edge: "start",
                };
                self.check_track_anchor(anchor, a, placements, est, total, diags);
            }
        }
    }

    fn check_track_anchor(
        &self,
        anchor: TrackAnchor<'_>,
        a: &Anchor,
        placements: &[Placement],
        est: &[Est],
        total: Total,
        diags: &mut Diags,
    ) {
        let TrackAnchor {
            owner,
            owner_id,
            field,
            edge,
        } = anchor;
        let Target::Track(u) = a.target else {
            return;
        };
        let list = &self.facts.members[u];
        let (Some(&first), Some(&last)) = (list.first(), list.last()) else {
            return;
        };
        let sr = self.sample_rate();
        let names = self.names;
        let track = names.track_id(u);
        let end_est = |w: NodeId| {
            if placements[w].open_ended {
                total.est || est[w].start
            } else {
                est[w].end
            }
        };
        let (declared, extreme, message, hint, estimated) = if a.mode == AnchorMode::After {
            if placements[last].open_ended {
                // `anchor_open_ended` was reported for this anchor.
                return;
            }
            // The latest end; ties go to the first declared.
            let latest = list
                .iter()
                .copied()
                .reduce(|x, y| {
                    if placements[y].end() > placements[x].end() {
                        y
                    } else {
                        x
                    }
                })
                .unwrap_or(last);
            if placements[latest].end() <= placements[last].end() {
                return;
            }
            let when = if placements[latest].open_ended {
                format!(
                    "plays until the timeline end ({} s)",
                    secs(total.frames, sr)
                )
            } else {
                format!("ends later, at {} s", secs(placements[latest].end(), sr))
            };
            let message = format!(
                "{owner} is anchored with `after_cue: {track}`, which resolves to the end of `{}` ({} s), the last \
                 item declared on track `{track}`; but `{}` on that track {when}",
                names.id(last),
                secs(placements[last].end(), sr),
                names.id(latest)
            );
            let reorder = format!(
                "reorder the items of track `{track}` so that the one that ends last is declared last"
            );
            let hint = if names.is_pause(latest) {
                format!("{reorder}, or anchor to the item that should come first")
            } else {
                format!(
                    "to {edge} after `{}`, anchor to it instead (`after_cue: {}`), or {reorder}",
                    names.id(latest),
                    names.id(latest)
                )
            };
            (
                last,
                latest,
                message,
                hint,
                end_est(last) || end_est(latest),
            )
        } else {
            // The earliest start; ties go to the first declared.
            let earliest = list
                .iter()
                .copied()
                .reduce(|x, y| {
                    if placements[y].start < placements[x].start {
                        y
                    } else {
                        x
                    }
                })
                .unwrap_or(first);
            if placements[earliest].start >= placements[first].start {
                return;
            }
            let key = mode_name(a.mode);
            let message = format!(
                "{owner} is anchored with `{key}: {track}`, which resolves to the start of `{}` ({} s), the first \
                 item declared on track `{track}`; but `{}` on that track starts earlier, at {} s",
                names.id(first),
                secs(placements[first].start, sr),
                names.id(earliest),
                secs(placements[earliest].start, sr)
            );
            let reorder = format!(
                "reorder the items of track `{track}` so that the one that starts first is declared first"
            );
            let hint = if names.is_pause(earliest) {
                format!("{reorder}, or anchor to the item that should come first")
            } else {
                format!(
                    "to anchor to `{}`, use `{key}: {}` instead, or {reorder}",
                    names.id(earliest),
                    names.id(earliest)
                )
            };
            let estimated = est[first].start || est[earliest].start;
            (first, earliest, message, hint, estimated)
        };
        diags.push_if(
            Diagnostic::warning(codes::TRACK_ANCHOR_OUT_OF_ORDER, message)
                .with_path(path::field(&field, mode_name(a.mode)))
                .with_hint(hint)
                .with_related([
                    owner_id.to_string(),
                    track.to_string(),
                    names.id(declared).to_string(),
                    names.id(extreme).to_string(),
                ]),
            estimated,
        );
    }
}

/// Scale `(fade_in, fade_out)` proportionally so they fit in `len` (unchanged when they already fit). The
/// scaled fade-in is rounded to the nearest frame and the fade-out takes the rest, so they sum to `len`.
fn scale_fades(fade_in: i64, fade_out: i64, len: i64) -> (i64, i64) {
    let sum = fade_in.saturating_add(fade_out);
    if sum <= len || sum <= 0 {
        return (fade_in, fade_out);
    }
    let len = len.max(0);
    let (fi, l, s) = (i128::from(fade_in), i128::from(len), i128::from(sum));
    let scaled_in = i64::try_from((2 * fi * l + s) / (2 * s))
        .unwrap_or(len)
        .min(len);
    (scaled_in, len - scaled_in)
}

#[cfg(test)]
mod tests {
    use super::scale_fades;

    #[test]
    fn fade_scaling_is_proportional_and_exact() {
        assert_eq!(scale_fades(100, 300, 1000), (100, 300));
        assert_eq!(scale_fades(2000, 3000, 4000), (1600, 2400));
        assert_eq!(scale_fades(1, 1, 1), (1, 0));
        assert_eq!(scale_fades(500, 0, 200), (200, 0));
        assert_eq!(scale_fades(500, 500, 0), (0, 0));
        assert_eq!(scale_fades(0, 0, 0), (0, 0));
        let (a, b) = scale_fades(333, 667, 500);
        assert_eq!(a + b, 500);
        assert_eq!((a, b), (167, 333));
    }
}
