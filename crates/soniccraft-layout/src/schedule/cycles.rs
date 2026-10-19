//! Phase 3 (defensive): dependency cycles never make the layout loop. Lowering rejects them, but if one
//! reaches the layout it is reported (`dependency_cycle`, or `self_reference` for a one-node cycle) and broken
//! by demoting a single anchor to sequential placement.
//!
//! Cycles are searched on the start/end vertex graph ([`super::rules::Vertex`]), like core's: a clip's end may
//! wait for items that are anchored to its start, which is not a cycle.

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::codes;

use super::facts::Facts;
use super::rules::{Edge, Graph, Length, Rules, Start, Vertex, labeled_deps, node_of};
use super::{Diags, Names};

/// Report every dependency cycle and demote one anchor per cycle until the graph is acyclic. Each round
/// removes at least one anchor edge, so this terminates; the round limit is a second safety net.
pub(super) fn break_cycles(names: Names<'_>, facts: &Facts, rules: &mut Rules, diags: &mut Diags) {
    let n = facts.nodes.len();
    let max_rounds = 3 * n + facts.members.len() + 1;
    for _ in 0..max_rounds {
        let stuck = Graph::new(facts, rules).stuck();
        let Some(first) = stuck.iter().position(|&s| s) else {
            return;
        };
        let demoted = match find_cycle(facts, rules, &stuck, first) {
            Some(cycle) => demote_cycle(names, facts, rules, &cycle, diags),
            None => false,
        };
        if !demoted {
            // Unreachable for well-formed graphs (every cycle has an anchor edge); guarantee progress anyway.
            demote_all(names, facts, rules, &stuck, diags);
        }
    }
}

/// Walk from `first` along unresolved dependencies until a vertex repeats; returns the cycle as
/// `(vertex, edge, dependency)` steps. Every stuck vertex has a stuck dependency, so the walk cannot dead-end.
fn find_cycle(
    facts: &Facts,
    rules: &Rules,
    stuck: &[bool],
    first: Vertex,
) -> Option<Vec<(Vertex, Edge, Vertex)>> {
    let mut index = vec![usize::MAX; stuck.len()];
    let mut walk: Vec<(Vertex, Edge, Vertex)> = Vec::new();
    let mut x = first;
    loop {
        if index[x] != usize::MAX {
            return Some(walk.split_off(index[x]));
        }
        index[x] = walk.len();
        let (w, edge) = labeled_deps(facts, rules, x)
            .into_iter()
            .filter(|&(w, _)| stuck[w])
            .min()?;
        walk.push((x, edge, w));
        x = w;
    }
}

/// The node ids along a cycle, without consecutive repeats (a node's start and end are one item for the
/// reader), also across the wrap-around.
fn node_chain(cycle: &[(Vertex, Edge, Vertex)]) -> Vec<usize> {
    let mut chain: Vec<usize> = Vec::new();
    for step in cycle {
        let v = node_of(step.0);
        if chain.last() != Some(&v) {
            chain.push(v);
        }
    }
    if chain.len() > 1 && chain.first() == chain.last() {
        chain.pop();
    }
    chain
}

/// Break `cycle` with the smallest change: rotate it to start at its smallest vertex (the smallest node id),
/// then demote the anchor of the first step that is an anchor edge (sequential steps and a node's own
/// start-to-end step cannot be demoted). Every other item keeps its rule. Returns false when nothing could be
/// demoted.
fn demote_cycle(
    names: Names<'_>,
    facts: &Facts,
    rules: &mut Rules,
    cycle: &[(Vertex, Edge, Vertex)],
    diags: &mut Diags,
) -> bool {
    let first = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, step)| step.0)
        .map_or(0, |(i, _)| i);
    let ordered: Vec<(Vertex, Edge, Vertex)> = cycle[first..]
        .iter()
        .chain(&cycle[..first])
        .copied()
        .collect();
    let mut demoted: Option<(usize, Vec<(String, Edge)>)> = None;
    for &(x, _, w) in &ordered {
        let mut fields = Vec::new();
        for (dep, edge) in labeled_deps(facts, rules, x) {
            if dep == w
                && let Some(field) = demote_edge(names, facts, rules, node_of(x), edge)
            {
                fields.push((field, edge));
            }
        }
        if !fields.is_empty() {
            demoted = Some((node_of(x), fields));
            break;
        }
    }
    let Some((u, fields)) = demoted else {
        return false;
    };

    let fields_text = fields
        .iter()
        .map(|(f, _)| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let consequence = fields
        .iter()
        .map(|(_, edge)| match edge {
            Edge::Length => format!("`{}` plays its natural length", names.id(u)),
            Edge::TrackStart => {
                let track = facts.nodes[u].track.map_or("?", |t| names.track_id(t));
                format!("track `{track}` starts at 0 s")
            }
            Edge::Start | Edge::Sequential | Edge::Own => {
                format!("`{}` is placed sequentially", names.id(u))
            }
        })
        .collect::<Vec<_>>()
        .join(" and ");
    let chain = node_chain(&ordered);
    let mut related: Vec<String> = Vec::new();
    for &v in &chain {
        let id = names.id(v).to_string();
        if !related.contains(&id) {
            related.push(id);
        }
    }
    let diagnostic = if chain.len() == 1 {
        Diagnostic::error(
            codes::SELF_REFERENCE,
            format!(
                "`{}` is anchored to itself; the layout ignored {fields_text} ({consequence})",
                names.id(u)
            ),
        )
        .with_hint(format!(
            "anchor `{}` to a different cue, clip or track",
            names.id(u)
        ))
    } else {
        let text = chain
            .iter()
            .chain(chain.first())
            .map(|&v| format!("`{}`", names.id(v)))
            .collect::<Vec<_>>()
            .join(" → ");
        Diagnostic::error(
            codes::DEPENDENCY_CYCLE,
            format!("dependency cycle {text}; the layout ignored {fields_text} to break it ({consequence})"),
        )
        .with_hint("anchor one of these items to an item outside the cycle, or give it an absolute `start_at_ms`")
    };
    diags.push(
        diagnostic
            .with_path(fields[0].0.clone())
            .with_related(related),
    );
    true
}

/// Demote one anchor edge of node `u`; returns the diagnostic path of the ignored field.
fn demote_edge(
    names: Names<'_>,
    facts: &Facts,
    rules: &mut Rules,
    u: usize,
    edge: Edge,
) -> Option<String> {
    match edge {
        Edge::Sequential | Edge::Own => None,
        Edge::Start => matches!(rules.start[u], Start::Relative(_)).then(|| {
            rules.start[u] = Start::Sequential;
            names.field(u, "start_relative")
        }),
        Edge::Length => matches!(rules.length[u], Length::EndRelative(_)).then(|| {
            rules.length[u] = Length::NaturalFinite;
            names.field(u, "end_relative")
        }),
        Edge::TrackStart => {
            let t = facts.nodes[u].track?;
            matches!(rules.track_start[t], Start::Relative(_)).then(|| {
                rules.track_start[t] = Start::Sequential;
                names.track_field(t, "start_relative")
            })
        }
    }
}

/// Last-resort progress guarantee: demote every anchor of every node with a stuck vertex.
fn demote_all(
    names: Names<'_>,
    facts: &Facts,
    rules: &mut Rules,
    stuck: &[bool],
    diags: &mut Diags,
) {
    let mut fields = Vec::new();
    let mut related: Vec<String> = Vec::new();
    for x in (0..stuck.len()).filter(|&x| stuck[x]) {
        let v = node_of(x);
        // Both vertices of a node are adjacent, so a node is listed once.
        if x % 2 == 0 || !stuck[x - 1] {
            related.push(names.id(v).to_string());
        }
        for edge in [Edge::Start, Edge::Length, Edge::TrackStart] {
            if let Some(field) = demote_edge(names, facts, rules, v, edge) {
                fields.push(field);
            }
        }
    }
    let first = fields.first().cloned().unwrap_or_default();
    diags.push(
        Diagnostic::error(
            codes::DEPENDENCY_CYCLE,
            "items depend on each other in a cycle; the layout ignored their anchors and placed them sequentially",
        )
        .with_path(first)
        .with_hint("anchor one of these items to an item outside the cycle, or give it an absolute `start_at_ms`")
        .with_related(related),
    );
}
