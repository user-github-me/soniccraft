//! Command-line surface that does not depend on the engine: help, version, usage errors (exit 2),
//! `schema`, and broken pipes.

mod common;

use std::io::Read;
use std::process::Stdio;

use common::{command, repo_root, run, temp_dir};

#[test]
fn help_lists_every_subcommand_and_the_exit_codes() {
    let dir = temp_dir();
    let help = run(dir.path(), &["--help"]);
    help.expect_status(0);
    for word in [
        "validate",
        "plan",
        "render",
        "schema",
        "init",
        "providers",
        "voices",
        "cache",
        "mcp",
        "Exit codes",
        "--json",
        "--quiet",
        "--verbose",
        "--color",
        "--cache-dir",
        "--jobs",
    ] {
        assert!(
            help.stdout.contains(word),
            "--help lacks `{word}`:\n{}",
            help.stdout
        );
    }
    assert!(help.stdout.contains("Usage: soniccraft"), "{}", help.stdout);
    for sub in [
        "validate",
        "plan",
        "render",
        "init",
        "cache",
        "mcp",
        "voices",
        "providers",
        "schema",
    ] {
        let r = run(dir.path(), &[sub, "--help"]);
        r.expect_status(0);
        assert!(
            r.stdout.contains("Examples:") || r.stdout.contains("Register it:"),
            "{sub} --help:\n{}",
            r.stdout
        );
    }
    let plan = run(dir.path(), &["dry-run", "--help"]);
    plan.expect_status(0);
    assert!(
        plan.stdout.contains("Usage: soniccraft plan"),
        "{}",
        plan.stdout
    );
}

#[test]
fn version() {
    let dir = temp_dir();
    let r = run(dir.path(), &["--version"]);
    r.expect_status(0);
    assert_eq!(
        r.stdout.trim(),
        format!("soniccraft {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_errors_exit_2() {
    let dir = temp_dir();
    for args in [
        &[][..],
        &["frobnicate"],
        &["render"],
        &["render", "t.yaml", "--format", "aiff"],
        &["render", "t.yaml", "--jobs", "0"],
        &["validate", "t.yaml", "-q", "-v"],
        &["init", "--template", "no-such-template"],
        &["init", "--list", "--force"],
        &["cache"],
        &["cache", "purge"],
        &["validate", "t.yaml", "--color", "sometimes"],
    ] {
        let r = run(dir.path(), args);
        assert_eq!(
            r.status, 2,
            "{args:?}\nstdout: {}\nstderr: {}",
            r.stdout, r.stderr
        );
        assert!(
            r.stdout.is_empty(),
            "{args:?} wrote to stdout: {}",
            r.stdout
        );
        assert!(!r.stderr.is_empty(), "{args:?} printed no message");
        assert!(!r.stderr.contains("panicked"), "{}", r.stderr);
    }
    // clap suggests the template names
    let r = run(dir.path(), &["init", "--template", "tech-briefin"]);
    assert!(r.stderr.contains("tech-briefing"), "{}", r.stderr);
}

#[test]
fn output_directory_is_a_usage_error() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("t.yaml"), "version: \"1.0\"\n").expect("write");
    let r = run(dir.path(), &["render", "t.yaml", "-o", "."]);
    r.expect_status(2);
    assert!(r.stderr.contains("is a directory"), "{}", r.stderr);
}

#[test]
fn schema_matches_the_committed_file() {
    let dir = temp_dir();
    let r = run(dir.path(), &["schema"]);
    r.expect_status(0);
    let printed = r.json();
    assert_eq!(printed["type"], "object", "{printed}");
    let committed = repo_root().join("skills/soniccraft/reference/timeline-schema.json");
    if let Ok(text) = std::fs::read_to_string(&committed) {
        let expected: serde_json::Value = serde_json::from_str(&text).expect("committed schema");
        assert_eq!(
            printed,
            expected,
            "`soniccraft schema` differs from {}",
            committed.display()
        );
        assert_eq!(
            r.stdout, text,
            "byte-identical to the committed (generated) file"
        );
    }
    // -o writes the same bytes, creating parent directories; the confirmation goes to stderr.
    let w = run(dir.path(), &["schema", "-o", "nested/dir/schema.json"]);
    w.expect_status(0);
    assert!(w.stdout.is_empty(), "{}", w.stdout);
    assert!(w.stderr.contains("nested/dir/schema.json"), "{}", w.stderr);
    let written = std::fs::read_to_string(dir.path().join("nested/dir/schema.json")).expect("read");
    assert_eq!(written, r.stdout);
    let quiet = run(dir.path(), &["schema", "-q", "-o", "s.json"]);
    quiet.expect_status(0);
    assert!(quiet.stderr.is_empty(), "{}", quiet.stderr);
}

#[test]
fn broken_pipe_on_stdout_does_not_panic() {
    let dir = temp_dir();
    for args in [&["schema"][..], &["--help"], &["init", "--list"]] {
        let mut child = command(common::bin(), dir.path())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        let mut stdout = child.stdout.take().expect("stdout");
        let mut first = [0u8; 1];
        stdout.read_exact(&mut first).expect("read a byte");
        drop(stdout); // close the pipe like `| head -c 1`
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .expect("stderr")
            .read_to_string(&mut stderr)
            .expect("read stderr");
        let status = child.wait().expect("wait");
        assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
        assert!(
            matches!(status.code(), Some(0)),
            "{args:?}: {status:?} {stderr}"
        );
    }
}

#[test]
fn missing_timeline_is_a_runtime_failure_with_a_json_document() {
    let dir = temp_dir();
    let human = run(dir.path(), &["validate", "nope.yaml"]);
    human.expect_status(3);
    assert!(human.stdout.is_empty());
    assert!(
        human
            .stderr
            .contains("error: timeline file `nope.yaml` not found"),
        "{}",
        human.stderr
    );
    assert!(human.stderr.contains("= hint:"), "{}", human.stderr);
    for sub in ["validate", "plan", "render"] {
        let json = run(dir.path(), &[sub, "nope.yaml", "--json"]);
        json.expect_status(3);
        let doc = json.json();
        assert!(
            doc["message"]
                .as_str()
                .is_some_and(|m| m.contains("not found")),
            "{doc}"
        );
        assert_eq!(doc["diagnostics"], serde_json::json!([]));
    }
    let r = run(dir.path(), &["validate", "."]);
    r.expect_status(3);
    assert!(r.stderr.contains("is a directory"), "{}", r.stderr);
}

#[test]
fn soniccraft_mcp_takes_no_arguments() {
    let dir = temp_dir();
    let r = common::run_program(common::mcp_bin(), dir.path(), &["--base-dir", "."], None);
    r.expect_status(2);
    assert!(r.stderr.contains("takes no arguments"), "{}", r.stderr);
    let v = common::run_program(common::mcp_bin(), dir.path(), &["--version"], None);
    v.expect_status(0);
    assert!(v.stdout.starts_with("soniccraft-mcp "), "{}", v.stdout);
    let h = common::run_program(common::mcp_bin(), dir.path(), &["--help"], None);
    h.expect_status(0);
    assert!(h.stdout.contains("soniccraft mcp"), "{}", h.stdout);
}

#[test]
fn mcp_rejects_a_missing_base_dir() {
    let dir = temp_dir();
    let r = run(dir.path(), &["mcp", "--base-dir", "does/not/exist"]);
    r.expect_status(2);
    assert!(
        r.stdout.is_empty(),
        "stdout must stay protocol-only: {}",
        r.stdout
    );
    assert!(r.stderr.contains("is not a directory"), "{}", r.stderr);
}

#[test]
fn json_usage_errors_print_one_json_document() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("t.yaml"), "version: \"1.0\"\n").expect("write");
    // (arguments, a fragment of the message, a fragment of the hint)
    let cases: [(&[&str], &str, &str); 9] = [
        (
            &["render", "t.yaml", "--json", "--jobs", "0"],
            "invalid value '0' for '--jobs <N>'",
            "see `soniccraft render --help`",
        ),
        (
            &["validate", "t.yaml", "--json", "--bogus"],
            "unexpected argument '--bogus' found",
            "see `soniccraft validate --help`",
        ),
        (
            &["plan", "--json"],
            "the following required arguments were not provided: <TIMELINE>",
            "see `soniccraft plan --help`",
        ),
        (
            &["render", "t.yaml", "--json", "--format", "aiff"],
            "[possible values: wav, mp3, flac, ogg, opus, m4a]",
            "see `soniccraft render --help`",
        ),
        (
            &["validate", "t.yaml", "--jsn", "--json"],
            "unexpected argument '--jsn' found",
            "a similar argument exists: '--json'",
        ),
        (
            &["cache", "info", "--json", "--bogus"],
            "'--bogus'",
            "see `soniccraft cache info --help`",
        ),
        (
            &["validat", "t.yaml", "--json"],
            "unrecognized subcommand 'validat'",
            "a similar subcommand exists: 'validate'",
        ),
        (
            &["--json", "validate", "t.yaml"],
            "unexpected argument '--json' found",
            "put --json after `validate`; see `soniccraft validate --help`",
        ),
        (
            &["init", "x", "--json"],
            "unexpected argument '--json' found",
            "`soniccraft init` has no --json option: drop it",
        ),
    ];
    for (args, message, hint) in cases {
        let r = run(dir.path(), args);
        r.expect_status(2);
        let doc = r.json();
        assert!(
            doc["message"].as_str().is_some_and(|m| m.contains(message)),
            "{args:?}: {doc}"
        );
        assert!(
            !doc["message"]
                .as_str()
                .unwrap_or_default()
                .starts_with("error"),
            "{doc}"
        );
        assert_eq!(doc["diagnostics"], serde_json::json!([]), "{args:?}");
        assert!(
            doc["hint"].as_str().is_some_and(|h| h.contains(hint)),
            "{args:?}: {doc}"
        );
        // clap's message stays on stderr.
        assert!(r.stderr.starts_with("error: "), "{args:?}: {}", r.stderr);
    }
    let schema = run(dir.path(), &["schema", "--json"]);
    schema.expect_status(2);
    assert!(
        schema.json()["hint"]
            .as_str()
            .is_some_and(|h| h.contains("always prints JSON")),
        "{}",
        schema.stdout
    );
    // Colors never reach the JSON document.
    let colored = run(
        dir.path(),
        &[
            "validate", "t.yaml", "--json", "--bogus", "--color", "always",
        ],
    );
    colored.expect_status(2);
    assert!(colored.stderr.contains('\x1b'), "{}", colored.stderr);
    assert!(!colored.stdout.contains('\x1b'), "{}", colored.stdout);
    colored.json();
    // --help and --version are unchanged; `mcp` keeps stdout for the protocol; `--json` after `--` is a
    // file name, not the option.
    let help = run(dir.path(), &["validate", "--help", "--json"]);
    help.expect_status(0);
    assert!(
        help.stdout.starts_with("Check a timeline"),
        "{}",
        help.stdout
    );
    let version = run(dir.path(), &["--version", "--json"]);
    version.expect_status(0);
    assert!(
        version.stdout.starts_with("soniccraft "),
        "{}",
        version.stdout
    );
    let mcp = run(dir.path(), &["mcp", "--json"]);
    mcp.expect_status(2);
    assert!(mcp.stdout.is_empty(), "{}", mcp.stdout);
    let positional = run(
        dir.path(),
        &["validate", "t.yaml", "--bogus", "--", "--json"],
    );
    positional.expect_status(2);
    assert!(positional.stdout.is_empty(), "{}", positional.stdout);
}

#[test]
fn oversized_and_endless_timelines_fail_fast_with_the_size_error() {
    use std::io::Write;
    use std::time::{Duration, Instant};
    let dir = temp_dir();
    let max = 5 * 1024 * 1024;
    // A regular file over the limit is rejected from its size, without reading it.
    let big = dir.path().join("big.yaml");
    std::fs::write(
        &big,
        format!("title: \"{}\"\n", "a".repeat(max + 1024 * 1024)),
    )
    .expect("write");
    for sub in ["validate", "plan", "render"] {
        let r = run(dir.path(), &[sub, "big.yaml", "--json"]);
        r.expect_status(1);
        let doc = r.json();
        assert_eq!(
            doc["message"], "timeline file `big.yaml` is 6.0 MB; the maximum is 5 MB",
            "{doc}"
        );
        let d = &doc["diagnostics"][0];
        assert_eq!(d["code"], "yaml_syntax", "{doc}");
        assert_eq!(d["message"], "the document is 6.0 MB; the maximum is 5 MB");
        assert_eq!(
            (d["line"].as_u64(), d["column"].as_u64()),
            (Some(1), Some(1))
        );
        assert!(
            d["hint"].as_str().is_some_and(|h| h.contains("Split")),
            "{d}"
        );
        assert!(doc["hint"].is_string(), "{doc}");
    }
    let human = run(dir.path(), &["validate", "big.yaml"]);
    human.expect_status(1);
    assert!(
        human
            .stderr
            .starts_with("error: timeline file `big.yaml` is 6.0 MB; the maximum is 5 MB\n"),
        "{}",
        human.stderr
    );
    // A JSON file gets the JSON code.
    std::fs::write(
        dir.path().join("big.json"),
        format!("{{\"title\": \"{}\"}}", "a".repeat(max)),
    )
    .expect("write");
    let r = run(dir.path(), &["validate", "big.json", "--json"]);
    r.expect_status(1);
    assert_eq!(r.json()["diagnostics"][0]["code"], "json_syntax");

    // Streams: stdin that never ends, and a device. At most the limit plus one byte is read.
    let started = Instant::now();
    let mut child = common::command(common::bin(), dir.path())
        .args(["validate", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let writer = std::thread::spawn(move || {
        let chunk = vec![b'a'; 64 * 1024];
        // Until the reader goes away (a broken pipe ends the loop).
        while stdin.write_all(&chunk).is_ok() {}
    });
    let output = child.wait_with_output().expect("wait");
    writer.join().expect("writer");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        doc["message"], "the timeline on stdin is larger than the 5 MB maximum",
        "{doc}"
    );
    assert!(started.elapsed() < Duration::from_secs(30));
    if cfg!(unix) {
        let started = Instant::now();
        let r = run(dir.path(), &["validate", "/dev/zero", "--json"]);
        r.expect_status(1);
        assert_eq!(
            r.json()["message"],
            "timeline file `/dev/zero` is larger than the 5 MB maximum"
        );
        assert!(started.elapsed() < Duration::from_secs(30));
    }
    // Just under the limit is read and parsed as usual.
    let fits = format!(
        "version: \"1.0\"\ntitle: \"{}\"\ntracks: []\n",
        "a".repeat(max - 64)
    );
    std::fs::write(dir.path().join("fits.yaml"), &fits).expect("write");
    let r = run(dir.path(), &["validate", "fits.yaml", "--json"]);
    r.expect_status(1);
    assert_eq!(r.json()["diagnostics"][0]["code"], "empty_tracks");
    let r = common::run_with_stdin(dir.path(), &["validate", "-", "--json"], &fits);
    r.expect_status(1);
    assert_eq!(r.json()["diagnostics"][0]["code"], "empty_tracks");
}

#[test]
fn unreadable_timelines_keep_their_messages() {
    let dir = temp_dir();
    std::fs::write(dir.path().join("latin1.yaml"), b"title: caf\xe9\n").expect("write");
    let r = run(dir.path(), &["validate", "latin1.yaml", "--json"]);
    r.expect_status(3);
    assert_eq!(
        r.json()["message"],
        "timeline file `latin1.yaml` is not UTF-8 text"
    );
    let mut child = command(common::bin(), dir.path())
        .args(["validate", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin.write_all(b"title: caf\xe9\n").expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait");
    assert_eq!(output.status.code(), Some(3));
    let doc: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(
        doc["message"], "the timeline on stdin is not UTF-8 text",
        "{doc}"
    );
    let r = run(dir.path(), &["validate", "missing.yaml"]);
    r.expect_status(3);
    assert!(
        r.stderr.contains("timeline file `missing.yaml` not found"),
        "{}",
        r.stderr
    );
}

#[test]
fn mcp_documents_allow_command() {
    let dir = temp_dir();
    let help = run(dir.path(), &["mcp", "--help"]);
    help.expect_status(0);
    for needle in [
        "--allow-command",
        "Off by default",
        "SONICCRAFT_ALLOW_COMMAND",
        "provider_unavailable",
    ] {
        assert!(
            help.stdout.contains(needle),
            "lacks `{needle}`:\n{}",
            help.stdout
        );
    }
    let launcher = common::run_program(common::mcp_bin(), dir.path(), &["--help"], None);
    launcher.expect_status(0);
    assert!(
        launcher.stdout.contains("SONICCRAFT_ALLOW_COMMAND=1"),
        "{}",
        launcher.stdout
    );
    // The server's other own variable (read by soniccraft-mcp itself) is documented in both helps.
    let root = run(dir.path(), &["--help"]);
    root.expect_status(0);
    for text in [&root.stdout, &launcher.stdout] {
        assert!(text.contains("SONICCRAFT_MCP_MAX_RENDERS"), "{text}");
    }
    // An unusable value is ignored with a warning (fail closed); the server still runs.
    for program in [common::bin(), common::mcp_bin()] {
        let args: &[&str] = if program == common::bin() {
            &["mcp"]
        } else {
            &[]
        };
        let mut cmd = common::command(program, dir.path());
        cmd.args(args)
            .env("SONICCRAFT_ALLOW_COMMAND", "${SONICCRAFT_ALLOW_COMMAND}")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = cmd.output().expect("run");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr.contains("ignoring SONICCRAFT_ALLOW_COMMAND=`${SONICCRAFT_ALLOW_COMMAND}`"),
            "{stderr}"
        );
    }
}
