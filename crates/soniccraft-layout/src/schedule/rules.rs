//! Phase 2: effective start/length rules in frames, and the dependency graph they induce.

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::{codes, path};
use soniccraft_core::ir::{
    AnchorMode, AnchorRule, AnchorTarget, ClipLength, NodeId, NodeKind, Program, StartRule,
    TrackIdx,
};

use super::facts::Facts;
use super::{Diags, Names};

/// What an anchor refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Node(NodeId),
    Track(TrackIdx),
    /// Out-of-range reference (reported as `unknown_reference`; resolves to time 0).
    Invalid,
}

/// A resolved anchor in frames.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Anchor {
    pub(super) target: Target,
    pub(super) mode: AnchorMode,
    /// `offset_ms` in frames.
    pub(super) offset: i64,
    pub(super) offset_ms: f64,
    pub(super) target_id: String,
}

/// An effective start rule.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Start {
    Sequential,
    Absolute(i64),
    Relative(Anchor),
}

/// An effective length rule.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Length {
    /// Cues and pauses: the node's own length (audio or pause length).
    Intrinsic,
    /// Natural length; open-ended when the clip loops.
    Natural,
    /// Natural length even when looping (an `end_relative` demoted by the cycle breaker).
    NaturalFinite,
    Duration(i64),
    EndAbsolute(i64),
    EndRelative(Anchor),
}

/// The effective rules of every node and track.
pub(super) struct Rules {
    pub(super) start: Vec<Start>,
    pub(super) length: Vec<Length>,
    pub(super) track_start: Vec<Start>,
}

impl Rules {
    pub(super) fn new(
        program: &Program,
        facts: &Facts,
        names: Names<'_>,
        diags: &mut Diags,
    ) -> Self {
        let n = program.nodes.len();
        let tracks = program.tracks.len();
        let anchor = |rule: &AnchorRule, owner: &str, field: String, diags: &mut Diags| {
            let target = match rule.target {
                AnchorTarget::Node(w) if w < n => Target::Node(w),
                AnchorTarget::Track(t) if t < tracks => Target::Track(t),
                _ => {
                    diags.push(
                        Diagnostic::error(
                            codes::UNKNOWN_REFERENCE,
                            format!(
                                "`{owner}` is anchored to `{}`, which is not a cue, clip or track of this timeline; \
                                 the anchor resolves to 0 s",
                                rule.target_id
                            ),
                        )
                        .with_path(field)
                        .with_hint("reference the id of an existing cue, clip or track")
                        .with_related([owner.to_string(), rule.target_id.clone()]),
                    );
                    Target::Invalid
                }
            };
            Anchor {
                target,
                mode: rule.mode,
                offset: facts.frames(rule.offset_ms),
                offset_ms: rule.offset_ms,
                target_id: rule.target_id.clone(),
            }
        };
        let start_rule = |rule: &StartRule, owner: &str, base: &str, diags: &mut Diags| match rule {
            StartRule::Sequential => Start::Sequential,
            StartRule::Absolute { ms } => Start::Absolute(facts.frames(*ms)),
            StartRule::Relative(a) => {
                Start::Relative(anchor(a, owner, path::field(base, "start_relative"), diags))
            }
        };

        let mut start = Vec::with_capacity(n);
        let mut length = Vec::with_capacity(n);
        for (v, node) in program.nodes.iter().enumerate() {
            // Pauses are always sequential; whatever start rule the IR carries is ignored.
            start.push(match node.kind {
                NodeKind::Pause { .. } => Start::Sequential,
                _ => start_rule(&node.start, &node.id, &node.path, diags),
            });
            length.push(match &node.kind {
                NodeKind::Clip(clip) => match &clip.length {
                    ClipLength::Natural => Length::Natural,
                    ClipLength::Duration { ms } => Length::Duration(facts.frames(*ms).max(0)),
                    ClipLength::EndAbsolute { ms } => Length::EndAbsolute(facts.frames(*ms)),
                    ClipLength::EndRelative(rule) => {
                        let field = names.field(v, "end_relative");
                        let mut a = anchor(rule, &node.id, field.clone(), diags);
                        if a.mode == AnchorMode::Before {
                            diags.push(
                                Diagnostic::error(
                                    codes::INVALID_VALUE,
                                    format!(
                                        "`before_cue` is not valid in `end_relative` (clip `{}`); it was treated as \
                                         `with_cue`",
                                        node.id
                                    ),
                                )
                                .with_path(path::field(&field, "before_cue"))
                                .with_hint(format!(
                                    "use `end_relative: {{with_cue: {}}}` to end the clip when `{}` starts",
                                    rule.target_id, rule.target_id
                                ))
                                .with_related([node.id.clone()]),
                            );
                            a.mode = AnchorMode::With;
                        }
                        Length::EndRelative(a)
                    }
                },
                _ => Length::Intrinsic,
            });
        }
        let track_start = program
            .tracks
            .iter()
            .map(|track| start_rule(&track.start, &track.id, &track.path, diags))
            .collect();
        Rules {
            start,
            length,
            track_start,
        }
    }
}

/// A scheduling event: the start or the end of a node. Start and end are separate times (DESIGN.md §2.13,
/// §3.3), so an item may wait for a clip's start while the clip's end waits for that item: `S(v) = 2v`,
/// `E(v) = 2v + 1`. Only a clip with `end_relative` has an end that depends on other items; every other node's
/// end follows from its start and own length, so it is resolved together with the start.
pub(super) type Vertex = usize;

/// `S(v)`.
pub(super) const fn start_of(v: NodeId) -> Vertex {
    2 * v
}

/// `E(v)`.
pub(super) const fn end_of(v: NodeId) -> Vertex {
    2 * v + 1
}

/// The node of a vertex.
pub(super) const fn node_of(x: Vertex) -> NodeId {
    x / 2
}

/// True for `E(v)` vertices.
pub(super) const fn is_end(x: Vertex) -> bool {
    x % 2 == 1
}

/// Why one vertex depends on another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Edge {
    /// A sequential node's start needs its predecessor's end.
    Sequential,
    /// The start of the first sequential node of a track needs the track's `start_relative` anchor.
    TrackStart,
    /// The node's own `start_relative` anchor.
    Start,
    /// A clip's `end_relative` anchor.
    Length,
    /// A node's end needs its own start.
    Own,
}

/// The vertex an anchor reads: the end of its target for `after_cue`, else the start. A track resolves to
/// its first declared item (`with_cue`/`before_cue`) or its last declared item (`after_cue`), as in core's
/// graph (DESIGN.md §2.13).
pub(super) fn anchor_vertex(facts: &Facts, a: &Anchor) -> Option<Vertex> {
    let edge = |w: NodeId| {
        if a.mode == AnchorMode::After {
            end_of(w)
        } else {
            start_of(w)
        }
    };
    match a.target {
        Target::Node(w) => Some(edge(w)),
        Target::Track(t) => {
            let list = &facts.members[t];
            let pick = if a.mode == AnchorMode::After {
                list.last()
            } else {
                list.first()
            };
            pick.map(|&w| edge(w))
        }
        Target::Invalid => None,
    }
}

/// The vertices `x` depends on, with the reason. May contain repeats.
pub(super) fn labeled_deps(facts: &Facts, rules: &Rules, x: Vertex) -> Vec<(Vertex, Edge)> {
    let v = node_of(x);
    let mut out = Vec::new();
    let push_anchor = |a: &Anchor, edge: Edge, out: &mut Vec<(Vertex, Edge)>| {
        if let Some(w) = anchor_vertex(facts, a) {
            out.push((w, edge));
        }
    };
    if is_end(x) {
        out.push((start_of(v), Edge::Own));
        if let Length::EndRelative(a) = &rules.length[v] {
            push_anchor(a, Edge::Length, &mut out);
        }
        return out;
    }
    let fact = &facts.nodes[v];
    if facts.is_sequential(rules, v) {
        match fact.track {
            Some(t) if fact.pos > 0 => {
                out.push((end_of(facts.members[t][fact.pos - 1]), Edge::Sequential));
            }
            Some(t) => {
                if let Start::Relative(a) = &rules.track_start[t] {
                    push_anchor(a, Edge::TrackStart, &mut out);
                }
            }
            None => {}
        }
    } else if let Start::Relative(a) = &rules.start[v] {
        push_anchor(a, Edge::Start, &mut out);
    }
    out
}

/// Dependencies between vertices (a vertex can be resolved once all its dependencies are).
pub(super) struct Graph {
    /// Distinct dependencies of each vertex, sorted.
    pub(super) deps: Vec<Vec<Vertex>>,
    /// Distinct dependents of each vertex, sorted.
    pub(super) dependents: Vec<Vec<Vertex>>,
}

impl Graph {
    pub(super) fn new(facts: &Facts, rules: &Rules) -> Self {
        let n = 2 * facts.nodes.len();
        let mut deps = Vec::with_capacity(n);
        let mut dependents = vec![Vec::new(); n];
        for x in 0..n {
            let mut d: Vec<Vertex> = labeled_deps(facts, rules, x)
                .into_iter()
                .map(|(w, _)| w)
                .collect();
            d.sort_unstable();
            d.dedup();
            for &w in &d {
                dependents[w].push(x);
            }
            deps.push(d);
        }
        Graph { deps, dependents }
    }

    /// Vertices that can never be resolved (on a cycle, or depending on one).
    pub(super) fn stuck(&self) -> Vec<bool> {
        let n = self.deps.len();
        let mut pending: Vec<usize> = self.deps.iter().map(Vec::len).collect();
        let mut stack: Vec<Vertex> = (0..n).filter(|&x| pending[x] == 0).collect();
        let mut done = vec![false; n];
        while let Some(x) = stack.pop() {
            if std::mem::replace(&mut done[x], true) {
                continue;
            }
            for &w in &self.dependents[x] {
                pending[w] = pending[w].saturating_sub(1);
                if pending[w] == 0 {
                    stack.push(w);
                }
            }
        }
        done.into_iter().map(|d| !d).collect()
    }
}

/// Reachability queries on a [`Graph`] ("does `from` depend on `to`?") with reusable buffers.
///
/// Queries for the same target share what they learn: after a query returns false, every vertex it explored
/// is known not to reach the target, and later queries skip those vertices until [`Reach::reset`].
pub(super) struct Reach {
    stamp: Vec<u32>,
    epoch: u32,
    stack: Vec<Vertex>,
}

impl Reach {
    pub(super) fn new(vertices: usize) -> Self {
        Reach {
            stamp: vec![0; vertices],
            epoch: 1,
            stack: Vec::new(),
        }
    }

    /// Forget what earlier queries learned (call it before queries for a different target).
    pub(super) fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.epoch = 1;
        }
    }

    /// True when vertex `from` depends on vertex `to`, directly or transitively (a vertex depends on itself).
    /// The search only walks through vertices for which `explore` returns true, so a caller that knows which
    /// vertices cannot lie on such a path (for example those resolved before `to` in a scheduling pass) keeps
    /// the search short.
    pub(super) fn depends(
        &mut self,
        graph: &Graph,
        from: Vertex,
        to: Vertex,
        explore: impl Fn(Vertex) -> bool,
    ) -> bool {
        if from == to {
            return true;
        }
        if from >= graph.deps.len() || self.stamp[from] == self.epoch {
            return false;
        }
        self.stamp[from] = self.epoch;
        self.stack.clear();
        self.stack.push(from);
        while let Some(x) = self.stack.pop() {
            for &w in &graph.deps[x] {
                if w == to {
                    // The explored vertices are not all known to miss `to`: start the next query afresh.
                    self.reset();
                    return true;
                }
                if self.stamp[w] != self.epoch && explore(w) {
                    self.stamp[w] = self.epoch;
                    self.stack.push(w);
                }
            }
        }
        false
    }
}
