//! Child-process providers (command, piper via a fake binary, real macOS `say`) and the mock voice through
//! the public client API.

mod support;

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use soniccraft_core::ir::ProviderType;
use soniccraft_tts::{
    CredentialStatus, EnvSource, TtsClient, TtsErrorKind, TtsOptions, command_argument_warnings,
    error_codes, estimate_duration_secs,
};
use support::{instance, request, tone, voice, wav_s16};

#[tokio::test]
async fn mock_voice_is_deterministic_and_estimated_exactly() {
    let client = TtsClient::new(TtsOptions::default());
    let make = |voice_id: &str, settings: serde_json::Value| {
        request(
            instance(ProviderType::Mock, None),
            voice(ProviderType::Mock, Some(voice_id), settings),
            "Autonomous software development, explained in three minutes. Ready?",
        )
    };
    for settings in [
        json!({}),
        json!({"wpm": 120}),
        json!({"wpm": 200, "speed": 1.3}),
    ] {
        let req = make("narrator", settings);
        let a = client.synthesize(&req).await.unwrap();
        let b = client.synthesize(&req).await.unwrap();
        assert_eq!(a.audio, b.audio);
        assert_eq!(a.words, b.words);
        assert_eq!(
            a.audio.duration_secs(),
            estimate_duration_secs(&req.provider, &req.voice, &req.text)
        );
        let words = a.words.unwrap();
        assert_eq!(words.len(), 8);
        assert_eq!(words.last().map(|w| w.text.as_str()), Some("Ready?"));
    }
    let other = client.synthesize(&make("guest", json!({}))).await.unwrap();
    let narrator = client
        .synthesize(&make("narrator", json!({})))
        .await
        .unwrap();
    assert_ne!(other.audio, narrator.audio, "voices differ in pitch");
    assert_eq!(
        other.audio.frames(),
        narrator.audio.frames(),
        "but not in timing"
    );
}

#[cfg(unix)]
fn write_script(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn command_provider_templates_stdin_output_and_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wav"),
        wav_s16(16_000, &tone(1_600)),
    )
    .unwrap();
    write_script(
        dir.path(),
        "tts.sh",
        r#"set -e
here="$(dirname "$0")"
cat > "$here/stdin.txt"
cp "$2" "$here/text_copy.txt"
echo "$2" > "$here/text_path.txt"
echo "$3|$4|$5" > "$here/args.txt"
cp "$here/fixture.wav" "$1""#,
    );
    let mut provider = instance(ProviderType::Command, None);
    provider.command = Some(vec![
        "./tts.sh".into(),
        "{output}".into(),
        "{text_file}".into(),
        "--voice={voice_id}".into(),
        "{model}".into(),
        "{language}".into(),
    ]);
    provider.stdin = Some("{text}\n".into());
    let mut v = voice(ProviderType::Command, Some("v1"), json!({}));
    v.model = Some("m2".into());
    v.language = Some("en".into());
    let mut req = request(provider, v, "Hello command");
    req.base_dir = dir.path().to_path_buf();

    let client = TtsClient::new(TtsOptions {
        env: EnvSource::Process,
        ..TtsOptions::default()
    });
    assert_eq!(
        client.credential_status(&req.provider, dir.path()),
        CredentialStatus::NotRequired
    );
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(
        (result.audio.sample_rate, result.audio.frames()),
        (16_000, 1_600)
    );
    assert_eq!(result.words, None);
    let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
    assert_eq!(read("stdin.txt"), "Hello command\n");
    assert_eq!(read("text_copy.txt"), "Hello command");
    assert_eq!(read("args.txt"), "--voice=v1|m2|en\n");
    let text_path = read("text_path.txt");
    assert!(
        !Path::new(text_path.trim()).exists(),
        "temporary files must be removed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn command_provider_reads_stdout_when_no_output_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wav"),
        wav_s16(22_050, &tone(2_205)),
    )
    .unwrap();
    write_script(
        dir.path(),
        "to_stdout.sh",
        r#"cat "$(dirname "$0")/fixture.wav""#,
    );
    let mut provider = instance(ProviderType::Command, None);
    provider.command = Some(vec!["./to_stdout.sh".into(), "{text}".into()]);
    let mut req = request(
        provider,
        voice(ProviderType::Command, Some("x"), json!({})),
        "Hi",
    );
    req.base_dir = dir.path().to_path_buf();
    let result = TtsClient::new(TtsOptions::default())
        .synthesize(&req)
        .await
        .unwrap();
    assert_eq!(
        (result.audio.sample_rate, result.audio.frames()),
        (22_050, 2_205)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn command_provider_failures() {
    let dir = tempfile::tempdir().unwrap();
    write_script(
        dir.path(),
        "fail.sh",
        "echo 'starting' >&2\necho 'voice not found: x' >&2\nexit 3",
    );
    write_script(dir.path(), "silent.sh", "exit 0");
    write_script(dir.path(), "slow.sh", "sleep 5");
    let client = TtsClient::new(TtsOptions::default());
    let run = |argv: Vec<&str>, timeout_ms: u64| {
        let mut provider = instance(ProviderType::Command, None);
        provider.command = Some(argv.into_iter().map(str::to_owned).collect());
        provider.timeout_ms = timeout_ms;
        let mut req = request(
            provider,
            voice(ProviderType::Command, Some("x"), json!({})),
            "Hi",
        );
        req.base_dir = dir.path().to_path_buf();
        let client = client.clone();
        async move { client.synthesize(&req).await }
    };

    let err = run(vec!["./fail.sh", "{output}"], 10_000)
        .await
        .unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Provider);
    assert!(
        err.message.contains("exit status 3") && err.message.contains("voice not found: x"),
        "{err}"
    );

    let err = run(vec!["./silent.sh", "{output}"], 10_000)
        .await
        .unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Provider);
    assert!(err.message.contains("did not write audio"), "{err}");

    let err = run(vec!["./missing.sh", "{output}"], 10_000)
        .await
        .unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Unavailable);

    let started = std::time::Instant::now();
    let err = run(vec!["./slow.sh", "{output}"], 200).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Timeout);
    assert!(started.elapsed() < Duration::from_secs(4));

    let mut provider = instance(ProviderType::Command, None);
    provider.command = Some(Vec::new());
    let err = client
        .synthesize(&request(
            provider,
            voice(ProviderType::Command, None, json!({})),
            "Hi",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::InvalidRequest);
}

#[cfg(unix)]
#[tokio::test]
async fn piper_argv_stdin_and_model_lookup_with_a_fake_binary() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    let voices = root.path().join("voices");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::write(voices.join("en_US-test-medium.onnx"), b"model").unwrap();
    std::fs::write(
        root.path().join("fixture.wav"),
        wav_s16(22_050, &tone(4_410)),
    )
    .unwrap();
    // Records its argv and stdin, then copies the fixture to the -f path.
    write_script(
        &bin,
        "piper",
        &format!(
            r#"log="{log}"
printf '%s\n' "$@" > "$log/argv.txt"
cat > "$log/stdin.txt"
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-f" ]; then out="$2"; fi
  shift
done
cp "{fixture}" "$out""#,
            log = root.path().display(),
            fixture = root.path().join("fixture.wav").display()
        ),
    );
    let env = EnvSource::fixed([
        ("PATH", bin.to_string_lossy().into_owned()),
        ("PIPER_VOICES_DIR", voices.to_string_lossy().into_owned()),
    ]);
    let client = TtsClient::new(TtsOptions {
        env,
        ..TtsOptions::default()
    });
    let provider = instance(ProviderType::Piper, None);
    assert_eq!(
        client.credential_status(&provider, root.path()),
        CredentialStatus::NotRequired
    );
    let mut req = request(
        provider,
        voice(
            ProviderType::Piper,
            Some("en_US-test-medium"),
            json!({"length_scale": 1.25, "speaker": 2}),
        ),
        "Hello piper\nworld",
    );
    req.base_dir = root.path().to_path_buf();
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(
        (result.audio.sample_rate, result.audio.frames()),
        (22_050, 4_410)
    );

    let argv = std::fs::read_to_string(root.path().join("argv.txt")).unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    assert_eq!(argv[0], "-m");
    assert_eq!(
        argv[1],
        voices.join("en_US-test-medium.onnx").to_string_lossy()
    );
    assert_eq!(argv[2], "-f");
    assert!(argv[3].ends_with("output.wav"));
    assert_eq!(&argv[4..], ["-s", "2", "--length_scale", "1.25"]);
    assert_eq!(
        std::fs::read_to_string(root.path().join("stdin.txt")).unwrap(),
        "Hello piper world\n"
    );

    let voices_listed = client.list_voices(&req.provider).await.unwrap();
    assert_eq!(
        voices_listed
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>(),
        ["en_US-test-medium"]
    );
    assert_eq!(voices_listed[0].language.as_deref(), Some("en_US"));

    let mut missing = req.clone();
    missing.voice.voice_id = Some("de_DE-nobody-low".into());
    let err = client.synthesize(&missing).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::NotFound);
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("download_voices de_DE-nobody-low"),
        "{err:?}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_say_synthesizes_and_validates_voices() {
    let client = TtsClient::new(TtsOptions::default());
    let provider = instance(ProviderType::MacosSay, None);
    assert_eq!(
        client.credential_status(&provider, Path::new(".")),
        CredentialStatus::NotRequired
    );
    let req = request(
        provider.clone(),
        voice(ProviderType::MacosSay, Some("Samantha"), json!({})),
        "Hello from say.",
    );
    let result = client.synthesize(&req).await.unwrap();
    assert_eq!(result.audio.sample_rate, 48_000);
    assert_eq!(result.audio.num_channels(), 1);
    assert!(
        result.audio.duration_secs() > 0.4 && result.audio.duration_secs() < 5.0,
        "{}",
        result.audio.duration_secs()
    );
    assert!(result.audio.peak() > 0.01, "audible");

    // Rate changes the duration.
    let fast = request(
        provider.clone(),
        voice(
            ProviderType::MacosSay,
            Some("Samantha"),
            json!({"rate": 320}),
        ),
        "Hello from say.",
    );
    let fast = client.synthesize(&fast).await.unwrap();
    assert!(fast.audio.duration_secs() < result.audio.duration_secs());

    let bad = request(
        provider.clone(),
        voice(ProviderType::MacosSay, Some("Samanta"), json!({})),
        "Hi",
    );
    let err = client.synthesize(&bad).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::NotFound);
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("`Samantha`"),
        "{err:?}"
    );

    let voices = client.list_voices(&provider).await.unwrap();
    assert!(voices.iter().any(|v| v.id.starts_with("Samantha")));
}

/// A command request running `argv` in `dir` (which holds `fixture.wav`).
#[cfg(unix)]
fn command_request(
    dir: &Path,
    argv: &[&str],
    voice_id: &str,
    text: &str,
) -> soniccraft_tts::SynthesisRequest {
    let mut provider = instance(ProviderType::Command, None);
    provider.command = Some(argv.iter().map(|a| (*a).to_owned()).collect());
    let mut req = request(
        provider,
        voice(ProviderType::Command, Some(voice_id), json!({})),
        text,
    );
    req.base_dir = dir.to_path_buf();
    req
}

#[cfg(unix)]
fn fixture_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("fixture.wav"),
        wav_s16(16_000, &tone(1_600)),
    )
    .unwrap();
    dir
}

/// Whether process `pid` still exists (zombies included).
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: kill(2) with signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
fn read_pid(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_kills_the_command_with_its_child_processes() {
    let dir = fixture_dir();
    // The script starts a grandchild, records its pid and waits for it.
    write_script(
        dir.path(),
        "slow.sh",
        "sleep 47 &\necho $! > \"$(dirname \"$0\")/grandchild.pid\"\nwait",
    );
    let mut req = command_request(dir.path(), &["./slow.sh", "{output}"], "x", "Hi");
    req.provider.timeout_ms = 500;
    let started = Instant::now();
    let err = TtsClient::new(TtsOptions::default())
        .synthesize(&req)
        .await
        .unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::Timeout, "{err:?}");
    assert!(
        err.message.contains("with its child processes"),
        "{}",
        err.message
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let pid = read_pid(&dir.path().join("grandchild.pid"));
    // Killed with its group (and reaped by init shortly after).
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive(pid), "grandchild {pid} survived the timeout");
}

#[cfg(unix)]
#[tokio::test]
async fn a_background_grandchild_does_not_turn_success_into_a_timeout() {
    let dir = fixture_dir();
    // The grandchild inherits stdout and stderr and outlives the script, which succeeds at once.
    write_script(
        dir.path(),
        "bg.sh",
        "here=\"$(dirname \"$0\")\"\ncp \"$here/fixture.wav\" \"$1\"\nsleep 30 &\necho $! > \"$here/bg.pid\"\nexit 0",
    );
    let mut req = command_request(dir.path(), &["./bg.sh", "{output}"], "x", "Hi");
    req.provider.timeout_ms = 10_000;
    let started = Instant::now();
    let result = TtsClient::new(TtsOptions::default()).synthesize(&req).await;
    let elapsed = started.elapsed();
    let pid = read_pid(&dir.path().join("bg.pid"));
    // SAFETY: kill(2) of the test's own leftover `sleep`.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    assert_eq!(result.unwrap().audio.frames(), 1_600);
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn option_like_text_and_voice_ids_never_reach_the_program() {
    let dir = fixture_dir();
    write_script(
        dir.path(),
        "tts.sh",
        "here=\"$(dirname \"$0\")\"\ntouch \"$here/ran\"\ncp \"$here/fixture.wav\" \"$1\"",
    );
    let client = TtsClient::new(TtsOptions::default());
    let ran = dir.path().join("ran");
    // The reviewer's injection: text that `say` would have read as `-o <file>`.
    let text_arg = command_request(
        dir.path(),
        &["./tts.sh", "{output}", "{text}"],
        "v",
        "-o/tmp/INJECTED.wav",
    );
    assert_eq!(command_argument_warnings(&text_arg.provider).len(), 1);
    let err = client.synthesize(&text_arg).await.unwrap_err();
    assert!(err.has_code(error_codes::OPTION_LIKE_ARGUMENT), "{err:?}");
    assert_eq!(err.kind, TtsErrorKind::InvalidRequest);
    assert!(err.message.contains("command[2]"), "{}", err.message);
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("{text_file}")
    );
    let voice_arg = command_request(
        dir.path(),
        &["./tts.sh", "{output}", "{voice_id}"],
        "-x",
        "Hi",
    );
    let err = client.synthesize(&voice_arg).await.unwrap_err();
    assert!(err.has_code(error_codes::OPTION_LIKE_ARGUMENT), "{err:?}");
    assert!(err.message.contains("`voice_id` `-x`"), "{}", err.message);
    assert!(!ran.exists(), "the program must not run");
    // Attached to its option, or passed on stdin, the same values are harmless.
    let attached = command_request(
        dir.path(),
        &["./tts.sh", "{output}", "--voice={voice_id}"],
        "-x",
        "-dash",
    );
    assert!(command_argument_warnings(&attached.provider).is_empty());
    client.synthesize(&attached).await.unwrap();
    let mut stdin = command_request(dir.path(), &["./tts.sh", "{output}"], "v", "-dash first");
    stdin.provider.stdin = Some("{text}".into());
    client.synthesize(&stdin).await.unwrap();
    assert!(ran.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn child_processes_do_not_inherit_api_key_variables() {
    let dir = fixture_dir();
    write_script(
        dir.path(),
        "env.sh",
        "here=\"$(dirname \"$0\")\"\nprintf '%s|%s' \"${HOME:-unset}\" \"${PATH:+path-set}\" > \"$here/env.txt\"\ncp \"$here/fixture.wav\" \"$1\"",
    );
    let env_seen = |options: TtsOptions, api_key_env: Option<&str>| {
        let mut req = command_request(dir.path(), &["./env.sh", "{output}"], "v", "Hi");
        req.provider.api_key_env = api_key_env.map(str::to_owned);
        let seen = dir.path().join("env.txt");
        async move {
            TtsClient::new(options).synthesize(&req).await.unwrap();
            std::fs::read_to_string(seen).unwrap()
        }
    };
    // HOME stands in for a key variable: it is set in every test environment.
    assert!(
        !env_seen(TtsOptions::default(), None)
            .await
            .starts_with("unset")
    );
    // The instance's own `api_key_env`, and every variable the caller names (e.g. all the timeline's).
    assert_eq!(
        env_seen(TtsOptions::default(), Some("HOME")).await,
        "unset|path-set"
    );
    let options = TtsOptions {
        child_env_remove: vec!["HOME".into()],
        ..TtsOptions::default()
    };
    assert_eq!(env_seen(options, None).await, "unset|path-set");
}

#[cfg(unix)]
#[tokio::test]
async fn piper_voices_downloaded_next_to_the_timeline_are_found() {
    // The documented setup: `python3 -m piper.download_voices en_US-lessac-medium` run in the project.
    let root = tempfile::tempdir().unwrap();
    let (bin, project, home) = (
        root.path().join("bin"),
        root.path().join("project"),
        root.path().join("home"),
    );
    for d in [&bin, &project, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(project.join("en_US-lessac-medium.onnx"), b"model").unwrap();
    std::fs::write(
        project.join("en_US-lessac-medium.onnx.json"),
        br#"{"audio":{"sample_rate":22050}}"#,
    )
    .unwrap();
    std::fs::write(
        root.path().join("fixture.wav"),
        wav_s16(22_050, &tone(2_205)),
    )
    .unwrap();
    write_script(
        &bin,
        "piper",
        &format!(
            "printf '%s\\n' \"$@\" > \"{log}/argv.txt\"\ncat > /dev/null\nout=\"\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"-f\" ]; then out=\"$2\"; fi\n  shift\ndone\ncp \"{fixture}\" \"$out\"",
            log = root.path().display(),
            fixture = root.path().join("fixture.wav").display()
        ),
    );
    let client = TtsClient::new(TtsOptions {
        env: EnvSource::fixed([
            ("PATH", bin.to_string_lossy().into_owned()),
            ("HOME", home.to_string_lossy().into_owned()),
        ]),
        ..TtsOptions::default()
    });
    let provider = instance(ProviderType::Piper, None);
    let mut req = request(
        provider.clone(),
        voice(ProviderType::Piper, Some("en_US-lessac-medium"), json!({})),
        "Hello from piper.",
    );
    req.base_dir = project.clone();
    let model = project.join("en_US-lessac-medium.onnx");
    assert_eq!(
        client.resolve_piper_model(&provider, "en_US-lessac-medium", &project),
        Ok(model.clone())
    );
    assert_eq!(client.synthesize(&req).await.unwrap().audio.frames(), 2_205);
    let argv = std::fs::read_to_string(root.path().join("argv.txt")).unwrap();
    assert_eq!(argv.lines().nth(1), Some(model.to_string_lossy().as_ref()));
    let listed = client.list_voices_in(&provider, &project).await.unwrap();
    assert_eq!(
        listed.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        ["en_US-lessac-medium"]
    );

    // Without the timeline directory there is nothing to find: the error lists where it looked and how to
    // download a voice.
    let err = client.list_voices(&provider).await.unwrap_err();
    assert_eq!(err.kind, TtsErrorKind::NotFound);
    assert!(err.message.contains("(piper's default)"), "{}", err.message);
    assert!(
        err.hint
            .as_deref()
            .unwrap_or_default()
            .contains("python3 -m piper.download_voices")
    );
    let missing = client
        .resolve_piper_model(&provider, "de_DE-nobody-low", &project)
        .unwrap_err();
    assert_eq!(missing.searched.len(), 2);
    assert!(
        missing.hint.contains(&format!(
            "python3 -m piper.download_voices de_DE-nobody-low --data-dir {}",
            project.display()
        )),
        "{}",
        missing.hint
    );

    // A relative `voices_dir` resolves against the timeline directory, not the process's working directory.
    std::fs::create_dir_all(project.join("voices")).unwrap();
    std::fs::rename(&model, project.join("voices/en_US-lessac-medium.onnx")).unwrap();
    let mut with_dir = req.clone();
    with_dir.provider.voices_dir = Some("voices".into());
    assert_eq!(
        client.synthesize(&with_dir).await.unwrap().audio.frames(),
        2_205
    );
    let listed = client
        .list_voices_in(&with_dir.provider, &project)
        .await
        .unwrap();
    assert_eq!(listed[0].id, "en_US-lessac-medium");
}
