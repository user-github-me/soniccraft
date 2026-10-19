//! Phase 4: rule combinations that cannot be honoured whatever the durations are. Each problem is reported
//! once here; the scheduling passes then apply a deterministic fallback silently.

use soniccraft_core::Diagnostic;
use soniccraft_core::diagnostics::codes;
use soniccraft_core::ir::{AnchorMode, NodeId};

use super::facts::Facts;
use super::rules::{Anchor, Length, Rules, Start};
use super::{Diags, Names};

/// Report `anchor_open_ended` (an `after_cue` anchor on, or a sequential item after, an open-ended clip) and
/// `conflicting_fields` (`before_cue` on an item whose length depends on its start).
pub(super) fn static_checks(names: Names<'_>, facts: &Facts, rules: &Rules, diags: &mut Diags) {
    let open_ended_anchor = |owner: &str, a: &Anchor, field: String, diags: &mut Diags| {
        if a.mode != AnchorMode::After {
            return;
        }
        let Some(end_node) = facts.end_node(a.target) else {
            return;
        };
        if !facts.is_open_ended(rules, end_node) {
            return;
        }
        diags.push(
            Diagnostic::error(
                codes::ANCHOR_OPEN_ENDED,
                format!(
                    "`{owner}` is anchored to the end of `{}`, which loops until the timeline end (open-ended), so \
                     that end is unknown; the anchor uses its start instead",
                    a.target_id
                ),
            )
            .with_path(field)
            .with_hint(format!(
                "anchor to its start with `with_cue: {}`, or give `{}` a `duration_ms`, `end_at_ms` or \
                 `end_relative`",
                a.target_id,
                names.id(end_node)
            ))
            .with_related([owner.to_string(), a.target_id.clone()]),
        );
    };
    let before_conflict = |v: NodeId, field: String, diags: &mut Diags| {
        let what = if facts.is_open_ended(rules, v) {
            "loops without a length (open-ended)"
        } else {
            "also sets `end_at_ms`/`end_relative`"
        };
        diags.push(
            Diagnostic::error(
                codes::CONFLICTING_FIELDS,
                format!(
                    "`{}` is placed with `before_cue`, which fixes where it ends, but it {what}; its start was \
                     anchored to the referenced start instead",
                    names.id(v)
                ),
            )
            .with_path(field)
            .with_hint("give the clip a `duration_ms`, or anchor its start with `after_cue`/`with_cue`")
            .with_related([names.id(v)]),
        );
    };

    for (v, fact) in facts.nodes.iter().enumerate() {
        if let Start::Relative(a) = &rules.start[v] {
            if a.mode == AnchorMode::Before && facts.independent_length(rules, v).is_none() {
                before_conflict(v, names.field(v, "start_relative"), diags);
            }
            open_ended_anchor(names.id(v), a, names.field(v, "start_relative"), diags);
        }
        if let Length::EndRelative(a) = &rules.length[v] {
            open_ended_anchor(names.id(v), a, names.field(v, "end_relative"), diags);
        }
        let Some(t) = fact.track else { continue };
        if fact.pos > 0 && facts.is_sequential(rules, v) {
            let prev = facts.members[t][fact.pos - 1];
            if facts.is_open_ended(rules, prev) {
                diags.push(
                    Diagnostic::error(
                        codes::ANCHOR_OPEN_ENDED,
                        format!(
                            "`{}` follows `{}`, which loops until the timeline end (open-ended), so it has no start; \
                             it was placed at the start of `{}`",
                            names.id(v),
                            names.id(prev),
                            names.id(prev)
                        ),
                    )
                    .with_path(names.path(v))
                    .with_hint(format!(
                        "give `{}` a `duration_ms`, `end_at_ms` or `end_relative`, or give `{}` a \
                         `start_at_ms`/`start_relative`",
                        names.id(prev),
                        names.id(v)
                    ))
                    .with_related([names.id(v), names.id(prev)]),
                );
            }
        }
    }

    for (t, list) in facts.members.iter().enumerate() {
        let Start::Relative(a) = &rules.track_start[t] else {
            continue;
        };
        let Some(&first) = list.first() else { continue };
        if !facts.is_sequential(rules, first) {
            // The first item has its own start; the track start is not used.
            continue;
        }
        if a.mode == AnchorMode::Before && facts.independent_length(rules, first).is_none() {
            before_conflict(first, names.track_field(t, "start_relative"), diags);
        }
        open_ended_anchor(
            names.track_id(t),
            a,
            names.track_field(t, "start_relative"),
            diags,
        );
    }
}
