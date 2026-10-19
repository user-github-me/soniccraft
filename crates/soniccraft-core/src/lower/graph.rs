//! The static dependency graph of a timeline (DESIGN.md §2.13, §3.1–§3.3): dependency cycles and anchors
//! on the end of open-ended loops.
//!
//! Vertices are the start `S(n)` and end `E(n)` of every node (cue, pause, clip) and the start rule
//! `T(t)` of every track. Edges point from a time to the times it is computed from:
//!
//! - `E(n) → S(n)` (an end is a start plus a length);
//! - a sequential node: `S(n) → E(previous node)`, or `S(n) → T(track)` for the first node;
//! - `after_cue X` → `E(X)`; `with_cue X` / `before_cue X` → `S(X)`. A track `X` is anchored in
//!   declaration order, exactly as the layout places it: `with_cue` / `before_cue` read the start of its
//!   first declared item, `after_cue` the end of its last declared item (one edge, not one per item);
//! - `start_relative` of a node starts at `S(n)`, `end_relative` of a clip at `E(n)`, a track's
//!   `start_relative` at `T(t)`.
//!
//! Every strongly connected component with more than one vertex is one `dependency_cycle`, reported once
//! with a shortest cycle through its smallest vertex (e.g. `cue_02 → cue_03 → cue_02`); a step through a
//! track anchor names the item it resolves to (`voice (track anchor: last item `outro`)`).

use std::collections::{HashSet, VecDeque};

use super::Lowerer;
use crate::diagnostics::{Diagnostic, codes};
use crate::ir::{
    AnchorMode, AnchorRule, AnchorTarget, ClipLength, NodeId, NodeKind, StartRule, TrackIdx,
};

/// Who owns an anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    /// A node and its track.
    Node(NodeId, TrackIdx),
    /// A track's `start_relative`.
    Track(TrackIdx),
}

/// Which time an anchor determines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    Start,
    End,
}

/// A resolved anchor and where it is written.
#[derive(Debug, Clone)]
pub(super) struct AnchorUse {
    pub owner: Owner,
    pub slot: Slot,
    pub rule: AnchorRule,
    /// Path of the anchor's target field (`…start_relative.after_cue`).
    pub path: String,
}

/// Run the graph checks.
pub(super) fn check(lw: &mut Lowerer<'_>) {
    open_ended(lw);
    cycles(lw);
}

/// A looping clip without a length. Clips whose length field was invalid (and reported) are not: their
/// fallback `Natural` length is an artifact of error recovery.
fn is_open_ended(lw: &Lowerer<'_>, node: NodeId) -> bool {
    !lw.invalid_length.contains(&node)
        && matches!(lw.nodes.get(node).map(|n| &n.kind), Some(NodeKind::Clip(c)) if c.looped && c.length == ClipLength::Natural)
}

/// The node a track anchor resolves to: the first declared item for `with_cue` / `before_cue`, the last
/// for `after_cue` (pauses included, as in the layout).
fn track_anchor_node(lw: &Lowerer<'_>, track: TrackIdx, mode: AnchorMode) -> Option<NodeId> {
    let nodes = &lw.tracks.get(track)?.nodes;
    if mode == AnchorMode::After {
        nodes.last().copied()
    } else {
        nodes.first().copied()
    }
}

fn owner_id(lw: &Lowerer<'_>, owner: Owner) -> String {
    match owner {
        Owner::Node(n, _) => lw.nodes.get(n).map(|n| n.id.clone()).unwrap_or_default(),
        Owner::Track(t) => lw.tracks.get(t).map(|t| t.id.clone()).unwrap_or_default(),
    }
}

/// `anchor_open_ended`: an anchor uses the end of a looping clip without a length (or of a track whose last
/// item is one).
fn open_ended(lw: &mut Lowerer<'_>) {
    let mut found = Vec::new();
    for anchor in &lw.anchors {
        if anchor.rule.mode != AnchorMode::After {
            continue;
        }
        let culprit = match anchor.rule.target {
            AnchorTarget::Node(m) => is_open_ended(lw, m).then(|| lw.nodes[m].id.clone()),
            AnchorTarget::Track(u) => track_anchor_node(lw, u, AnchorMode::After)
                .filter(|&m| is_open_ended(lw, m))
                .map(|m| lw.nodes[m].id.clone()),
        };
        if let Some(clip) = culprit {
            let target = &anchor.rule.target_id;
            let owner = owner_id(lw, anchor.owner);
            let what = if anchor.slot == Slot::End {
                "ends"
            } else {
                "starts"
            };
            let subject = if &clip == target {
                format!(
                    "`{target}` is a looping clip without a length, which plays until the end of the timeline"
                )
            } else {
                format!(
                    "track `{target}` ends with `{clip}`, a looping clip without a length that plays until the end of the timeline"
                )
            };
            found.push(
                Diagnostic::error(
                    codes::ANCHOR_OPEN_ENDED,
                    format!("`{owner}` {what} after the end of `{target}`, but {subject}"),
                )
                .with_path(anchor.path.clone())
                .with_hint(format!(
                    "Anchor to its start with `with_cue: {target}`, or give `{clip}` a length with `duration_ms`, \
                     `end_at_ms` or `end_relative`"
                ))
                .with_related([target.clone(), clip]),
            );
        }
    }
    lw.diags.extend(found);
}

/// Adjacency with an optional anchor index per edge (for blaming a field).
type Adjacency = Vec<Vec<(usize, Option<usize>)>>;

fn build_graph(lw: &Lowerer<'_>) -> Adjacency {
    let n_nodes = lw.nodes.len();
    let start = |n: NodeId| 2 * n;
    let end = |n: NodeId| 2 * n + 1;
    let track_v = |t: TrackIdx| 2 * n_nodes + t;
    let mut adj: Adjacency = vec![Vec::new(); 2 * n_nodes + lw.tracks.len()];

    for (n, node) in lw.nodes.iter().enumerate() {
        adj[end(n)].push((start(n), None));
        if node.start == StartRule::Sequential {
            let previous = lw.tracks.get(node.track).and_then(|t| {
                let pos = t.nodes.iter().position(|&m| m == n)?;
                pos.checked_sub(1).map(|p| t.nodes[p])
            });
            match previous {
                Some(p) => adj[start(n)].push((end(p), None)),
                None => adj[start(n)].push((track_v(node.track), None)),
            }
        }
    }
    for (k, anchor) in lw.anchors.iter().enumerate() {
        let from = match (anchor.owner, anchor.slot) {
            (Owner::Node(n, _), Slot::Start) => start(n),
            (Owner::Node(n, _), Slot::End) => end(n),
            (Owner::Track(t), _) => track_v(t),
        };
        if from >= adj.len() {
            continue;
        }
        let edge_of = |m: NodeId| {
            if anchor.rule.mode == AnchorMode::After {
                end(m)
            } else {
                start(m)
            }
        };
        let target = match anchor.rule.target {
            AnchorTarget::Node(m) => Some(m),
            AnchorTarget::Track(u) => track_anchor_node(lw, u, anchor.rule.mode),
        };
        if let Some(m) = target.filter(|&m| m < n_nodes) {
            adj[from].push((edge_of(m), Some(k)));
        }
    }
    for edges in &mut adj {
        edges.sort_unstable();
        edges.dedup();
    }
    adj
}

/// Strongly connected components (iterative Tarjan), each sorted, in discovery order.
fn strongly_connected(adj: &Adjacency) -> Vec<Vec<usize>> {
    const UNSEEN: usize = usize::MAX;
    let n = adj.len();
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut stack = Vec::new();
    let mut sccs = Vec::new();
    let mut next = 0usize;
    let mut calls: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if index[root] != UNSEEN {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        calls.push((root, 0));
        while let Some(&mut (v, ref mut edge)) = calls.last_mut() {
            if let Some(&(w, _)) = adj[v].get(*edge) {
                *edge += 1;
                if index[w] == UNSEEN {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    calls.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(parent, _)) = calls.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut component = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                component.sort_unstable();
                sccs.push(component);
            }
        }
    }
    sccs
}

/// A shortest cycle through `v0` inside `component`, as (vertex, anchor of the edge leaving it) pairs.
fn shortest_cycle(adj: &Adjacency, component: &[usize], v0: usize) -> Vec<(usize, Option<usize>)> {
    let members: HashSet<usize> = component.iter().copied().collect();
    let mut parent: Vec<Option<(usize, Option<usize>)>> = vec![None; adj.len()];
    let mut queue = VecDeque::new();
    for &(w, anchor) in &adj[v0] {
        if members.contains(&w) && parent[w].is_none() {
            parent[w] = Some((v0, anchor));
            queue.push_back(w);
        }
    }
    while let Some(v) = queue.pop_front() {
        if v == v0 {
            break;
        }
        for &(w, anchor) in &adj[v] {
            if members.contains(&w) && parent[w].is_none() {
                parent[w] = Some((v, anchor));
                queue.push_back(w);
            }
        }
    }
    // Walk back from v0 to v0.
    let mut path = Vec::new();
    let mut cur = v0;
    for _ in 0..adj.len() {
        let Some((prev, anchor)) = parent[cur] else {
            break;
        };
        path.push((prev, anchor));
        cur = prev;
        if cur == v0 {
            break;
        }
    }
    path.reverse();
    path
}

/// `dependency_cycle`: one diagnostic per strongly connected component.
fn cycles(lw: &mut Lowerer<'_>) {
    let adj = build_graph(lw);
    let n_nodes = lw.nodes.len();
    let label = |v: usize| -> String {
        if v < 2 * n_nodes {
            lw.nodes[v / 2].id.clone()
        } else {
            lw.tracks
                .get(v - 2 * n_nodes)
                .map(|t| t.id.clone())
                .unwrap_or_default()
        }
    };
    let mut found = Vec::new();
    for component in strongly_connected(&adj) {
        if component.len() < 2 {
            continue;
        }
        let v0 = component[0];
        let cycle = shortest_cycle(&adj, &component, v0);
        if cycle.is_empty() {
            continue;
        }
        // Steps reached through a track anchor show which item the anchor resolves to. `cycle[i].1` is the
        // anchor of the edge from `cycle[i]` to `cycle[i + 1]` (wrapping).
        let mut through_track: Vec<Option<TrackStep>> = vec![None; cycle.len()];
        for (i, &(_, anchor)) in cycle.iter().enumerate() {
            let Some(anchor) = anchor.and_then(|k| lw.anchors.get(k)) else {
                continue;
            };
            let AnchorTarget::Track(u) = anchor.rule.target else {
                continue;
            };
            let next = (i + 1) % cycle.len();
            let v = cycle[next].0;
            if v < 2 * n_nodes {
                through_track[next] = Some(TrackStep {
                    track: lw.tracks.get(u).map(|t| t.id.clone()).unwrap_or_default(),
                    item: lw.nodes[v / 2].id.clone(),
                    mode: anchor.rule.mode,
                });
            }
        }
        // (id, how the step is reached when that differs from the id), consecutive vertices of one node (its
        // start and end) collapsed. The first step is shown by its id (nothing leads to it in the message);
        // its own annotation, if any, goes to the closing step, which is the same item reached again.
        let mut chain: Vec<(String, Option<String>)> = Vec::new();
        for (i, &(v, _)) in cycle.iter().enumerate() {
            let id = label(v);
            let shown = through_track[i].as_ref().map(TrackStep::describe);
            match chain.last_mut() {
                Some((last, annotation)) if *last == id => {
                    if annotation.is_none() {
                        *annotation = shown;
                    }
                }
                _ => chain.push((id, shown)),
            }
        }
        // The closing step repeats the first item: drop it, keeping its annotation for the first step.
        if chain.len() > 1
            && chain.first().map(|c| &c.0) == chain.last().map(|c| &c.0)
            && let Some((_, annotation)) = chain.pop()
            && chain[0].1.is_none()
        {
            chain[0].1 = annotation;
        }
        let mut text: Vec<String> = chain
            .iter()
            .enumerate()
            .map(|(i, (id, annotation))| match annotation {
                Some(shown) if i > 0 => shown.clone(),
                _ => id.clone(),
            })
            .collect();
        if let Some((id, annotation)) = chain.first() {
            text.push(annotation.clone().unwrap_or_else(|| id.clone()));
        }
        let blame = cycle
            .iter()
            .find_map(|&(_, anchor)| anchor)
            .and_then(|k| lw.anchors.get(k));
        // A single-clip shorthand track is its clip: anchoring to it is not a step through a track anchor.
        let hint = match through_track.iter().flatten().find(|s| s.item != s.track) {
            Some(step) => format!(
                "Break the cycle: `{}: {}` follows the {} declared item of track `{}` (`{}`); anchor to a specific \
                 cue or clip outside the cycle instead, or give one of these items an absolute `start_at_ms`",
                step.key(),
                step.track,
                step.which(),
                step.track,
                step.item
            ),
            None => "Break the cycle: anchor one of these items to something outside the cycle, or give it an \
                     absolute `start_at_ms`"
                .to_string(),
        };
        // The cycle's items, then the tracks it passes through by a track anchor (`with_related` dedupes).
        let related: Vec<String> = chain
            .iter()
            .map(|(id, _)| id.clone())
            .chain(through_track.iter().flatten().map(|s| s.track.clone()))
            .collect();
        let mut d = Diagnostic::error(
            codes::DEPENDENCY_CYCLE,
            format!("dependency cycle: {}", text.join(" → ")),
        )
        .with_hint(hint)
        .with_related(related);
        if let Some(anchor) = blame {
            d = d.with_path(anchor.path.clone());
        }
        found.push(d);
    }
    lw.diags.extend(found);
}

/// A step of a cycle reached through a track anchor.
#[derive(Debug, Clone)]
struct TrackStep {
    track: String,
    /// The declared item the anchor resolves to.
    item: String,
    mode: AnchorMode,
}

impl TrackStep {
    fn which(&self) -> &'static str {
        if self.mode == AnchorMode::After {
            "last"
        } else {
            "first"
        }
    }

    fn key(&self) -> &'static str {
        match self.mode {
            AnchorMode::After => "after_cue",
            AnchorMode::With => "with_cue",
            AnchorMode::Before => "before_cue",
        }
    }

    /// `voice (track anchor: first item `hello`)`, or just the id for a single-clip shorthand track (whose
    /// clip is the track).
    fn describe(&self) -> String {
        if self.item == self.track {
            self.track.clone()
        } else {
            format!(
                "{} (track anchor: {} item `{}`)",
                self.track,
                self.which(),
                self.item
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tarjan_finds_cycles() {
        // 0 → 1 → 2 → 0, 3 → 0, 4 alone.
        let adj: Adjacency = vec![
            vec![(1, None)],
            vec![(2, None)],
            vec![(0, Some(7))],
            vec![(0, None)],
            vec![],
        ];
        let sccs = strongly_connected(&adj);
        let big: Vec<&Vec<usize>> = sccs.iter().filter(|c| c.len() > 1).collect();
        assert_eq!(big, [&vec![0, 1, 2]]);
        let cycle = shortest_cycle(&adj, big[0], 0);
        let vertices: Vec<usize> = cycle.iter().map(|(v, _)| *v).collect();
        assert_eq!(vertices, [0, 1, 2]);
        assert_eq!(cycle[2].1, Some(7));
    }

    #[test]
    fn deep_chains_do_not_overflow() {
        let n = 200_000;
        let adj: Adjacency = (0..n)
            .map(|i| {
                if i + 1 < n {
                    vec![(i + 1, None)]
                } else {
                    vec![]
                }
            })
            .collect();
        assert_eq!(strongly_connected(&adj).len(), n);
    }
}
