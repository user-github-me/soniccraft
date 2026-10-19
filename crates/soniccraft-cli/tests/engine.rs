//! End-to-end tests through the engine: validate / plan / render / providers / voices / cache with the
//! offline `mock` provider, rustc-style diagnostics (snapshots), `--json` documents and exit statuses.

mod common;

use std::path::{Path, PathBuf};

use common::{Run, repo_root, run, run_with_stdin, temp_dir};
use serde_json::Value;

/// The errors of [`BROKEN`]: five from core (invalid values, unknown references, a misplaced field) and
/// the engine's `asset_not_found` for the bed (environment checks run on invalid documents too).
const BROKEN_ERRORS: usize = 6;

/// Five independent errors from core: invalid values, unknown references, a misplaced field.
const BROKEN: &str = r#"version: "1.0"
output:
  format: flack
  export_subtitles: [vtt, srtt]
speakers:
  host: {provider: mock, voice_id: host, label: Host}
tracks:
  - id: bed
    type: music
    source: assets/missing.wav
    loop: true
    fade_out_ms: 2000
    ducking: {trigger_tracks: [voice, voise], reduction_db: -15}
  - id: voice
    type: speech
    speaker: host
    sequence:
      - id: cue_01
        text: "Hello there."
      - id: cue_02
        start_relative: {after_cue: cue_9, offset_ms: 150}
        text: "General Kenobi."
        fade_in_ms: 10
"#;

/// A warning (core) and an environment error (engine: the asset does not exist).
const WARNING_AND_MISSING_ASSET: &str = r#"version: "1.0"
output:
  format: wav
  bitrate: 192k
tracks:
  - id: bed
    type: music
    source: "assets/bed.wav"
    loop: true
  - id: voice
    type: speech
    sequence:
      - text: "Hello there."
        provider: mock
"#;

/// A valid speech-only timeline that renders anywhere.
const MOCK_ONLY: &str = r#"version: "1.0"
title: "Mock only"
output:
  path: mock-only.wav
tracks:
  - id: voice
    type: speech
    sequence:
      - id: hello
        text: "Hello from the mock voice."
        provider: mock
        voice_id: narrator
      - pause: 300ms
      - id: bye
        text: "And goodbye."
        provider: mock
        voice_id: narrator
"#;

/// Compare `actual` with `tests/snapshots/<name>`; `SONICCRAFT_UPDATE_SNAPSHOTS=1` rewrites the file.
fn snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("SONICCRAFT_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {} ({e}); run with SONICCRAFT_UPDATE_SNAPSHOTS=1",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "snapshot {name} differs (SONICCRAFT_UPDATE_SNAPSHOTS=1 regenerates it)\n--- expected\n{expected}\n--- actual\n{actual}"
    );
}

/// Replace the temporary directory (as given and canonicalized) with `<dir>`.
fn normalize(text: &str, dir: &Path) -> String {
    let mut out = text.to_string();
    if let Ok(canonical) = dir.canonicalize() {
        out = out.replace(&canonical.to_string_lossy().into_owned(), "<dir>");
    }
    out.replace(&dir.to_string_lossy().into_owned(), "<dir>")
}

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("write fixture");
    path
}

/// Diagnostics of a JSON report with the given severity.
fn with_severity<'a>(report: &'a Value, severity: &str) -> Vec<&'a Value> {
    report["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .filter(|d| d["severity"] == severity)
        .collect()
}

#[test]
fn validate_renders_rustc_style_diagnostics() {
    let dir = temp_dir();
    write(dir.path(), "timeline.yaml", BROKEN);
    let r = run(dir.path(), &["validate", "timeline.yaml"]);
    r.expect_status(1);
    assert!(r.stderr.is_empty(), "{}", r.stderr);
    snapshot("validate_errors.txt", &normalize(&r.stdout, dir.path()));

    write(dir.path(), "warn.yaml", WARNING_AND_MISSING_ASSET);
    let r = run(dir.path(), &["validate", "warn.yaml"]);
    r.expect_status(1);
    snapshot(
        "validate_warning_and_missing_asset.txt",
        &normalize(&r.stdout, dir.path()),
    );

    // --color always: the same text with rustc's palette
    let colored = run(
        dir.path(),
        &["validate", "timeline.yaml", "--color", "always"],
    );
    colored.expect_status(1);
    assert!(
        colored
            .stdout
            .contains("\x1b[1;31merror[invalid_value]\x1b[0m"),
        "{}",
        colored.stdout
    );
    assert!(
        colored.stdout.contains("\x1b[1;34m-->\x1b[0m"),
        "{}",
        colored.stdout
    );
}

#[test]
fn verbosity_controls_notes_warnings_and_paths() {
    let dir = temp_dir();
    write(dir.path(), "warn.yaml", WARNING_AND_MISSING_ASSET);
    let normal = run(dir.path(), &["validate", "warn.yaml"]);
    assert!(
        normal.stdout.contains("warning[bitrate_ignored]"),
        "{}",
        normal.stdout
    );
    assert!(!normal.stdout.contains("info["), "{}", normal.stdout);
    assert!(!normal.stdout.contains("= at:"), "{}", normal.stdout);

    let verbose = run(dir.path(), &["validate", "warn.yaml", "-v"]);
    assert!(verbose.stdout.contains("info["), "{}", verbose.stdout);
    assert!(
        verbose.stdout.contains("= at: output.bitrate"),
        "{}",
        verbose.stdout
    );

    let quiet = run(dir.path(), &["validate", "warn.yaml", "-q"]);
    quiet.expect_status(1);
    assert!(
        quiet.stdout.contains("error[asset_not_found]"),
        "{}",
        quiet.stdout
    );
    assert!(!quiet.stdout.contains("warning["), "{}", quiet.stdout);

    write(dir.path(), "ok.yaml", MOCK_ONLY);
    let quiet_ok = run(dir.path(), &["validate", "ok.yaml", "-q"]);
    quiet_ok.expect_status(0);
    assert!(quiet_ok.stdout.is_empty(), "{}", quiet_ok.stdout);
}

#[test]
fn validate_json_and_exit_statuses() {
    let dir = temp_dir();
    write(dir.path(), "ok.yaml", MOCK_ONLY);
    write(dir.path(), "bad.yaml", BROKEN);

    let ok = run(dir.path(), &["validate", "ok.yaml", "--json"]);
    ok.expect_status(0);
    let report = ok.json();
    assert_eq!(report["valid"], true, "{report}");
    assert_eq!(report["summary"]["speech_cues"], 2, "{report}");
    assert!(ok.stderr.is_empty(), "{}", ok.stderr);

    let human = run(dir.path(), &["validate", "ok.yaml"]);
    human.expect_status(0);
    assert!(
        human.stdout.starts_with("ok: ok.yaml is valid"),
        "{}",
        human.stdout
    );
    assert!(human.stdout.contains("2 speech cues"), "{}", human.stdout);

    let bad = run(dir.path(), &["validate", "bad.yaml", "--json"]);
    bad.expect_status(1);
    let report = bad.json();
    assert_eq!(report["valid"], false);
    let errors = with_severity(&report, "error");
    assert_eq!(errors.len(), BROKEN_ERRORS, "{report}");
    for e in errors {
        for field in ["code", "message", "path", "line", "column", "hint"] {
            assert!(!e[field].is_null(), "missing {field}: {e}");
        }
    }

    // stdin: same report, relative paths against the current directory
    let piped = run_with_stdin(dir.path(), &["validate", "-", "--json"], MOCK_ONLY);
    piped.expect_status(0);
    assert_eq!(piped.json()["valid"], true);
    let piped_bad = run_with_stdin(dir.path(), &["validate", "-"], BROKEN);
    piped_bad.expect_status(1);
    assert!(
        piped_bad.stdout.contains(" --> <stdin>:3:11"),
        "{}",
        piped_bad.stdout
    );

    // JSON timelines get JSON-aware carets
    let json_timeline = r#"{"version": "1.0", "tracks": [{"id": "v", "type": "speech", "sequence": [{"text": "Hi", "provider": "mokc"}]}]}"#;
    write(dir.path(), "t.json", &format!("{json_timeline}\n"));
    let j = run(dir.path(), &["validate", "t.json"]);
    j.expect_status(1);
    assert!(j.stdout.contains("error[unknown_provider]"), "{}", j.stdout);
    assert!(j.stdout.contains("^^^^^^\n"), "{}", j.stdout);
}

#[test]
fn plan_prints_the_diagram_and_the_synthesis_cost() {
    let dir = temp_dir();
    run(
        dir.path(),
        &["init", "p", "--template", "offline-demo-mock", "-q"],
    )
    .expect_status(0);
    let project = dir.path().join("p");
    let human = run(&project, &["plan", "timeline.yaml"]);
    human.expect_status(0);
    for needle in [
        "SonicCraft plan",
        "legend:",
        "cue_03 was pushed",
        "synthesis: 4 cues, 0 cached",
        "ducking:",
        "ambient_bed",
        "ok: timeline.yaml:",
    ] {
        assert!(
            human.stdout.contains(needle),
            "plan output lacks `{needle}`:\n{}",
            human.stdout
        );
    }
    let alias = run(&project, &["dry-run", "timeline.yaml", "--json"]);
    alias.expect_status(0);
    let plan = alias.json();
    assert_eq!(plan["valid"], true);
    // Every cue uses the mock voice, which the dry run measures exactly.
    assert_eq!(plan["estimated"], false, "{plan}");
    assert!(
        plan["diagram"]
            .as_str()
            .is_some_and(|d| d.contains("SonicCraft plan"))
    );
    assert!(
        plan["items"]
            .as_array()
            .is_some_and(|items| items.len() >= 8),
        "{plan}"
    );

    write(dir.path(), "bad.yaml", BROKEN);
    let bad = run(dir.path(), &["plan", "bad.yaml"]);
    bad.expect_status(1);
    assert!(
        bad.stdout.contains(&format!(
            "error: bad.yaml is invalid: {BROKEN_ERRORS} errors"
        )),
        "{}",
        bad.stdout
    );
    let bad_json = run(dir.path(), &["plan", "bad.yaml", "--json"]);
    bad_json.expect_status(1);
    assert_eq!(bad_json.json()["valid"], false);
}

/// Render `timeline` in `project` to `out` with a shared cache and `--json`.
fn render_json(project: &Path, out: &str, cache: &Path, extra: &[&str]) -> Run {
    let cache = cache.to_string_lossy().into_owned();
    let mut args = vec![
        "render",
        "timeline.yaml",
        "-o",
        out,
        "--cache-dir",
        &cache,
        "--json",
    ];
    args.extend_from_slice(extra);
    run(project, &args)
}

#[test]
fn render_with_the_mock_provider_end_to_end() {
    let dir = temp_dir();
    run(
        dir.path(),
        &["init", "p", "--template", "offline-demo-mock", "-q"],
    )
    .expect_status(0);
    let project = dir.path().join("p");
    let cache = dir.path().join("cache");

    let first = render_json(&project, "run1/demo.wav", &cache, &[]);
    first.expect_status(0);
    let r1 = first.json();
    let output = PathBuf::from(r1["output"]["path"].as_str().expect("output path"));
    assert!(output.is_absolute(), "{output:?}");
    assert!(output.is_file());
    assert_eq!(
        output.canonicalize().expect("canonical output"),
        project
            .join("run1/demo.wav")
            .canonicalize()
            .expect("canonical expected")
    );
    assert_eq!(r1["output"]["format"], "wav");
    assert_eq!(r1["output"]["sample_rate"], 48_000);
    let loudness = &r1["loudness"];
    let integrated = loudness["integrated_lufs"].as_f64().expect("integrated");
    let target = loudness["target_lufs"].as_f64().expect("target");
    assert!((integrated - target).abs() < 0.5, "{loudness}");
    let peak = loudness["true_peak_dbtp"].as_f64().expect("true peak");
    assert!(
        peak <= loudness["true_peak_target_dbtp"].as_f64().expect("ceiling") + 0.1,
        "{loudness}"
    );
    let item = |report: &Value, id: &str| {
        report["items"]
            .as_array()
            .and_then(|items| items.iter().find(|i| i["id"] == id).cloned())
            .unwrap_or_else(|| panic!("no item {id}"))
    };
    assert!(item(&r1, "cue_03")["start_ms"].as_f64() >= item(&r1, "cue_02")["end_ms"].as_f64());
    let subtitles = r1["files"]["subtitles"].as_array().expect("subtitles");
    assert_eq!(subtitles.len(), 4, "{r1}");
    let mut files = vec![output.clone()];
    for s in subtitles {
        files.push(PathBuf::from(s["path"].as_str().expect("path")));
    }
    files.push(PathBuf::from(
        r1["files"]["manifest"].as_str().expect("manifest"),
    ));
    for f in &files {
        assert!(f.is_file(), "{f:?} missing");
    }

    // A second render is served from the cache and byte-identical.
    let second = render_json(&project, "run2/demo.wav", &cache, &[]);
    second.expect_status(0);
    let r2 = second.json();
    assert_eq!(r2["synthesis"]["synthesized"], 0, "{}", r2["synthesis"]);
    assert_eq!(r2["synthesis"]["cache_hits"], r2["synthesis"]["cues"]);
    for f in &files[..files.len() - 1] {
        let name = f.file_name().expect("name");
        let a = std::fs::read(f).expect("read run 1");
        let b = std::fs::read(project.join("run2").join(name)).expect("read run 2");
        assert!(a == b, "{name:?} differs between renders");
    }

    // Human output: diagnostics on stderr, the result block on stdout; -q prints only the path.
    let cache_arg = cache.to_string_lossy().into_owned();
    let human = run(
        &project,
        &[
            "render",
            "timeline.yaml",
            "-o",
            "run3/demo.wav",
            "--cache-dir",
            &cache_arg,
        ],
    );
    human.expect_status(0);
    for needle in [
        "Rendered run3/demo.wav",
        "format",
        "loudness",
        "LUFS (target -16.0)",
        "subtitles",
        "demo.vtt",
        "manifest",
        "synthesis",
    ] {
        assert!(
            human.stdout.contains(needle),
            "render output lacks `{needle}`:\n{}",
            human.stdout
        );
    }
    let quiet = run(
        &project,
        &[
            "render",
            "timeline.yaml",
            "-o",
            "run4/demo.wav",
            "--cache-dir",
            &cache_arg,
            "-q",
        ],
    );
    quiet.expect_status(0);
    assert_eq!(quiet.stdout, "run4/demo.wav\n");
    assert!(quiet.stderr.is_empty(), "{}", quiet.stderr);
}

#[test]
fn render_flags_switch_on_side_files_and_formats() {
    let dir = temp_dir();
    write(dir.path(), "timeline.yaml", MOCK_ONLY);
    let cache = dir.path().join("cache");
    let r = render_json(
        dir.path(),
        "out/flags.flac",
        &cache,
        &[
            "--format",
            "flac",
            "--subtitles",
            "--stems",
            "--manifest",
            "--preview",
            "--jobs",
            "2",
        ],
    );
    r.expect_status(0);
    let report = r.json();
    assert_eq!(report["output"]["format"], "flac", "{report}");
    let files = &report["files"];
    let formats: Vec<&str> = files["subtitles"]
        .as_array()
        .expect("subtitles")
        .iter()
        .filter_map(|s| s["format"].as_str())
        .collect();
    assert_eq!(formats, ["vtt", "srt"], "{files}");
    assert_eq!(files["stems"].as_array().map(Vec::len), Some(1), "{files}");
    for key in ["manifest", "preview"] {
        let path = files[key]
            .as_str()
            .unwrap_or_else(|| panic!("no {key}: {files}"));
        assert!(Path::new(path).is_file(), "{path}");
    }
    // --no-cache re-synthesizes although the cache is warm
    let fresh = render_json(dir.path(), "out/again.wav", &cache, &["--no-cache"]);
    fresh.expect_status(0);
    let s = &fresh.json()["synthesis"];
    assert_eq!(s["cache_hits"], 0, "{s}");
}

#[test]
fn relative_output_paths_resolve_against_the_current_directory() {
    let dir = temp_dir();
    let project = dir.path().join("project");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");
    write(&project, "timeline.yaml", MOCK_ONLY);
    let cache = dir.path().join("cache").to_string_lossy().into_owned();
    let r = run(
        &work,
        &[
            "render",
            "../project/timeline.yaml",
            "-o",
            "rel/out.wav",
            "--cache-dir",
            &cache,
            "--json",
        ],
    );
    r.expect_status(0);
    assert!(work.join("rel/out.wav").is_file());
    assert!(!project.join("rel").exists());
    // Without -o the document's output.path is relative to the timeline.
    let r = run(
        &work,
        &[
            "render",
            "../project/timeline.yaml",
            "--cache-dir",
            &cache,
            "-q",
        ],
    );
    r.expect_status(0);
    assert!(project.join("mock-only.wav").is_file(), "{r:?}");
}

#[test]
fn failed_renders_report_diagnostics_and_exit_1() {
    let dir = temp_dir();
    write(dir.path(), "bad.yaml", BROKEN);
    let human = run(dir.path(), &["render", "bad.yaml"]);
    human.expect_status(1);
    assert!(human.stdout.is_empty(), "{}", human.stdout);
    assert!(
        human.stderr.contains("error[unknown_reference]"),
        "{}",
        human.stderr
    );
    assert!(
        human
            .stderr
            .lines()
            .last()
            .is_some_and(|l| l.starts_with("error: ")),
        "{}",
        human.stderr
    );

    let json = run(dir.path(), &["render", "bad.yaml", "--json"]);
    json.expect_status(1);
    let failure = json.json();
    assert!(failure["message"].is_string(), "{failure}");
    assert_eq!(
        with_severity(&failure, "error").len(),
        BROKEN_ERRORS,
        "{failure}"
    );
    assert!(failure.get("cancelled").is_none(), "{failure}");

    write(dir.path(), "missing.yaml", WARNING_AND_MISSING_ASSET);
    let missing = run(dir.path(), &["render", "missing.yaml", "--json"]);
    missing.expect_status(1);
    assert!(
        with_severity(&missing.json(), "error")
            .iter()
            .any(|d| d["code"] == "asset_not_found")
    );
}

#[test]
fn providers_and_voices() {
    let dir = temp_dir();
    let json = run(dir.path(), &["providers", "--json"]);
    json.expect_status(0);
    let report = json.json();
    let providers = report["providers"].as_array().expect("providers");
    let mock = providers
        .iter()
        .find(|p| p["name"] == "mock")
        .expect("mock provider");
    assert_eq!(mock["available"], true, "{mock}");
    let human = run(dir.path(), &["providers"]);
    human.expect_status(0);
    for needle in [
        "PROVIDER",
        "READY",
        "mock",
        "elevenlabs",
        "providers ready here",
    ] {
        assert!(human.stdout.contains(needle), "{}", human.stdout);
    }
    let verbose = run(dir.path(), &["providers", "-v"]);
    verbose.expect_status(0);
    assert!(
        verbose.stdout.contains("ALIASES") && verbose.stdout.contains("11labs"),
        "{}",
        verbose.stdout
    );

    let voices = run(dir.path(), &["voices", "mock", "--json"]);
    voices.expect_status(0);
    let v = voices.json();
    assert!(
        v["voices"].as_array().is_some_and(|list| !list.is_empty()),
        "{v}"
    );
    let table = run(dir.path(), &["voices", "mock"]);
    table.expect_status(0);
    assert!(table.stdout.contains("VOICE ID"), "{}", table.stdout);

    let unknown = run(dir.path(), &["voices", "elevnlabs"]);
    unknown.expect_status(2);
    assert!(unknown.stderr.contains("error:"), "{}", unknown.stderr);
    let unknown_json = run(dir.path(), &["voices", "nope", "--json"]);
    unknown_json.expect_status(2);
    assert!(unknown_json.json()["message"].is_string());

    let no_key = run(dir.path(), &["voices", "elevenlabs", "--json"]);
    no_key.expect_status(3);
    assert!(no_key.json()["message"].is_string());
}

#[test]
fn cache_info_and_clear() {
    let dir = temp_dir();
    write(dir.path(), "timeline.yaml", MOCK_ONLY);
    // Without --cache-dir the cache lives next to the timeline.
    run(dir.path(), &["render", "timeline.yaml", "-q"]).expect_status(0);
    let info = run(dir.path(), &["cache", "info", "timeline.yaml", "--json"]);
    info.expect_status(0);
    let info = info.json();
    let path = PathBuf::from(info["path"].as_str().expect("path"));
    assert!(path.ends_with(".soniccraft/cache"), "{info}");
    assert_eq!(info["entries"], 2, "{info}");
    assert!(info["bytes"].as_u64().is_some_and(|b| b > 0));

    let human = run(dir.path(), &["cache", "info", "timeline.yaml"]);
    human.expect_status(0);
    assert!(human.stdout.contains("2 entries"), "{}", human.stdout);

    let cleared = run(dir.path(), &["cache", "clear", "timeline.yaml", "--json"]);
    cleared.expect_status(0);
    assert_eq!(cleared.json()["removed"], 2);
    let after = run(dir.path(), &["cache", "info", "timeline.yaml", "--json"]);
    assert_eq!(after.json()["entries"], 0);

    // --cache-dir wins, relative to the current directory
    let other = run(
        dir.path(),
        &["cache", "info", "--cache-dir", "elsewhere", "--json"],
    );
    other.expect_status(0);
    let other_path = PathBuf::from(other.json()["path"].as_str().expect("path").to_string());
    assert!(other_path.ends_with("elsewhere"), "{other_path:?}");
    let human = run(dir.path(), &["cache", "clear", "--cache-dir", "elsewhere"]);
    human.expect_status(0);
    assert!(
        human.stdout.contains("removed 0 cache entries"),
        "{}",
        human.stdout
    );
}

#[test]
fn a_closed_pipe_during_a_large_report_does_not_panic() {
    use std::io::Read;
    let dir = temp_dir();
    // 2000 unknown fields: far more output than a pipe buffer holds.
    let mut text = String::from(
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    sequence:\n      - text: hi\n        provider: mock\n",
    );
    for i in 0..2000 {
        text.push_str(&format!("        bogus_field_{i}: 1\n"));
    }
    write(dir.path(), "big.yaml", &text);
    let mut child = common::command(common::bin(), dir.path())
        .args(["validate", "big.yaml"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdout = child.stdout.take().expect("stdout");
    let mut first = [0u8; 16];
    stdout.read_exact(&mut first).expect("read");
    drop(stdout);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("stderr");
    let status = child.wait().expect("wait");
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert_eq!(status.code(), Some(1), "{stderr}");
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_a_render_and_removes_partial_output() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let dir = temp_dir();
    run(
        dir.path(),
        &["init", "-q", "--template", "offline-demo-mock"],
    )
    .expect_status(0);
    // Ten minutes of audio: several seconds of mixing and mastering.
    write(
        dir.path(),
        "long.yaml",
        "version: \"1.0\"\noutput:\n  path: long.wav\n  duration_ms: 600000\ntracks:\n  - id: bed\n    type: music\n    source: assets/audio/synthwave_loop.wav\n    loop: true\n    fade_out_ms: 2000\n    ducking: {trigger_tracks: [voice]}\n  - id: voice\n    type: speech\n    sequence:\n      - text: \"A long render that will be cancelled.\"\n        provider: mock\n",
    );
    let child = common::command(common::bin(), dir.path())
        .args(["render", "long.yaml", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    // Well after startup (the handler is installed within milliseconds) and long before the render ends.
    std::thread::sleep(Duration::from_millis(1000));
    let started = Instant::now();
    let kill = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(kill.success());
    let output = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    assert!(stderr.contains("render cancelled"), "{stderr}");
    assert!(started.elapsed() < Duration::from_secs(20));
    let failure: Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(failure["cancelled"], true, "{failure}");
    assert!(
        !dir.path().join("long.wav").exists(),
        "partial output left behind"
    );
}

#[test]
fn every_repository_timeline_validates_like_ci_expects() {
    // The same rules as scripts/check-timelines.sh: environment errors are tolerated, except for the
    // timelines that need nothing from the environment.
    let root = repo_root();
    let candidates = [
        "examples/offline-demo/timeline.mock.yaml",
        "examples/offline-demo/timeline.yaml",
        "examples/tech-briefing/timeline.yaml",
        "examples/two-person-podcast/timeline.yaml",
    ];
    for rel in candidates {
        let path = root.join(rel);
        if !path.is_file() {
            continue; // outside the repository
        }
        let r = run(&root, &["validate", rel, "--json"]);
        assert!(r.status <= 1, "{rel}: {r:?}");
        let report = r.json();
        let strict = rel.ends_with("mock.yaml")
            || (cfg!(target_os = "macos") && rel == "examples/offline-demo/timeline.yaml");
        for e in with_severity(&report, "error") {
            assert!(
                !strict
                    && ["missing_credentials", "provider_unavailable"]
                        .contains(&e["code"].as_str().unwrap_or_default()),
                "{rel}: {e}"
            );
        }
    }
}

/// Secrets of every shape the scan knows, on one line or several, quoted and plain.
const SECRETS: &str = r#"version: "1.0"
providers:
  eleven:
    type: elevenlabs
    api_key: "sk_live_0123456789abcdefABCDEF"
  oa: {type: openai, api_key_env: sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789}
x-notes:
  password: hunter2hunter2
  token: |
    tok_0123456789abcdef
tracks:
  - id: voice
    type: speech
    sequence:
      - text: "Hello"
        provider: mock
        voice_id: v
        settings: {speed: "sk-ant-api03-ABCDEFGHIJKLMNOPQRST"}
"#;

/// Fragments of the secrets in [`SECRETS`].
const SECRET_FRAGMENTS: [&str; 6] = [
    "sk_live_0123",
    "sk-proj-AbCd",
    "hunter2",
    "tok_0123",
    "sk-ant-api03",
    "ABCDEFGHIJKLMNOPQRST",
];

#[test]
fn human_output_never_shows_secrets() {
    let dir = temp_dir();
    write(dir.path(), "secrets.yaml", SECRETS);
    let cache = dir.path().join("cache").to_string_lossy().into_owned();
    let runs = [
        run(dir.path(), &["validate", "secrets.yaml", "-v"]),
        run(dir.path(), &["plan", "secrets.yaml", "-v"]),
        run(
            dir.path(),
            &["render", "secrets.yaml", "-v", "--cache-dir", &cache],
        ),
        run(dir.path(), &["validate", "secrets.yaml", "--json"]),
        run(dir.path(), &["render", "secrets.yaml", "--json"]),
    ];
    for r in &runs {
        r.expect_status(1);
        for fragment in SECRET_FRAGMENTS {
            assert!(
                !r.stdout.contains(fragment) && !r.stderr.contains(fragment),
                "`{fragment}` leaked:\n{}\n{}",
                r.stdout,
                r.stderr
            );
        }
    }
    assert_eq!(
        runs[0].stdout.matches("<redacted>").count(),
        5,
        "{}",
        runs[0].stdout
    );
    assert!(
        runs[2].stderr.contains("api_key: \"<redacted>\""),
        "{}",
        runs[2].stderr
    );
    snapshot(
        "validate_secrets.txt",
        &run(dir.path(), &["validate", "secrets.yaml"]).stdout,
    );
}

#[test]
fn environment_failures_say_the_render_stopped() {
    let dir = temp_dir();
    write(
        dir.path(),
        "keyed.yaml",
        "version: \"1.0\"\ntracks:\n  - id: v\n    type: speech\n    provider: elevenlabs\n    voice_id: 21m00Tcm4TlvDq8ikWAM\n    sequence:\n      - text: \"Hello\"\n",
    );
    let cache = dir.path().join("cache").to_string_lossy().into_owned();
    let human = run(dir.path(), &["render", "keyed.yaml", "--cache-dir", &cache]);
    human.expect_status(1);
    assert!(
        human.stderr.contains("error[missing_credentials]"),
        "{}",
        human.stderr
    );
    let last = human.stderr.lines().last().unwrap_or_default();
    assert!(
        last.starts_with("error: render stopped: 1 error"),
        "{}",
        human.stderr
    );
    // The JSON document is the engine's report, unchanged.
    let json = run(
        dir.path(),
        &["render", "keyed.yaml", "--cache-dir", &cache, "--json"],
    );
    json.expect_status(1);
    assert!(json.json()["message"].is_string());
    // A document error keeps the engine's summary, which names the document's problems.
    write(dir.path(), "bad.yaml", BROKEN);
    let bad = run(dir.path(), &["render", "bad.yaml"]);
    bad.expect_status(1);
    let bad_json = run(dir.path(), &["render", "bad.yaml", "--json"]);
    let message = bad_json.json()["message"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        bad.stderr.lines().last(),
        Some(format!("error: {message}").as_str()),
        "{}",
        bad.stderr
    );
}

#[test]
fn voices_accepts_provider_names_in_any_case() {
    let dir = temp_dir();
    for name in ["Mock", "MOCK", " mock "] {
        let r = run(dir.path(), &["voices", name, "--json"]);
        r.expect_status(0);
        assert_eq!(r.json()["provider"], "mock", "{name:?}");
    }
    let r = run(dir.path(), &["voices", "ElevenLabs", "--json"]);
    // Recognized (no key here: a provider failure, not an unknown provider).
    r.expect_status(3);
    assert!(
        !r.json()["message"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown provider"),
        "{}",
        r.stdout
    );
}

#[test]
fn a_format_option_that_needs_ffmpeg_is_not_blamed_on_the_timeline() {
    let dir = temp_dir();
    write(dir.path(), "timeline.yaml", MOCK_ONLY);
    // No ffmpeg on an empty PATH.
    let empty = dir.path().join("empty-path");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let output = common::command(common::bin(), dir.path())
        .env("PATH", &empty)
        .args([
            "render",
            "timeline.yaml",
            "--format",
            "opus",
            "-o",
            "out.opus",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("error[format_requires_ffmpeg]"), "{stderr}");
    // The format is a render option: no snippet of the document's `output` block.
    assert!(!stderr.contains("-->"), "{stderr}");
    assert!(
        stderr
            .lines()
            .last()
            .is_some_and(|l| l.starts_with("error: render stopped: 1 error")),
        "{stderr}"
    );
    assert!(!dir.path().join("out.opus").exists());
}
