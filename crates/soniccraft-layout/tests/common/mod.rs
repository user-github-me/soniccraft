//! Helpers shared by the integration tests.
#![allow(dead_code)] // each test crate uses a different subset

use soniccraft_core::{Diagnostic, Severity};
use soniccraft_layout::test_support::TestTimeline;
use soniccraft_layout::{Layout, Placement};

/// The placement of the node with this id.
pub fn at<'a>(timeline: &TestTimeline, layout: &'a Layout, id: &str) -> &'a Placement {
    &layout.placements[timeline.node(id)]
}

/// `(start, end)` in frames of the node with this id.
pub fn span(timeline: &TestTimeline, layout: &Layout, id: &str) -> (i64, i64) {
    let p = at(timeline, layout, id);
    (p.start, p.end())
}

/// All diagnostics with this code.
pub fn with_code<'a>(layout: &'a Layout, code: &str) -> Vec<&'a Diagnostic> {
    layout
        .diagnostics
        .iter()
        .filter(|d| d.code == code)
        .collect()
}

/// The only diagnostic with this code.
pub fn single<'a>(layout: &'a Layout, code: &str) -> &'a Diagnostic {
    let found = with_code(layout, code);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one `{code}`, got: {:#?}",
        layout.diagnostics
    );
    found[0]
}

/// Codes of all diagnostics, in order.
pub fn codes(layout: &Layout) -> Vec<&str> {
    layout.diagnostics.iter().map(|d| d.code.as_str()).collect()
}

/// Assert there are no warnings or errors.
pub fn assert_clean(layout: &Layout) {
    let bad: Vec<&Diagnostic> = layout
        .diagnostics
        .iter()
        .filter(|d| d.severity != Severity::Info)
        .collect();
    assert!(bad.is_empty(), "unexpected warnings/errors: {bad:#?}");
}

/// Assert `text` contains every needle.
pub fn assert_contains(text: &str, needles: &[&str]) {
    for needle in needles {
        assert!(text.contains(needle), "expected {needle:?} in:\n{text}");
    }
}
