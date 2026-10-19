//! The layout engine (DESIGN.md §3): a pure function of the lowered program and a duration table.
//!
//! Phases, one submodule each:
//! 1. [`facts`]: per-node static data in frames (lengths, trims, crossfades, push gaps) and the cleaned
//!    member list of every track; source-range errors.
//! 2. [`rules`]: each node's effective start and length rule with anchors resolved to node/track indices,
//!    and the dependency graph between the start and the end of every node (a clip's end may wait for items
//!    that are anchored to its start). The rules are a private copy, so the cycle breaker can demote anchors
//!    without touching the program.
//! 3. [`cycles`] (defensive; lowering already rejects cycles): report each dependency cycle and demote one
//!    anchor on it to sequential placement, until the dependency graph is acyclic.
//! 4. [`checks`]: rule combinations that cannot be honoured whatever the durations are.
//! 5. [`passes`]: nodes are placed in dependency order; among ready nodes explicit starts go first, then the
//!    smallest tentative start, then declaration order. Under the `push` policy implicit cues move past
//!    already scheduled speech ([`obstacles`] keeps that speech in ordered structures, so a push costs a few
//!    logarithmic queries); if an implicit cue ends up colliding with an explicit cue that was scheduled after
//!    it, the pass is re-run with the explicit cues' intervals pre-seeded (at most [`MAX_PASSES`] passes, the
//!    best of which is kept).
//! 6. [`finish`]: overlap diagnostics, content end, tail, total, open-ended lengths, fades, truncation, track
//!    spans and track-anchor checks.
//!
//! Every diagnostic is recorded together with its estimate dependence (see
//! [`crate::Layout::estimate_dependent`]).

mod checks;
mod cycles;
mod facts;
mod finish;
mod obstacles;
mod passes;
mod rules;

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::path;
use soniccraft_core::ir::{AnchorMode, NodeId, NodeKind, Program, TrackIdx};

use crate::types::{Layout, NodeDuration};
use facts::Facts;
use passes::Scheduler;
use rules::{Graph, Rules};

/// Maximum number of scheduling passes of the `push` policy (DESIGN.md §3.4).
pub(crate) const MAX_PASSES: u32 = 4;

/// At most this many `speech_overlap` and `cue_pushed` diagnostics are listed one by one; the rest are
/// summarized in a single diagnostic, so a pathological timeline cannot flood a report.
const MAX_REPEATED: usize = 50;

/// How many cut items a `content_truncated` message lists by id.
const TRUNCATION_LIST_MAX: usize = 5;

/// How the layout runs. Only tests change the default.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Settings {
    /// Push implicit cues with the straightforward implementation (scan every obstacle on every move)
    /// instead of the ordered structures. The results are identical; property tests compare the two.
    pub(crate) reference_push: bool,
}

/// Lay out `program` with the given duration table. See [`crate::layout`].
pub(crate) fn run(program: &Program, durations: &[NodeDuration], settings: Settings) -> Layout {
    let names = Names { program };
    let mut diags = Diags::default();
    let facts = Facts::new(program, durations, names, &mut diags);
    let mut rules = Rules::new(program, &facts, names, &mut diags);
    cycles::break_cycles(names, &facts, &mut rules, &mut diags);
    checks::static_checks(names, &facts, &rules, &mut diags);

    let scheduler = Scheduler {
        names,
        program,
        graph: Graph::new(&facts, &rules),
        facts: &facts,
        rules: &rules,
        policy: program.timing.speech_overlap,
        tolerance: facts.frames(program.timing.overlap_tolerance_ms).max(0),
        settings,
    };
    let (mut pass, passes) = scheduler.schedule();
    diags.append(&mut pass.diagnostics);
    scheduler.finish(pass, passes, diags)
}

/// Diagnostics with their estimate dependence: `estimated[i]` is true when `list[i]` depends on a time that
/// is derived from an estimated duration (see [`crate::Layout::estimate_dependent`]).
#[derive(Debug, Default)]
pub(super) struct Diags {
    list: Vec<Diagnostic>,
    estimated: Vec<bool>,
}

impl Diags {
    /// Record a diagnostic that holds whatever the cue lengths are (static rules, exact times).
    pub(super) fn push(&mut self, diagnostic: Diagnostic) {
        self.push_if(diagnostic, false);
    }

    /// Record a diagnostic; `estimated` says whether its condition depends on estimated durations.
    pub(super) fn push_if(&mut self, diagnostic: Diagnostic, estimated: bool) {
        self.list.push(diagnostic);
        self.estimated.push(estimated);
    }

    /// Move every diagnostic of `other` to the end of `self`.
    pub(super) fn append(&mut self, other: &mut Diags) {
        self.list.append(&mut other.list);
        self.estimated.append(&mut other.estimated);
    }

    /// The diagnostics and their flags, as two parallel vectors.
    pub(super) fn into_parts(self) -> (Vec<Diagnostic>, Vec<bool>) {
        (self.list, self.estimated)
    }
}

/// Ids and diagnostic paths of nodes and tracks, tolerant of out-of-range indices.
#[derive(Clone, Copy)]
struct Names<'a> {
    program: &'a Program,
}

impl<'a> Names<'a> {
    fn id(&self, v: NodeId) -> &'a str {
        self.program.nodes.get(v).map_or("?", |n| n.id.as_str())
    }

    fn path(&self, v: NodeId) -> &'a str {
        self.program.nodes.get(v).map_or("", |n| n.path.as_str())
    }

    fn field(&self, v: NodeId, name: &str) -> String {
        path::field(self.path(v), name)
    }

    fn track_id(&self, t: TrackIdx) -> &'a str {
        self.program.tracks.get(t).map_or("?", |t| t.id.as_str())
    }

    fn track_field(&self, t: TrackIdx, name: &str) -> String {
        path::field(
            self.program.tracks.get(t).map_or("", |t| t.path.as_str()),
            name,
        )
    }

    fn track_of(&self, v: NodeId) -> &'a str {
        self.program
            .nodes
            .get(v)
            .map_or("?", |n| self.track_id(n.track))
    }

    fn source(&self, v: NodeId) -> &'a str {
        match self.program.nodes.get(v).map(|n| &n.kind) {
            Some(NodeKind::Clip(c)) => c.source.as_str(),
            _ => "",
        }
    }

    fn is_pause(&self, v: NodeId) -> bool {
        matches!(
            self.program.nodes.get(v).map(|n| &n.kind),
            Some(NodeKind::Pause { .. })
        )
    }
}

/// The DSL key of an anchor mode.
fn mode_name(mode: AnchorMode) -> &'static str {
    match mode {
        AnchorMode::After => "after_cue",
        AnchorMode::With => "with_cue",
        AnchorMode::Before => "before_cue",
    }
}
