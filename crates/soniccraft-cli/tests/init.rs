//! `soniccraft init`: every template, asset generation, overwrite protection, `--list`, and the
//! init → validate (→ plan) round trip through the engine.

mod common;

use std::path::Path;

use common::{run, temp_dir};

const TEMPLATES: [&str; 6] = [
    "offline-demo",
    "offline-demo-mock",
    "tech-briefing",
    "two-person-podcast",
    "voiceover-with-music",
    "dialogue-crosstalk",
];

/// The assets each template references (relative to the project directory).
fn expected_assets(template: &str) -> &'static [&'static str] {
    const LOOP: &str = "assets/audio/synthwave_loop.wav";
    const WHOOSH: &str = "assets/sfx/whoosh.wav";
    const IMPACT: &str = "assets/sfx/impact.wav";
    const CHIME: &str = "assets/sfx/chime.wav";
    match template {
        "offline-demo" | "offline-demo-mock" => &[LOOP, WHOOSH, IMPACT, CHIME],
        "tech-briefing" => &[LOOP, WHOOSH],
        "two-person-podcast" => &[LOOP, CHIME],
        "voiceover-with-music" => &[LOOP, IMPACT],
        _ => &[],
    }
}

/// Every `.wav` below `dir`, relative and sorted.
fn wav_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "wav") {
                let rel = path
                    .strip_prefix(dir)
                    .expect("inside")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_template_writes_its_timeline_and_only_its_assets() {
    let root = temp_dir();
    for template in TEMPLATES {
        let r = run(root.path(), &["init", template, "--template", template]);
        r.expect_status(0);
        let dir = root.path().join(template);
        let text = std::fs::read_to_string(dir.join("timeline.yaml")).expect("timeline written");
        assert!(
            text.starts_with(&format!(
                "# Created by `soniccraft init --template {template}`."
            )),
            "{text}"
        );
        assert!(
            !text.contains("../"),
            "{template}: repository paths left:\n{text}"
        );
        let mut expected: Vec<String> = expected_assets(template)
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.sort();
        assert_eq!(wav_files(&dir), expected, "{template}");
        // next steps
        assert!(r.stdout.contains(&format!("cd {template}")), "{}", r.stdout);
        assert!(
            r.stdout.contains("soniccraft render timeline.yaml"),
            "{}",
            r.stdout
        );
        assert!(
            r.stdout.contains(&format!("from template `{template}`")),
            "{}",
            r.stdout
        );
    }
}

#[test]
fn default_template_renders_offline() {
    let root = temp_dir();
    let r = run(root.path(), &["init"]);
    r.expect_status(0);
    let text = std::fs::read_to_string(root.path().join("timeline.yaml")).expect("timeline");
    let expected = if cfg!(target_os = "macos") {
        "offline-demo"
    } else {
        "offline-demo-mock"
    };
    assert!(text.contains(&format!("--template {expected}`")), "{text}");
    // init in the current directory prints no `cd`
    assert!(!r.stdout.contains("  cd "), "{}", r.stdout);
}

#[test]
fn existing_files_are_kept_unless_forced() {
    let root = temp_dir();
    run(root.path(), &["init", "p", "--template", "tech-briefing"]).expect_status(0);
    let timeline = root.path().join("p/timeline.yaml");
    std::fs::write(&timeline, "edited\n").expect("edit");

    let again = run(root.path(), &["init", "p", "--template", "tech-briefing"]);
    again.expect_status(3);
    assert!(
        again.stderr.contains("refusing to overwrite"),
        "{}",
        again.stderr
    );
    assert!(again.stderr.contains("--force"), "{}", again.stderr);
    assert_eq!(
        std::fs::read_to_string(&timeline).expect("read"),
        "edited\n"
    );

    // Even one existing asset blocks the whole init (nothing is half-written).
    let other = root.path().join("q");
    std::fs::create_dir_all(other.join("assets/sfx")).expect("mkdir");
    std::fs::write(other.join("assets/sfx/whoosh.wav"), "mine").expect("write");
    let blocked = run(root.path(), &["init", "q", "--template", "tech-briefing"]);
    blocked.expect_status(3);
    assert!(!other.join("timeline.yaml").exists());
    assert_eq!(
        std::fs::read(other.join("assets/sfx/whoosh.wav")).expect("read"),
        b"mine"
    );

    let forced = run(
        root.path(),
        &["init", "p", "--template", "tech-briefing", "--force"],
    );
    forced.expect_status(0);
    assert!(
        std::fs::read_to_string(&timeline)
            .expect("read")
            .starts_with("# Created by")
    );
}

#[test]
fn no_assets_writes_only_the_timeline() {
    let root = temp_dir();
    let r = run(
        root.path(),
        &[
            "init",
            "x",
            "--template",
            "offline-demo-mock",
            "--no-assets",
        ],
    );
    r.expect_status(0);
    let dir = root.path().join("x");
    assert!(dir.join("timeline.yaml").is_file());
    assert!(!dir.join("assets").exists());
    assert!(r.stdout.contains("--no-assets"), "{}", r.stdout);
    let text = std::fs::read_to_string(dir.join("timeline.yaml")).expect("timeline");
    assert!(
        text.contains("No audio was written (--no-assets)"),
        "{text}"
    );
}

#[test]
fn quiet_init_prints_nothing() {
    let root = temp_dir();
    let r = run(root.path(), &["init", "x", "-q"]);
    r.expect_status(0);
    assert!(r.stdout.is_empty() && r.stderr.is_empty(), "{r:?}");
}

#[test]
fn list_shows_every_template() {
    let root = temp_dir();
    let r = run(root.path(), &["init", "--list"]);
    r.expect_status(0);
    for template in TEMPLATES {
        assert!(r.stdout.contains(template), "{}", r.stdout);
    }
    assert!(r.stdout.contains("(default)"), "{}", r.stdout);
    assert!(
        r.stdout.contains("needs: ELEVENLABS_API_KEY"),
        "{}",
        r.stdout
    );
    // nothing was written
    assert_eq!(std::fs::read_dir(root.path()).expect("read dir").count(), 0);
}

#[test]
fn generated_assets_are_byte_identical_across_runs() {
    let root = temp_dir();
    run(
        root.path(),
        &["init", "a", "--template", "offline-demo-mock"],
    )
    .expect_status(0);
    run(
        root.path(),
        &["init", "b", "--template", "offline-demo-mock"],
    )
    .expect_status(0);
    for asset in expected_assets("offline-demo-mock") {
        let a = std::fs::read(root.path().join("a").join(asset)).expect("read a");
        let b = std::fs::read(root.path().join("b").join(asset)).expect("read b");
        assert!(a == b, "{asset} differs between two init runs");
        assert!(
            a.starts_with(b"RIFF") && &a[8..12] == b"WAVE",
            "{asset} is not a WAV file"
        );
    }
}

#[test]
fn init_target_must_be_a_directory() {
    let root = temp_dir();
    std::fs::write(root.path().join("file"), "x").expect("write");
    let r = run(root.path(), &["init", "file"]);
    r.expect_status(3);
    assert!(r.stderr.contains("not a directory"), "{}", r.stderr);
}

/// Environment errors that depend on the machine (no API keys, no Kokoro server, no `say`).
const ENVIRONMENT_ERRORS: [&str; 2] = ["missing_credentials", "provider_unavailable"];

#[test]
fn init_then_validate_round_trip_for_every_template() {
    let root = temp_dir();
    for template in TEMPLATES {
        run(root.path(), &["init", template, "--template", template]).expect_status(0);
        let dir = root.path().join(template);
        let r = run(&dir, &["validate", "timeline.yaml", "--json"]);
        assert!(r.status == 0 || r.status == 1, "{template}: {r:?}");
        let report = r.json();
        let errors: Vec<&serde_json::Value> = report["diagnostics"]
            .as_array()
            .expect("diagnostics")
            .iter()
            .filter(|d| d["severity"] == "error")
            .collect();
        let strict = template == "offline-demo-mock"
            || (template == "offline-demo" && cfg!(target_os = "macos"));
        for e in &errors {
            let code = e["code"].as_str().unwrap_or_default();
            assert!(
                !strict && ENVIRONMENT_ERRORS.contains(&code),
                "{template}: unexpected error {e}"
            );
        }
        assert_eq!(
            r.status == 0,
            errors.is_empty(),
            "{template}: exit {}",
            r.status
        );
        if strict {
            assert_eq!(report["valid"], true, "{template}: {report}");
            let plan = run(&dir, &["plan", "timeline.yaml", "--json"]);
            plan.expect_status(0);
            let plan = plan.json();
            assert_eq!(plan["valid"], true, "{template}: {plan}");
            assert!(
                plan["items"].as_array().is_some_and(|i| !i.is_empty()),
                "{plan}"
            );
        }
    }
}
