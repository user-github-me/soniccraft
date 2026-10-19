//! Starter timelines for `soniccraft init`.
//!
//! The templates are vendored copies of the Agent Skill templates
//! (`skills/soniccraft/reference/templates/*.yaml`) and of the mock-only offline demo
//! (`examples/offline-demo/timeline.mock.yaml`), so the crate stays publishable; a test keeps each copy
//! byte-identical to its canonical file.
//!
//! Their `source:` paths are relative to the timeline (`assets/<audio|sfx>/<file>`), naming assets that
//! [`crate::assets`] can generate. [`instantiate`] normalizes every such path to `assets/...` (line by
//! line, so comments and layout survive; a path with leading directories such as `../x/assets/sfx/a.wav`
//! is cut back to `assets/sfx/a.wav`), replaces the header paragraphs that talk about the repository with
//! one that says how the file was created, and reports which assets the timeline needs.

use crate::assets::Asset;

/// One starter timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Template {
    /// `--template` value.
    pub(crate) name: &'static str,
    /// One-line description for `init --list`.
    pub(crate) summary: &'static str,
    /// What rendering it needs besides soniccraft.
    pub(crate) needs: &'static str,
    /// The vendored text.
    pub(crate) text: &'static str,
    /// The canonical file, relative to the workspace root (kept identical by a test).
    pub(crate) canonical: &'static str,
}

/// Every template, in `init --list` order.
pub(crate) const TEMPLATES: [Template; 6] = [
    Template {
        name: "offline-demo",
        summary: "README exchange over a ducked synth bed with SFX; macOS `say` voices plus the mock voice",
        needs: "macOS (the `say` voices Samantha and Daniel)",
        text: include_str!("../templates/offline-demo.yaml"),
        canonical: "skills/soniccraft/reference/templates/offline-demo.yaml",
    },
    Template {
        name: "offline-demo-mock",
        summary: "the offline demo with the deterministic mock voice only: renders anywhere",
        needs: "nothing",
        text: include_str!("../templates/offline-demo-mock.yaml"),
        canonical: "examples/offline-demo/timeline.mock.yaml",
    },
    Template {
        name: "tech-briefing",
        summary: "the README timeline: ElevenLabs narrator, Kokoro guest, ducked bed, overlap push",
        needs: "ELEVENLABS_API_KEY and a Kokoro-FastAPI server",
        text: include_str!("../templates/tech-briefing.yaml"),
        canonical: "skills/soniccraft/reference/templates/tech-briefing.yaml",
    },
    Template {
        name: "two-person-podcast",
        summary: "host and guest taking turns on one track, intro/outro music beds and chimes, MP3",
        needs: "ELEVENLABS_API_KEY",
        text: include_str!("../templates/two-person-podcast.yaml"),
        canonical: "skills/soniccraft/reference/templates/two-person-podcast.yaml",
    },
    Template {
        name: "voiceover-with-music",
        summary: "one narrator over a ducked music bed with a cold-open hit, streaming loudness",
        needs: "ELEVENLABS_API_KEY",
        text: include_str!("../templates/voiceover-with-music.yaml"),
        canonical: "skills/soniccraft/reference/templates/voiceover-with-music.yaml",
    },
    Template {
        name: "dialogue-crosstalk",
        summary: "two panned voices with interruptions and a backchannel (no audio assets)",
        needs: "a Kokoro-FastAPI server",
        text: include_str!("../templates/dialogue-crosstalk.yaml"),
        canonical: "skills/soniccraft/reference/templates/dialogue-crosstalk.yaml",
    },
];

/// The `--template` values.
pub(crate) const NAMES: [&str; 6] = [
    TEMPLATES[0].name,
    TEMPLATES[1].name,
    TEMPLATES[2].name,
    TEMPLATES[3].name,
    TEMPLATES[4].name,
    TEMPLATES[5].name,
];

/// The template used without `--template`: the `say` demo on macOS, the mock-only demo elsewhere, so
/// `soniccraft init && soniccraft render timeline.yaml` works offline out of the box everywhere.
pub(crate) fn default_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "offline-demo"
    } else {
        "offline-demo-mock"
    }
}

/// Look a template up by name.
pub(crate) fn find(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

/// A template made concrete for a new project directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instance {
    /// The timeline text to write.
    pub(crate) text: String,
    /// Assets the timeline references, in first-use order, without duplicates.
    pub(crate) assets: Vec<Asset>,
}

/// Header paragraphs containing any of these talk about the repository layout and are replaced.
const REPO_MARKERS: [&str; 5] = [
    "yaml-language-server",
    "SonicCraft repository",
    "examples/",
    "Maintainers",
    "sync-templates.sh",
];

/// Repository-specific phrases inside kept paragraphs, and their replacement in a new project.
const PHRASE_FIXUPS: [(&str, &str); 4] = [
    (
        "use timeline.mock.yaml (same timeline, mock voices only)",
        "use the `offline-demo-mock` template (same timeline, mock voices only)",
    ),
    (
        "Mock-only twin of timeline.yaml: every voice",
        "Mock-only variant of the offline demo: every voice",
    ),
    (" CI renders this file on Linux and macOS.", ""),
    (
        "(see reference/providers.md)",
        "(`soniccraft voices <provider>` lists voice ids)",
    ),
];

/// Make `template` concrete: `file_name` is the timeline's file name (it replaces `<this file>` in the
/// usage comments) and `with_assets` says whether `init` writes the referenced audio files.
pub(crate) fn instantiate(
    template: &Template,
    file_name: &str,
    with_assets: bool,
) -> Result<Instance, String> {
    let lines: Vec<&str> = template.text.lines().collect();
    let header_len = lines
        .iter()
        .take_while(|l| l.trim_start().starts_with('#'))
        .count();

    // Body first: it decides which assets exist.
    let mut assets = Vec::new();
    let mut body = Vec::with_capacity(lines.len() - header_len);
    for line in &lines[header_len..] {
        let (rewritten, path) = rewrite_source_line(line);
        if let Some(path) = path {
            let asset = Asset::from_relative_path(&path).ok_or_else(|| {
                format!(
                    "template `{}` references `{path}`, which `soniccraft init` cannot generate",
                    template.name
                )
            })?;
            if !assets.contains(&asset) {
                assets.push(asset);
            }
        }
        body.push(rewritten);
    }

    // Header: drop repository paragraphs, fix phrases, prepend how the file was made.
    let mut paragraphs: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in &lines[..header_len] {
        if line.trim() == "#" {
            if !current.is_empty() {
                paragraphs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(fix_phrases(line, file_name));
        }
    }
    if !current.is_empty() {
        paragraphs.push(current);
    }
    paragraphs.retain(|p| !p.iter().any(|l| REPO_MARKERS.iter().any(|m| l.contains(m))));
    paragraphs.insert(0, provenance(template.name, &assets, with_assets));

    let mut text = String::with_capacity(template.text.len() + 256);
    for (i, paragraph) in paragraphs.iter().enumerate() {
        if i > 0 {
            text.push_str("#\n");
        }
        for line in paragraph {
            text.push_str(line);
            text.push('\n');
        }
    }
    for line in body {
        text.push_str(&line);
        text.push('\n');
    }
    Ok(Instance { text, assets })
}

/// The first header paragraph of an instantiated template.
fn provenance(name: &str, assets: &[Asset], with_assets: bool) -> Vec<String> {
    let mut lines = vec![format!("# Created by `soniccraft init --template {name}`.")];
    if assets.is_empty() {
        return lines;
    }
    let explanation: [&str; 2] = if with_assets {
        [
            "# The audio under assets/ was generated by soniccraft init: royalty-free, procedurally",
            "# synthesized stand-ins. Replace them with your own files, or point `source:` elsewhere.",
        ]
    } else {
        [
            "# No audio was written (--no-assets): put your own files at the `source:` paths below,",
            "# or point each `source:` at another audio file (or delete the music/sfx tracks).",
        ]
    };
    lines.extend(explanation.iter().map(|l| (*l).to_string()));
    lines
}

/// Apply [`PHRASE_FIXUPS`] and replace `<this file>` with `file_name`. When the name is longer than the
/// placeholder, the next run of two or more spaces (the one aligning a trailing comment) gives up the
/// difference, so comment columns stay aligned.
fn fix_phrases(line: &str, file_name: &str) -> String {
    const PLACEHOLDER: &str = "<this file>";
    let mut out = line.to_string();
    for (from, to) in PHRASE_FIXUPS {
        out = out.replace(from, to);
    }
    while let Some(at) = out.find(PLACEHOLDER) {
        let after = at + PLACEHOLDER.len();
        let mut tail = out[after..].to_string();
        let grow = file_name.chars().count().saturating_sub(PLACEHOLDER.len());
        if grow > 0
            && let Some(run_start) = tail.find("  ")
        {
            let run_len = tail[run_start..].chars().take_while(|&c| c == ' ').count();
            let keep = run_len.saturating_sub(grow).max(2);
            tail.replace_range(run_start..run_start + run_len, &" ".repeat(keep));
        }
        out = format!("{}{file_name}{tail}", &out[..at]);
    }
    out
}

/// Rewrite the asset path of a `source:` line (`source: x`, `- source: "x"`, with an optional trailing
/// comment) to `assets/<…>`, returning the new line and the new path. Other lines are returned unchanged.
fn rewrite_source_line(line: &str) -> (String, Option<String>) {
    let indent = line.len() - line.trim_start().len();
    let mut rest = &line[indent..];
    let mut prefix_len = indent;
    while let Some(after) = rest.strip_prefix("- ") {
        let skipped = rest.len() - after.trim_start().len();
        prefix_len += skipped;
        rest = after.trim_start();
    }
    let Some(value) = rest.strip_prefix("source:") else {
        return (line.to_string(), None);
    };
    let value_start = prefix_len + "source:".len() + (value.len() - value.trim_start().len());
    let value = value.trim_start();
    let (path_start, path) = match value.chars().next() {
        Some(q @ ('"' | '\'')) => match value[1..].find(q) {
            Some(end) => (value_start + 1, &value[1..1 + end]),
            None => return (line.to_string(), None),
        },
        Some(_) => {
            let end = value.find(" #").unwrap_or(value.len());
            (value_start, value[..end].trim_end())
        }
        None => return (line.to_string(), None),
    };
    let Some(at) = path.rfind("assets/") else {
        return (line.to_string(), None);
    };
    // Only a whole path component counts (`myassets/` does not).
    if at > 0 && !path[..at].ends_with('/') {
        return (line.to_string(), None);
    }
    let new_path = path[at..].to_string();
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..path_start]);
    out.push_str(&new_path);
    out.push_str(&line[path_start + path.len()..]);
    (out, Some(new_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn vendored_copies_match_their_canonical_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for t in &TEMPLATES {
            let canonical = root.join(t.canonical);
            // Outside the repository (crates.io package) there is nothing to compare against.
            let Ok(expected) = std::fs::read_to_string(&canonical) else {
                continue;
            };
            assert!(
                expected == t.text,
                "crates/soniccraft-cli/templates/{}.yaml is out of date; run:\n  cp {} crates/soniccraft-cli/templates/{}.yaml",
                t.name,
                t.canonical,
                t.name
            );
        }
    }

    #[test]
    fn names_and_defaults() {
        assert_eq!(NAMES.len(), TEMPLATES.len());
        for (n, t) in NAMES.iter().zip(&TEMPLATES) {
            assert_eq!(*n, t.name);
            assert_eq!(find(n), Some(t));
        }
        assert!(find(default_name()).is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn every_template_instantiates_cleanly() {
        for t in &TEMPLATES {
            for with_assets in [true, false] {
                let inst = instantiate(t, "timeline.yaml", with_assets).expect("instantiate");
                assert!(
                    inst.text
                        .starts_with("# Created by `soniccraft init --template "),
                    "{}",
                    t.name
                );
                for bad in [
                    "../",
                    "examples/",
                    "Maintainers",
                    "<this file>",
                    "yaml-language-server",
                    "timeline.mock.yaml",
                    "CI renders",
                    "reference/providers.md",
                ] {
                    assert!(
                        !inst.text.contains(bad),
                        "{}: `{bad}` left in\n{}",
                        t.name,
                        inst.text
                    );
                }
                // The body (everything from `version:`) is unchanged apart from `source:` paths.
                let original_body: Vec<&str> =
                    t.text.lines().skip_while(|l| l.starts_with('#')).collect();
                let new_body: Vec<&str> = inst
                    .text
                    .lines()
                    .skip_while(|l| l.starts_with('#'))
                    .collect();
                assert_eq!(original_body.len(), new_body.len(), "{}", t.name);
                for (a, b) in original_body.iter().zip(&new_body) {
                    if a != b {
                        assert!(
                            a.contains("source:") && b.contains("source:") && b.contains("assets/"),
                            "{a} -> {b}"
                        );
                    }
                }
                // It still parses and lowers (assets are checked by the engine, not here).
                let compiled = soniccraft_core::compile(
                    &inst.text,
                    soniccraft_core::SourceFormat::Yaml,
                    &soniccraft_core::LowerOptions::default(),
                );
                let errors: Vec<_> = compiled
                    .diagnostics
                    .iter()
                    .filter(|d| d.is_error())
                    .collect();
                assert!(errors.is_empty(), "{}: {errors:?}", t.name);
            }
        }
    }

    #[test]
    fn assets_per_template() {
        let assets = |name: &str| {
            find(name)
                .map(|t| {
                    instantiate(t, "timeline.yaml", true)
                        .expect("instantiate")
                        .assets
                })
                .unwrap_or_default()
        };
        use Asset::*;
        assert_eq!(
            assets("offline-demo"),
            [SynthwaveLoop, Whoosh, Impact, Chime]
        );
        assert_eq!(
            assets("offline-demo-mock"),
            [SynthwaveLoop, Whoosh, Impact, Chime]
        );
        assert_eq!(assets("tech-briefing"), [SynthwaveLoop, Whoosh]);
        assert_eq!(assets("two-person-podcast"), [SynthwaveLoop, Chime]);
        assert_eq!(assets("voiceover-with-music"), [SynthwaveLoop, Impact]);
        assert!(assets("dialogue-crosstalk").is_empty());
    }

    #[test]
    fn source_lines() {
        let cases = [
            (
                "    source: ../../../../examples/offline-demo/assets/audio/synthwave_loop.wav",
                "    source: assets/audio/synthwave_loop.wav",
            ),
            (
                "    source: \"../../../../examples/tech-briefing/assets/sfx/whoosh.wav\" # hit",
                "    source: \"assets/sfx/whoosh.wav\" # hit",
            ),
            (
                "      - source: '../x/assets/sfx/chime.wav'",
                "      - source: 'assets/sfx/chime.wav'",
            ),
            (
                "    source: assets/sfx/impact.wav",
                "    source: assets/sfx/impact.wav",
            ),
        ];
        for (input, expected) in cases {
            let (out, path) = rewrite_source_line(input);
            assert_eq!(out, expected);
            assert!(path.is_some_and(|p| p.starts_with("assets/")));
        }
        for unchanged in [
            "    text: \"source: assets/x.wav\"",
            "    source: myassets/x.wav",
            "    source: bed.wav",
            "# source: ../assets/x.wav",
        ] {
            assert_eq!(
                rewrite_source_line(unchanged),
                (unchanged.to_string(), None)
            );
        }
    }

    #[test]
    fn placeholder_keeps_comment_columns() {
        for line in [
            "#   soniccraft plan   <this file>              # dry run",
            "#   soniccraft render <this file> --preview    # offline-demo.wav + subtitles",
        ] {
            let fixed = fix_phrases(line, "timeline.yaml");
            assert!(fixed.contains("timeline.yaml"), "{fixed}");
            assert_eq!(fixed.rfind("# "), line.rfind("# "), "{line} -> {fixed}");
        }
        assert_eq!(
            fix_phrases(
                "#   soniccraft plan   <this file>              # dry run",
                "timeline.yaml"
            ),
            "#   soniccraft plan   timeline.yaml            # dry run"
        );
        assert_eq!(fix_phrases("x <this file>", "a.yaml"), "x a.yaml");
        assert_eq!(fix_phrases("x <this file> y", "t.yaml"), "x t.yaml y");
    }

    #[test]
    fn provenance_paragraph() {
        let t = find("offline-demo").expect("template");
        let with = instantiate(t, "timeline.yaml", true).expect("instantiate");
        assert!(with.text.contains("generated by soniccraft init"));
        for line in with.text.lines().take_while(|l| l.starts_with('#')) {
            assert!(line.chars().count() <= 110, "header line too long: {line}");
        }
        let without = instantiate(t, "timeline.yaml", false).expect("instantiate");
        assert!(without.text.contains("--no-assets"));
        let crosstalk = instantiate(
            find("dialogue-crosstalk").expect("template"),
            "t.yaml",
            true,
        )
        .expect("instantiate");
        assert!(crosstalk.text.starts_with("# Created by `soniccraft init --template dialogue-crosstalk`.\n#\n# SonicCraft template \"dialogue-crosstalk\""));
    }
}
