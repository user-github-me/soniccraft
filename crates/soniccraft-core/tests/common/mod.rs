//! Helpers shared by the integration tests.

#![allow(dead_code)]

use std::path::PathBuf;

use soniccraft_core::{Compiled, Diagnostic, LowerOptions, SourceFormat, compile};

/// Compile YAML text with default options.
pub fn yaml(text: &str) -> Compiled {
    compile(text, SourceFormat::Yaml, &LowerOptions::default())
}

/// Compile JSON text with default options.
pub fn json(text: &str) -> Compiled {
    compile(text, SourceFormat::Json, &LowerOptions::default())
}

/// Compile YAML text with options.
pub fn yaml_with(text: &str, options: &LowerOptions) -> Compiled {
    compile(text, SourceFormat::Yaml, options)
}

/// Every diagnostic, one per line, for assertion messages.
pub fn dump(c: &Compiled) -> String {
    c.diagnostics
        .iter()
        .map(|d| format!("  {d} [{:?}:{:?}]", d.line, d.column))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The diagnostic with `code` at `path`; panics with all diagnostics otherwise.
pub fn find<'a>(c: &'a Compiled, code: &str, path: &str) -> &'a Diagnostic {
    c.diagnostics
        .iter()
        .find(|d| d.code == code && d.path.as_deref() == Some(path))
        .unwrap_or_else(|| panic!("no `{code}` at `{path}`; diagnostics:\n{}", dump(c)))
}

/// Assert a diagnostic exists and its hint contains every needle; returns it.
pub fn expect<'a>(
    c: &'a Compiled,
    code: &str,
    path: &str,
    hint_needles: &[&str],
) -> &'a Diagnostic {
    let d = find(c, code, path);
    let hint = d
        .hint
        .as_deref()
        .unwrap_or_else(|| panic!("`{code}` at `{path}` has no hint"));
    for needle in hint_needles {
        assert!(
            hint.contains(needle),
            "hint of `{code}` at `{path}` lacks {needle:?}: {hint}"
        );
    }
    d
}

/// Codes of the error diagnostics, in report order.
pub fn error_codes(c: &Compiled) -> Vec<&str> {
    c.diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code.as_str())
        .collect()
}

/// Number of diagnostics with `code`.
pub fn count(c: &Compiled, code: &str) -> usize {
    c.diagnostics.iter().filter(|d| d.code == code).count()
}

/// Assert there are no errors.
pub fn assert_clean(c: &Compiled) {
    assert!(
        !soniccraft_core::diagnostics::has_errors(&c.diagnostics),
        "unexpected errors:\n{}",
        dump(c)
    );
    assert!(c.program.is_some());
}

/// Path of a fixture file.
pub fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Read a fixture file.
pub fn read_fixture(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// A minimal valid timeline: one speech track `voice` with cue `cue_01` (provider mock).
pub const BASE: &str = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - id: cue_01
        text: "Hello there."
        provider: mock
"#;
