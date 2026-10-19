//! Phase 5: scheduling passes. A pass places every node once, in dependency order; among ready nodes, explicit
//! starts go first, then the smallest tentative start, then declaration order (DESIGN.md §3.4).
//!
//! The start and the end of a node are separate events ([`super::rules::Vertex`]). A node is placed when its
//! start's dependencies are placed; its end is resolved at the same time unless it is a clip whose length
//! waits for other items (`end_relative`), whose end is resolved as soon as those are placed. So an item
//! anchored to a clip's start can be placed before that clip's end is known.
//!
//! Every placed time also records whether it depends on estimated durations (see [`Est`]).

use std::collections::{BTreeSet, VecDeque};
use std::ops::ControlFlow;

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::codes;
use soniccraft_core::ir::{AnchorMode, NodeId, OverlapPolicy, Program, TrackIdx};

use super::facts::{Facts, Kind};
use super::obstacles::{Obstacles, Rec, Seed};
use super::rules::{
    Anchor, Graph, Length, Reach, Rules, Start, Target, Vertex, end_of, is_end, node_of, start_of,
};
use super::{Diags, MAX_PASSES, MAX_REPEATED, Names, Settings, mode_name};
use crate::text::{ceil_ms, floor_ms, ms_num, secs, span};
use crate::types::Interval;

/// A placed node during scheduling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Slot {
    pub(super) start: i64,
    /// None for open-ended clips (resolved once the total is known).
    pub(super) len: Option<i64>,
    pub(super) pushed_from: Option<i64>,
    /// The length is the source's natural length (also when a request was cut short by it).
    pub(super) from_source: bool,
}

impl Slot {
    pub(super) fn end(&self) -> Option<i64> {
        self.len.map(|len| self.start.saturating_add(len))
    }

    /// The end, or the start for open-ended clips (whose end is not known during scheduling).
    pub(super) fn end_or_start(&self) -> i64 {
        self.end().unwrap_or(self.start)
    }

    pub(super) fn interval(&self) -> Interval {
        Interval::new(self.start, self.end_or_start())
    }
}

/// Whether a node's placement depends on estimated durations: its start, its length, and so its end may change
/// once exact durations are known.
///
/// A time is estimate-dependent when it is computed from one that is: a cue whose duration-table entry is an
/// estimate, a clip whose source length is unknown, and every start, length and end anchored to, following or
/// cut by those. Under the `push` policy the outcome of a push depends on every speech interval already
/// scheduled, so an implicit cue pushed (or left in place) among estimate-dependent speech is
/// estimate-dependent; and when exact durations could change which overlaps make the policy re-run, every
/// implicit cue is (see [`Scheduler::schedule`]). The flags over-approximate: a time marked independent is
/// exact whatever the estimated cues turn out to be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Est {
    pub(super) start: bool,
    pub(super) len: bool,
    pub(super) end: bool,
}

/// Why a node starts where it starts (for `negative_start` messages).
#[derive(Debug, Clone, Copy)]
enum Cause {
    /// Not a member of any track: starts at 0.
    Orphan,
    /// Sequential: after the given predecessor (minus this clip's crossfade).
    Cursor(NodeId),
    /// First sequential item of the given track: the track start.
    TrackStart(TrackIdx),
    /// `start_at_ms`.
    Absolute,
    /// `start_relative`.
    Relative,
}

#[derive(Debug, Clone, Copy)]
struct StartCalc {
    /// The tentative start: the resolved start clamped at 0.
    start: i64,
    /// The start as resolved, possibly negative.
    raw: i64,
    cause: Cause,
    /// The resolved start depends on estimated durations.
    est: bool,
}

/// What a clip's requested length was (for messages and paths).
#[derive(Debug, Clone, Copy)]
enum LengthField {
    Duration,
    EndAt,
    EndRelative,
}

impl LengthField {
    fn name(self) -> &'static str {
        match self {
            LengthField::Duration => "duration_ms",
            LengthField::EndAt => "end_at_ms",
            LengthField::EndRelative => "end_relative",
        }
    }
}

/// A node's length as computed during scheduling: `(length, from_source, estimate-dependent)`, the length being
/// None for open-ended clips.
type LengthCalc = (Option<i64>, bool, bool);

/// The result of one scheduling pass.
#[derive(Debug)]
pub(super) struct Pass {
    pub(super) slots: Vec<Option<Slot>>,
    pub(super) est: Vec<Est>,
    /// Resolution order of every vertex within the pass (`usize::MAX` while unresolved).
    pub(super) order: Vec<usize>,
    /// Scheduling order of each node's start within the pass.
    pub(super) seq: Vec<usize>,
    end_done: Vec<bool>,
    resolved: usize,
    placed: usize,
    /// Cues moved by the push policy (beyond [`MAX_REPEATED`] they are only counted).
    pushed: usize,
    /// One of the cues moved beyond [`MAX_REPEATED`] was moved for a reason that depends on estimates.
    unlisted_push_est: bool,
    /// Total distance in frames the push policy moved cues by.
    displacement: i128,
    pub(super) diagnostics: Diags,
}

impl Pass {
    fn new(n: usize) -> Self {
        Pass {
            slots: vec![None; n],
            est: vec![Est::default(); n],
            order: vec![usize::MAX; 2 * n],
            seq: vec![usize::MAX; n],
            end_done: vec![false; n],
            resolved: 0,
            placed: 0,
            pushed: 0,
            unlisted_push_est: false,
            displacement: 0,
            diagnostics: Diags::default(),
        }
    }

    fn resolve(&mut self, x: Vertex) {
        self.order[x] = self.resolved;
        self.resolved += 1;
    }

    /// The node's interval depends on estimated durations.
    pub(super) fn interval_est(&self, v: NodeId) -> bool {
        self.est[v].start || self.est[v].end
    }
}

/// How good a pass is: remaining speech overlaps, their total length in frames, and the total push
/// displacement in frames (smaller is better; ties go to the earlier pass).
type Score = (usize, i128, i128);

/// Readiness order: explicit (0) before sequential (1), tentative start, track, position, node.
type ReadyKey = (u8, i64, usize, usize, NodeId);

/// Everything a pass needs; immutable across passes.
pub(super) struct Scheduler<'a> {
    pub(super) names: Names<'a>,
    pub(super) program: &'a Program,
    pub(super) facts: &'a Facts,
    pub(super) rules: &'a Rules,
    pub(super) graph: Graph,
    pub(super) policy: OverlapPolicy,
    /// `timing.overlap_tolerance_ms` in frames.
    pub(super) tolerance: i64,
    pub(super) settings: Settings,
}

impl Scheduler<'_> {
    pub(super) fn sample_rate(&self) -> u32 {
        self.facts.sample_rate
    }

    pub(super) fn node_count(&self) -> usize {
        self.facts.nodes.len()
    }

    /// Follows the track cursor (pauses always; cues and clips without an explicit start).
    pub(super) fn is_implicit(&self, v: NodeId) -> bool {
        self.facts.is_sequential(self.rules, v)
    }

    /// A speech cue that takes part in overlap handling (not `allow_overlap`).
    pub(super) fn is_speech_obstacle(&self, v: NodeId) -> bool {
        let fact = &self.facts.nodes[v];
        fact.kind == Kind::Cue && !fact.allow_overlap
    }

    /// True when the start of `from` depends on the start of `to` (so `from` moves whenever `to` does), in a
    /// finished pass.
    pub(super) fn depends_in(
        &self,
        pass: &Pass,
        reach: &mut Reach,
        from: NodeId,
        to: NodeId,
    ) -> bool {
        let floor = pass.order[start_of(to)];
        reach.reset();
        reach.depends(&self.graph, start_of(from), start_of(to), |w| {
            pass.order[w] >= floor
        })
    }

    /// Run the passes (DESIGN.md §3.4): one, or under the `push` policy up to [`MAX_PASSES`], each re-run
    /// seeded with the previous pass's explicit-cue intervals while an implicit cue collides with an explicit
    /// cue scheduled after it (one that does not depend on it). Returns the best pass (fewest and shortest
    /// remaining overlaps, then the least push displacement, then the earliest) and the number of passes run.
    pub(super) fn schedule(&self) -> (Pass, u32) {
        let mut seeds: Vec<Seed> = Vec::new();
        let mut latest = self.run_pass(&seeds, false);
        let mut passes = 1;
        if self.policy != OverlapPolicy::Push {
            return (latest, passes);
        }
        let mut best: Option<(Score, Vec<Seed>, Pass)> = None;
        while passes < MAX_PASSES && self.needs_rerun(&latest) {
            let next = self.explicit_cue_intervals(&latest);
            if same_intervals(&next, &seeds) {
                // The next pass would see exactly the same obstacles: it is a fixed point.
                break;
            }
            let score = self.score(&latest);
            let done = std::mem::replace(&mut latest, self.run_pass(&next, false));
            let done_seeds = std::mem::replace(&mut seeds, next);
            passes += 1;
            if best.as_ref().is_none_or(|(s, _, _)| score < *s) {
                best = Some((score, done_seeds, done));
            }
        }
        if let Some((score, best_seeds, best_pass)) = best
            && score <= self.score(&latest)
        {
            latest = best_pass;
            seeds = best_seeds;
        }
        if self.rerun_uncertain(&latest) {
            // Same geometry, every implicit cue marked as depending on estimates.
            latest = self.run_pass(&seeds, true);
        }
        (latest, passes)
    }

    /// True when exact durations could change whether (and how) the policy re-runs, or which pass it keeps:
    /// the push policy with implicit and explicit speech cues, some of whose intervals depend on estimates.
    fn rerun_uncertain(&self, pass: &Pass) -> bool {
        let speech: Vec<NodeId> = (0..self.node_count())
            .filter(|&v| self.is_speech_obstacle(v))
            .collect();
        speech.iter().any(|&v| self.is_implicit(v))
            && speech.iter().any(|&v| !self.is_implicit(v))
            && speech.iter().any(|&v| pass.interval_est(v))
    }

    fn score(&self, pass: &Pass) -> Score {
        let (mut count, mut frames) = (0_usize, 0_i128);
        self.visit_overlaps(&pass.slots, |_, _, overlap| {
            count += 1;
            frames += i128::from(overlap);
            ControlFlow::Continue(())
        });
        (count, frames, pass.displacement)
    }

    pub(super) fn run_pass(&self, seeds: &[Seed], force_est: bool) -> Pass {
        let n = self.node_count();
        let vertices = 2 * n;
        let mut pass = Pass::new(n);
        let mut pending: Vec<usize> = self.graph.deps.iter().map(Vec::len).collect();
        let mut tentative: Vec<Option<StartCalc>> = vec![None; n];
        let mut ready: BTreeSet<ReadyKey> = BTreeSet::new();
        let mut obstacles = Obstacles::new(n, seeds, self.tolerance, self.settings.reference_push);
        let mut reach = Reach::new(vertices);
        let mut work: VecDeque<Vertex> = VecDeque::new();
        let mut forced = 0;

        for v in (0..n).filter(|&v| pending[start_of(v)] == 0) {
            let calc = self.start_calc(v, &pass);
            self.enqueue(v, calc, &mut tentative, &mut ready);
        }
        while pass.resolved < vertices {
            let x = match ready.pop_first() {
                Some(key) => start_of(key.4),
                // Defensive: with an acyclic graph this never happens. Resolve the first unresolved vertex so
                // the loop always terminates (its unresolved references read as 0).
                None => {
                    while forced < vertices && pass.order[forced] != usize::MAX {
                        forced += 1;
                    }
                    if forced == vertices {
                        break;
                    }
                    forced
                }
            };
            let v = node_of(x);
            if is_end(x) {
                // Only on the defensive path; the start (`x - 1`, resolved first) is placed.
                self.resolve_end(v, &mut pass);
                work.push_back(x);
            } else {
                if pass.slots[v].is_some() {
                    continue;
                }
                let calc = tentative[v]
                    .take()
                    .unwrap_or_else(|| self.start_calc(v, &pass));
                let with_end =
                    self.place(v, calc, &mut pass, &mut obstacles, &mut reach, force_est);
                work.push_back(start_of(v));
                if with_end {
                    work.push_back(end_of(v));
                }
            }
            while let Some(y) = work.pop_front() {
                for &w in &self.graph.dependents[y] {
                    pending[w] = pending[w].saturating_sub(1);
                    if pending[w] != 0 || pass.order[w] != usize::MAX {
                        continue;
                    }
                    let u = node_of(w);
                    if is_end(w) {
                        if pass.slots[u].is_some() {
                            self.resolve_end(u, &mut pass);
                            work.push_back(w);
                        }
                    } else if pass.slots[u].is_none() && tentative[u].is_none() {
                        let calc = self.start_calc(u, &pass);
                        self.enqueue(u, calc, &mut tentative, &mut ready);
                    }
                }
            }
        }
        if pass.pushed > MAX_REPEATED {
            let summary = Diagnostic::info(
                codes::CUE_PUSHED,
                format!(
                    "{} more cues were moved by the speech-overlap policy; they are not listed individually",
                    pass.pushed - MAX_REPEATED
                ),
            )
            .with_hint("every moved cue reports its tentative start as `pushed_from_ms` in the plan items");
            let est = pass.unlisted_push_est;
            pass.diagnostics.push_if(summary, est);
        }
        pass
    }

    /// Put `v` in the ready set at `calc.start` (readiness order: explicit first, then the smallest tentative
    /// start, then declaration order).
    fn enqueue(
        &self,
        v: NodeId,
        calc: StartCalc,
        tentative: &mut [Option<StartCalc>],
        ready: &mut BTreeSet<ReadyKey>,
    ) {
        let fact = &self.facts.nodes[v];
        let rank = u8::from(self.is_implicit(v));
        ready.insert((
            rank,
            calc.start,
            fact.track.unwrap_or(usize::MAX),
            fact.pos,
            v,
        ));
        tentative[v] = Some(calc);
    }

    /// Fix the start of `v` in the timeline (implicit cues are pushed first), and its end too when its length
    /// can already be computed (then returns true). Called exactly once per node and pass.
    fn place(
        &self,
        v: NodeId,
        calc: StartCalc,
        pass: &mut Pass,
        obstacles: &mut Obstacles,
        reach: &mut Reach,
        force_est: bool,
    ) -> bool {
        if calc.raw < 0 {
            let diagnostic = self.negative_start(v, &calc, pass);
            pass.diagnostics.push_if(diagnostic, calc.est);
        }
        let with_end = self.graph.deps[end_of(v)]
            .iter()
            .all(|&w| w == start_of(v) || pass.order[w] != usize::MAX);
        let (len, from_source, len_est) = if with_end {
            self.length_at(v, calc.start, calc.est, pass)
        } else {
            (Some(0), false, false)
        };
        let mut slot = Slot {
            start: calc.start,
            len,
            pushed_from: None,
            from_source,
        };
        let mut start_est = calc.est;
        pass.resolve(start_of(v));
        if self.is_speech_obstacle(v) {
            if self.policy == OverlapPolicy::Push && self.is_implicit(v) {
                // A push depends on every speech interval in effect, and on the cue's own length.
                let (seen, seen_est) = obstacles.counts();
                if force_est || (seen > 0 && (len_est || seen_est > 0)) {
                    start_est = true;
                }
                self.push(v, &mut slot, obstacles, reach, pass, start_est);
            }
            obstacles.schedule(v, slot.interval(), start_est || len_est);
        }
        pass.seq[v] = pass.placed;
        pass.placed += 1;
        pass.slots[v] = Some(slot);
        pass.est[v] = Est {
            start: start_est,
            len: len_est,
            end: start_est || len_est,
        };
        if with_end {
            pass.end_done[v] = true;
            pass.resolve(end_of(v));
        }
        with_end
    }

    /// Resolve the end of a placed clip whose length waited for other items.
    fn resolve_end(&self, v: NodeId, pass: &mut Pass) {
        let Some(mut slot) = pass.slots[v] else {
            return;
        };
        if pass.end_done[v] {
            return;
        }
        let start_est = pass.est[v].start;
        let (len, from_source, len_est) = self.length_at(v, slot.start, start_est, pass);
        slot.len = len;
        slot.from_source = from_source;
        pass.slots[v] = Some(slot);
        pass.est[v].len = len_est;
        pass.est[v].end = start_est || len_est;
        pass.end_done[v] = true;
        pass.resolve(end_of(v));
    }

    /// The `push` policy (DESIGN.md §3.4): while the implicit cue overlaps scheduled speech, move it to
    /// `end(other) + gap`, `other` being the latest-ending cue it overlaps (so the gap follows all of them). The
    /// cue is placed as soon as it overlaps nothing: it keeps the priority of its original tentative start,
    /// which is what makes speakers relying on `push` take turns. Moves only go later, never before the cursor.
    ///
    /// Seeds (explicit cues as placed by the previous pass) that depend on the cue are ignored: they would move
    /// with it, so pushing it past them only chases them.
    fn push(
        &self,
        v: NodeId,
        slot: &mut Slot,
        obstacles: &mut Obstacles,
        reach: &mut Reach,
        pass: &mut Pass,
        est: bool,
    ) {
        let len = slot.len.unwrap_or(0);
        if len <= self.tolerance {
            // Overlaps by at most the tolerance never count.
            return;
        }
        reach.reset();
        let order = &pass.order;
        let target = start_of(v);
        // Unresolved vertices are the only ones that can depend on the cue being placed now.
        let mut depends = |node: NodeId| {
            reach.depends(&self.graph, start_of(node), target, |w| {
                order[w] == usize::MAX
            })
        };
        let start = slot.start;
        let (end, hit) = if obstacles.is_reference() {
            self.push_reference(v, start, len, obstacles, &mut depends)
        } else {
            self.push_fast(v, start, len, obstacles, &mut depends)
        };
        obstacles.restore_seeds();
        let Some(hit) = hit else {
            return;
        };
        slot.pushed_from = Some(start);
        slot.start = end;
        pass.displacement += i128::from(end) - i128::from(start);
        if pass.pushed < MAX_REPEATED {
            let diagnostic = self.cue_pushed(v, start, end, &hit);
            pass.diagnostics.push_if(diagnostic, est);
        } else if est {
            pass.unlisted_push_est = true;
        }
        pass.pushed += 1;
    }

    /// [`Scheduler::push`] on the ordered structures: one prefix-maximum query per move, and runs of chained
    /// obstacles skipped at once ([`super::obstacles::Staircase::jump`]). Returns the final start and the
    /// obstacle of the last move (None when the cue does not move).
    fn push_fast(
        &self,
        v: NodeId,
        start: i64,
        len: i64,
        obstacles: &mut Obstacles,
        depends: &mut impl FnMut(NodeId) -> bool,
    ) -> (i64, Option<Rec>) {
        let tol = self.tolerance;
        let gap = self.facts.nodes[v].gap;
        let window = len - tol;
        let (g, d) = (gap.saturating_add(window), gap.saturating_add(tol));
        let mut s = start;
        let mut last = None;
        // The last staircase record found: later queries search forward from it (no insertion happens
        // during a push, so positions stay valid, and query points only increase).
        let mut cursor = None;
        loop {
            let x = s.saturating_add(window);
            let stair = match cursor {
                Some(from) => Some(obstacles.staircase().last_before_after(from, x)),
                None => obstacles.staircase().last_before(x),
            };
            cursor = stair.or(cursor);
            let stair = stair.map(|p| (p, *obstacles.staircase().get(p)));
            let seed = obstacles.seed_before(x, depends);
            let (mut hit, at) = match (stair, seed) {
                (Some((p, r)), Some(q)) if !q.beats(&r) => (r, Some(p)),
                (_, Some(q)) => (q, None),
                (Some((p, r)), None) => (r, Some(p)),
                (None, None) => break,
            };
            if hit.end <= s.saturating_add(tol) {
                break;
            }
            if let Some(far_at) = at.and_then(|p| obstacles.staircase().jump(p, g, d)) {
                let far = *obstacles.staircase().get(far_at);
                // Seeds starting before the far record's window that end beyond the current one could block
                // somewhere along the skipped moves.
                let quiet = obstacles
                    .seed_before(far.end.saturating_add(g), depends)
                    .is_none_or(|q| q.end <= hit.end.saturating_add(d));
                if quiet {
                    hit = far;
                    cursor = Some(far_at);
                }
            }
            let to = hit.end.saturating_add(gap);
            if to <= s {
                break;
            }
            s = to;
            last = Some(hit);
        }
        (s, last)
    }

    /// The straightforward [`Scheduler::push`]: every move scans every obstacle in effect (a reference for
    /// tests of [`Scheduler::push_fast`]).
    fn push_reference(
        &self,
        v: NodeId,
        start: i64,
        len: i64,
        obstacles: &Obstacles,
        depends: &mut impl FnMut(NodeId) -> bool,
    ) -> (i64, Option<Rec>) {
        let gap = self.facts.nodes[v].gap;
        let mut s = start;
        let mut last = None;
        for _ in 0..=obstacles.reference_len() {
            let current = Interval::new(s, s.saturating_add(len));
            let Some(hit) = obstacles.reference_hit(v, current, depends) else {
                break;
            };
            let to = hit.end.saturating_add(gap);
            if to <= s {
                break;
            }
            s = to;
            last = Some(hit);
        }
        (s, last)
    }

    fn start_calc(&self, v: NodeId, pass: &Pass) -> StartCalc {
        let (raw, cause, est) = if self.is_implicit(v) {
            self.cursor(v, pass)
        } else {
            match &self.rules.start[v] {
                Start::Absolute(at) => (*at, Cause::Absolute, false),
                Start::Relative(a) => {
                    let (at, est) = self.relative_start(v, a, pass);
                    (at, Cause::Relative, est)
                }
                Start::Sequential => self.cursor(v, pass),
            }
        };
        StartCalc {
            start: raw.max(0),
            raw,
            cause,
            est,
        }
    }

    /// Where a sequential node starts: after its predecessor (minus this clip's crossfade), or at the track
    /// start for the first item.
    fn cursor(&self, v: NodeId, pass: &Pass) -> (i64, Cause, bool) {
        let fact = &self.facts.nodes[v];
        match fact.track {
            Some(t) if fact.pos > 0 => {
                let prev = self.facts.members[t][fact.pos - 1];
                let (base, est) = self.edge(prev, true, pass);
                (
                    base.saturating_sub(fact.crossfade_in),
                    Cause::Cursor(prev),
                    est,
                )
            }
            Some(t) => {
                let (at, est) = match &self.rules.track_start[t] {
                    Start::Sequential => (0, false),
                    Start::Absolute(at) => (*at, false),
                    Start::Relative(a) => self.relative_start(v, a, pass),
                };
                (at, Cause::TrackStart(t), est)
            }
            None => (0, Cause::Orphan, false),
        }
    }

    /// `after_cue`: end(target) + offset; `with_cue`: start(target) + offset; `before_cue`: the node ends at
    /// start(target) + offset.
    fn relative_start(&self, v: NodeId, a: &Anchor, pass: &Pass) -> (i64, bool) {
        let (point, est) = self.anchor_point(a, pass);
        let at = point.saturating_add(a.offset);
        if a.mode == AnchorMode::Before {
            // A length that depends on the start was reported as `conflicting_fields`; fall back to `with`.
            let len = self.facts.independent_length(self.rules, v).unwrap_or(0);
            (
                at.saturating_sub(len),
                est || self.facts.independent_length_est(self.rules, v),
            )
        } else {
            (at, est)
        }
    }

    /// The start (or, with `end`, the end) of a placed node and whether it depends on estimates. Unplaced
    /// nodes (only possible on the defensive forced path) read as 0; the end of an open-ended clip reads as its
    /// start (`anchor_open_ended`).
    fn edge(&self, w: NodeId, end: bool, pass: &Pass) -> (i64, bool) {
        match pass.slots[w] {
            Some(s) if end && s.len.is_some() => (s.end_or_start(), pass.est[w].end),
            Some(s) => (s.start, pass.est[w].start),
            None => (0, false),
        }
    }

    /// The anchored edge of the target (without the offset), and whether it depends on estimates.
    fn anchor_point(&self, a: &Anchor, pass: &Pass) -> (i64, bool) {
        let after = a.mode == AnchorMode::After;
        match a.target {
            Target::Node(w) => self.edge(w, after, pass),
            Target::Track(t) => {
                let list = &self.facts.members[t];
                match (list.first(), list.last()) {
                    (Some(&first), Some(&last)) => {
                        if after {
                            self.edge(last, true, pass)
                        } else {
                            self.edge(first, false, pass)
                        }
                    }
                    _ => (self.empty_track_start(t), false),
                }
            }
            Target::Invalid => (0, false),
        }
    }

    /// Where a track without items "is": its absolute start, else 0.
    pub(super) fn empty_track_start(&self, t: TrackIdx) -> i64 {
        match self.rules.track_start[t] {
            Start::Absolute(at) => at.max(0),
            _ => 0,
        }
    }

    /// The node's length given its start (DESIGN.md §3.3); `start_est` says whether the start depends on
    /// estimates.
    fn length_at(&self, v: NodeId, start: i64, start_est: bool, pass: &mut Pass) -> LengthCalc {
        let fact = &self.facts.nodes[v];
        match &self.rules.length[v] {
            Length::Intrinsic | Length::NaturalFinite => (Some(fact.len), true, fact.len_est),
            Length::Natural => {
                if fact.looped {
                    (None, false, false)
                } else {
                    (Some(fact.len), true, fact.len_est)
                }
            }
            Length::Duration(d) => self.cut(v, *d, LengthField::Duration, false, pass),
            Length::EndAbsolute(end) => {
                self.until(v, *end, start, LengthField::EndAt, start_est, pass)
            }
            Length::EndRelative(a) => {
                let (point, est) = self.anchor_point(a, pass);
                let end = point.saturating_add(a.offset);
                self.until(
                    v,
                    end,
                    start,
                    LengthField::EndRelative,
                    est || start_est,
                    pass,
                )
            }
        }
    }

    /// A clip that ends at `end`; an end before the start is an `invalid_value` error and plays nothing.
    fn until(
        &self,
        v: NodeId,
        end: i64,
        start: i64,
        field: LengthField,
        est: bool,
        pass: &mut Pass,
    ) -> LengthCalc {
        if end < start {
            let sr = self.sample_rate();
            let diagnostic = Diagnostic::error(
                codes::INVALID_VALUE,
                format!(
                    "clip `{}` would end at {} s, before it starts at {} s (`{}`); it plays nothing",
                    self.names.id(v),
                    secs(end, sr),
                    secs(start, sr),
                    field.name()
                ),
            )
            .with_path(self.names.field(v, field.name()))
            .with_hint(format!(
                "make the end later than the start ({} ms), or remove `{}` to play the natural length",
                ceil_ms(start, sr),
                field.name()
            ))
            .with_related([self.names.id(v)]);
            pass.diagnostics.push_if(diagnostic, est);
            return (Some(0), false, est);
        }
        self.cut(v, end - start, field, est, pass)
    }

    /// A requested length, cut to the natural length for non-looping clips whose source length is known
    /// (`clip_shorter_than_requested`). `est` says whether the requested length depends on estimates.
    fn cut(
        &self,
        v: NodeId,
        requested: i64,
        field: LengthField,
        est: bool,
        pass: &mut Pass,
    ) -> LengthCalc {
        let fact = &self.facts.nodes[v];
        let requested = requested.max(0);
        if fact.looped || !fact.source_known || requested <= fact.len {
            // An unknown source may turn out shorter than the request and cut it.
            let unknown_cut = fact.len_est && !fact.looped;
            return (Some(requested), false, est || unknown_cut);
        }
        let sr = self.sample_rate();
        let trim = if fact.trim_start > 0 {
            format!(" after `trim_start_ms` ({} s)", secs(fact.trim_start, sr))
        } else {
            String::new()
        };
        let diagnostic = Diagnostic::info(
            codes::CLIP_SHORTER_THAN_REQUESTED,
            format!(
                "clip `{}` requests {} s (`{}`), but its source `{}` provides only {} s{trim}; it plays {} s",
                self.names.id(v),
                secs(requested, sr),
                field.name(),
                self.names.source(v),
                secs(fact.len, sr),
                secs(fact.len, sr)
            ),
        )
        .with_path(self.names.field(v, field.name()))
        .with_hint(format!(
            "set `loop: true` on `{}` to fill the requested length, or request at most {} ms",
            self.names.id(v),
            floor_ms(fact.len, sr)
        ))
        .with_related([self.names.id(v)]);
        pass.diagnostics.push_if(diagnostic, est);
        (Some(fact.len), true, est)
    }

    /// True when an implicit cue overlaps an explicit cue that was scheduled after it and does not depend on it:
    /// the next pass, with the explicit cues' intervals pre-seeded, can move the implicit cue out of the way.
    /// (An explicit cue that depends on the implicit one would move with it.)
    fn needs_rerun(&self, pass: &Pass) -> bool {
        let mut reach = Reach::new(self.graph.deps.len());
        let mut found = false;
        self.visit_overlaps(&pass.slots, |a, b, _| {
            let (implicit, explicit) = match (self.is_implicit(a), self.is_implicit(b)) {
                (true, false) => (a, b),
                (false, true) => (b, a),
                _ => return ControlFlow::Continue(()),
            };
            if pass.seq[implicit] < pass.seq[explicit]
                && !self.depends_in(pass, &mut reach, explicit, implicit)
            {
                found = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        found
    }

    /// The intervals of every explicit speech cue (the seeds of the next pass), by node.
    fn explicit_cue_intervals(&self, pass: &Pass) -> Vec<Seed> {
        (0..self.node_count())
            .filter(|&v| self.is_speech_obstacle(v) && !self.is_implicit(v))
            .filter_map(|v| {
                pass.slots[v].map(|s| Seed {
                    node: v,
                    interval: s.interval(),
                    est: pass.interval_est(v),
                })
            })
            .collect()
    }

    /// Call `visit(earlier, later, overlap_frames)` for every pair of speech cues overlapping by more than the
    /// tolerance (`allow_overlap` cues excluded), ordered by the earlier cue's start. Cues starting together
    /// are ordered by declaration, so the later-declared one counts as "later".
    pub(super) fn visit_overlaps(
        &self,
        slots: &[Option<Slot>],
        mut visit: impl FnMut(NodeId, NodeId, i64) -> ControlFlow<()>,
    ) {
        let mut cues: Vec<(i64, NodeId, i64)> = (0..self.node_count())
            .filter(|&v| self.is_speech_obstacle(v))
            .filter_map(|v| slots[v].map(|s| (s.start, v, s.end_or_start())))
            .collect();
        cues.sort_unstable();
        for (i, &(_, a, end_a)) in cues.iter().enumerate() {
            for &(start_b, b, end_b) in &cues[i + 1..] {
                if start_b >= end_a {
                    break;
                }
                let overlap = end_a.min(end_b) - start_b;
                if overlap > self.tolerance && visit(a, b, overlap).is_break() {
                    return;
                }
            }
        }
    }

    fn cue_pushed(&self, v: NodeId, from: i64, to: i64, hit: &Rec) -> Diagnostic {
        let sr = self.sample_rate();
        let names = self.names;
        let seeded = if hit.seed {
            ", as placed by the previous scheduling pass"
        } else {
            ""
        };
        Diagnostic::info(
            codes::CUE_PUSHED,
            format!(
                "cue `{}` (track `{}`) was pushed from {} s to {} s by `{}` (track `{}`, {}{seeded}) to avoid \
                 overlapping speech",
                names.id(v),
                names.track_of(v),
                secs(from, sr),
                secs(to, sr),
                names.id(hit.node),
                names.track_of(hit.node),
                span(hit.start, hit.end, sr),
            ),
        )
        .with_path(names.path(v))
        .with_hint(format!(
            "give `{}` a `start_at_ms`/`start_relative` to place it yourself, or set `allow_overlap: true` to keep \
             it at {} s",
            names.id(v),
            secs(from, sr)
        ))
        .with_related([names.id(v), names.id(hit.node)])
    }

    fn negative_start(&self, v: NodeId, calc: &StartCalc, pass: &Pass) -> Diagnostic {
        let sr = self.sample_rate();
        let names = self.names;
        let deficit = calc.raw.checked_neg().unwrap_or(i64::MAX);
        let describe = |a: &Anchor| {
            format!(
                "`{}: {}`, `offset_ms: {}`",
                mode_name(a.mode),
                a.target_id,
                ms_num(a.offset_ms)
            )
        };
        let offset_fix = |a: &Anchor| {
            format!(
                "use `offset_ms: {}` or greater on `{}: {}`",
                ms_num(a.offset_ms + ceil_ms(deficit, sr) as f64),
                mode_name(a.mode),
                a.target_id
            )
        };
        let start_at_fix = || "use a `start_at_ms` of 0 or more".to_string();
        let (message, path, hint) = match calc.cause {
            Cause::Relative | Cause::Absolute | Cause::Orphan => {
                let (rule, path, hint) = match &self.rules.start[v] {
                    Start::Relative(a) if a.mode == AnchorMode::Before => (
                        self.before_rule(v, a, pass),
                        names.field(v, "start_relative"),
                        self.before_fix(v, a, pass),
                    ),
                    Start::Relative(a) => {
                        (describe(a), names.field(v, "start_relative"), offset_fix(a))
                    }
                    _ => (
                        "`start_at_ms`".to_string(),
                        names.field(v, "start_at_ms"),
                        start_at_fix(),
                    ),
                };
                let message = format!(
                    "`{}` would start at {} s ({rule}); it was clamped to 0.00 s",
                    names.id(v),
                    secs(calc.raw, sr)
                );
                (message, path, hint)
            }
            Cause::TrackStart(t) => {
                let (rule, path, hint) = match &self.rules.track_start[t] {
                    Start::Relative(a) if a.mode == AnchorMode::Before => (
                        self.before_rule(v, a, pass),
                        names.track_field(t, "start_relative"),
                        self.before_fix(v, a, pass),
                    ),
                    Start::Relative(a) => (
                        describe(a),
                        names.track_field(t, "start_relative"),
                        offset_fix(a),
                    ),
                    _ => (
                        "`start_at_ms`".to_string(),
                        names.track_field(t, "start_at_ms"),
                        start_at_fix(),
                    ),
                };
                let message = format!(
                    "track `{}` would start at {} s ({rule}); its first item `{}` was clamped to 0.00 s",
                    names.track_id(t),
                    secs(calc.raw, sr),
                    names.id(v)
                );
                (message, path, hint)
            }
            Cause::Cursor(prev) => {
                let prev_end = pass.slots[prev].map_or(0, |s| s.end_or_start());
                let message = format!(
                    "`{}` would start at {} s because its `crossfade_ms` ({} s) is longer than `{}`, which ends at \
                     {} s; it was clamped to 0.00 s",
                    names.id(v),
                    secs(calc.raw, sr),
                    secs(self.facts.nodes[v].crossfade_in, sr),
                    names.id(prev),
                    secs(prev_end, sr)
                );
                let hint = format!(
                    "reduce `crossfade_ms` on `{}` to at most {}",
                    names.id(v),
                    floor_ms(prev_end, sr)
                );
                (message, names.field(v, "crossfade_ms"), hint)
            }
        };
        Diagnostic::warning(codes::NEGATIVE_START, message)
            .with_path(path)
            .with_hint(hint)
            .with_related([names.id(v)])
    }

    /// The rule of `v` placed with `before_cue` (directly or as the first item of its track), for messages:
    /// the anchor and why the start is negative.
    fn before_rule(&self, v: NodeId, a: &Anchor, pass: &Pass) -> String {
        let sr = self.sample_rate();
        let len = self.facts.independent_length(self.rules, v).unwrap_or(0);
        let (point, _) = self.anchor_point(a, pass);
        format!(
            "`before_cue: {}`, `offset_ms: {}`: it lasts {} s and has to end at {} s",
            a.target_id,
            ms_num(a.offset_ms),
            secs(len, sr),
            secs(point.saturating_add(a.offset), sr)
        )
    }

    /// The hint for a `before_cue` item that would start before 0: it needs its whole length before the anchor
    /// point, so the target has to start later or the item has to be shorter. (A larger `offset_ms` would make
    /// it end after the target starts, which for speech is an overlap.)
    fn before_fix(&self, v: NodeId, a: &Anchor, pass: &Pass) -> String {
        let sr = self.sample_rate();
        let names = self.names;
        let len = self.facts.independent_length(self.rules, v).unwrap_or(0);
        let (point, _) = self.anchor_point(a, pass);
        let needed = ceil_ms(len.saturating_sub(a.offset), sr);
        let target = match a.target {
            Target::Track(t) => match self.facts.members[t].first() {
                Some(&first) => format!(
                    "`{}` (the first item of track `{}`)",
                    names.id(first),
                    names.track_id(t)
                ),
                None => format!("track `{}`", names.track_id(t)),
            },
            _ => format!("`{}`", a.target_id),
        };
        let mut hint = format!(
            "make {target} start at {needed} ms or later (for example `start_at_ms: {needed}`; it starts at {} s) so \
             that `{}` ({} s) fits before it",
            secs(point, sr),
            names.id(v),
            secs(len, sr)
        );
        let room = point.saturating_add(a.offset);
        if room > 0 {
            let room_ms = floor_ms(room, sr);
            if self.facts.nodes[v].kind == Kind::Clip {
                hint.push_str(&format!(
                    ", or give `{}` a `duration_ms` of at most {room_ms}",
                    names.id(v)
                ));
            } else {
                hint.push_str(&format!(
                    ", or shorten `{}` to at most {room_ms} ms",
                    names.id(v)
                ));
            }
        }
        hint
    }
}

/// True when two seed lists hold the same intervals for the same nodes.
fn same_intervals(a: &[Seed], b: &[Seed]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.node == y.node && x.interval == y.interval)
}
