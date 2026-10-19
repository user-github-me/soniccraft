//! Piper (rhasspy/piper C++ binary or OHF-Voice/piper1-gpl): a child process
//! `piper -m <model.onnx> -f <tmp.wav> [-s N] [--length_scale X] [--noise_scale X] [--noise_w X]
//! [--sentence_silence X]` with the text on stdin (newlines collapsed to spaces). The underscore flag spellings
//! work with both implementations; the output sample rate comes from the WAV the voice produces.
//!
//! `voice_id` is a model path (contains a path separator or ends in `.onnx`; relative paths resolve against the
//! timeline directory) or a voice name looked up as `<dir>/<voice_id>.onnx` in, in order:
//! 1. the instance `voices_dir` (relative to the timeline directory),
//! 2. `$PIPER_VOICES_DIR` (relative to the timeline directory when relative),
//! 3. the timeline directory itself (where `python3 -m piper.download_voices <name>` writes when run there),
//! 4. `~/.local/share/piper/voices`.
//!
//! Relative locations always resolve against the timeline (base) directory, never the process's working
//! directory. A voice that is found nowhere is an error listing every searched location and the download
//! command; [`resolve_piper_model`] applies the same rules for validation.
//!
//! Settings: `speaker` (or `speaker_id`, an integer), `length_scale` (default `1 / speed` when `speed` is
//! set; above 1 is slower), `noise_scale`, `noise_w` (or `noise_w_scale`), `sentence_silence` (seconds).
//! With a custom `command` template on the instance, piper runs like the `command` provider with `{voice_id}`
//! expanded to the resolved model path and the text written to stdin unless the templates mention `{text}`
//! or `{text_file}` (or set `stdin`).

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use soniccraft_core::ir::ProviderInstance;

use super::command::{self, Invocation};
use super::{Ctx, RawSynthesis, decode_encoded, missing_voice};
use crate::defaults::PIPER_VOICES_DIR_ENV;
use crate::env::EnvSource;
use crate::error::{TtsError, TtsErrorKind};
use crate::http::request_timeout;
use crate::process::{self, Spawn};
use crate::settings::{self, Resolved, get_i64, get_positive};
use crate::types::{SynthesisRequest, Voice};

/// Effective `length_scale`: the setting, else `1 / speed`, else 1 (clamped to 0.1–10).
pub(crate) fn length_scale(settings: &Map<String, Value>) -> f64 {
    get_positive(settings, "length_scale")
        .or_else(|| get_positive(settings, "speed").map(|s| 1.0 / s))
        .unwrap_or(1.0)
        .clamp(0.1, 10.0)
}

/// A directory searched for `<voice>.onnx`, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceDir {
    pub path: PathBuf,
    /// `voices_dir`, `PIPER_VOICES_DIR`, `timeline directory` (or `current directory` when listing voices for a
    /// command run there) or `piper's default`.
    pub source: &'static str,
}

/// Label of the base directory in search lists: synthesis and validation search next to the timeline.
pub(crate) const TIMELINE_DIR_LABEL: &str = "timeline directory";

/// Directories searched for `<voice>.onnx`, in order (see the module docs). Without `base_dir`, relative
/// locations and the timeline directory are skipped.
pub(crate) fn voice_dirs(
    origin: &ProviderInstance,
    env: &EnvSource,
    base_dir: Option<&Path>,
) -> Vec<VoiceDir> {
    voice_dirs_labeled(origin, env, base_dir, TIMELINE_DIR_LABEL)
}

/// [`voice_dirs`] with `base_label` naming the base directory in search lists.
fn voice_dirs_labeled(
    origin: &ProviderInstance,
    env: &EnvSource,
    base_dir: Option<&Path>,
    base_label: &'static str,
) -> Vec<VoiceDir> {
    let resolve = |dir: &str| {
        let dir = Path::new(dir);
        if dir.is_absolute() {
            Some(dir.to_path_buf())
        } else {
            base_dir.map(|base| base.join(dir))
        }
    };
    let mut dirs: Vec<VoiceDir> = Vec::new();
    let mut push = |path: Option<PathBuf>, source: &'static str| {
        if let Some(path) = path
            && !dirs.iter().any(|d| d.path == path)
        {
            dirs.push(VoiceDir { path, source });
        }
    };
    push(
        settings::non_empty(origin.voices_dir.as_deref()).and_then(resolve),
        "voices_dir",
    );
    push(
        env.get(PIPER_VOICES_DIR_ENV).as_deref().and_then(resolve),
        PIPER_VOICES_DIR_ENV,
    );
    push(base_dir.map(Path::to_path_buf), base_label);
    push(
        env.home_dir().map(|home| {
            home.join(".local")
                .join("share")
                .join("piper")
                .join("voices")
        }),
        "piper's default",
    );
    dirs
}

/// True when `voice_id` names a model file rather than a voice to look up.
fn is_model_path(voice_id: &str) -> bool {
    voice_id.contains('/') || voice_id.contains('\\') || voice_id.ends_with(".onnx")
}

/// Resolve `voice_id` to a model file, or return the candidates that were tried.
pub(crate) fn resolve_model(
    voice_id: &str,
    origin: &ProviderInstance,
    env: &EnvSource,
    base_dir: &Path,
) -> Result<PathBuf, Vec<PathBuf>> {
    let candidates: Vec<PathBuf> = if is_model_path(voice_id) {
        let path = Path::new(voice_id);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.join(path)
        };
        let mut candidates = vec![path.clone()];
        if !voice_id.ends_with(".onnx") {
            candidates.push(PathBuf::from(format!("{}.onnx", path.display())));
        }
        candidates
    } else {
        voice_dirs(origin, env, Some(base_dir))
            .into_iter()
            .map(|d| d.path.join(format!("{voice_id}.onnx")))
            .collect()
    };
    candidates
        .iter()
        .find(|c| c.is_file())
        .cloned()
        .ok_or(candidates)
}

/// A piper voice that is installed nowhere SonicCraft looks ([`resolve_piper_model`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiperModelNotFound {
    /// The `voice_id` as written.
    pub voice_id: String,
    /// Every model file that was tried, in search order.
    pub searched: Vec<PathBuf>,
    /// What is wrong, listing the searched locations.
    pub message: String,
    /// How to install the voice (the `piper.download_voices` command) or point at it.
    pub hint: String,
}

impl fmt::Display for PiperModelNotFound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PiperModelNotFound {}

/// The piper model file `voice_id` names for `provider`, found exactly as synthesis finds it (see the
/// module docs for the search order); `base_dir` is the timeline directory.
pub fn resolve_piper_model(
    provider: &ProviderInstance,
    voice_id: &str,
    env: &EnvSource,
    base_dir: &Path,
) -> Result<PathBuf, PiperModelNotFound> {
    let voice_id = voice_id.trim();
    resolve_model(voice_id, provider, env, base_dir)
        .map_err(|searched| model_not_found(provider, voice_id, searched, env, base_dir))
}

/// Where the download hint tells the user to put a missing voice: the first searched directory.
fn download_dir(origin: &ProviderInstance, env: &EnvSource, base_dir: &Path) -> PathBuf {
    voice_dirs(origin, env, Some(base_dir))
        .into_iter()
        .next()
        .map_or_else(|| base_dir.to_path_buf(), |d| d.path)
}

fn model_not_found(
    origin: &ProviderInstance,
    voice_id: &str,
    searched: Vec<PathBuf>,
    env: &EnvSource,
    base_dir: &Path,
) -> PiperModelNotFound {
    let (message, hint) = if is_model_path(voice_id) {
        (
            format!(
                "piper model file `{voice_id}` not found (looked for {})",
                list_paths(&searched)
            ),
            "check the path (relative paths resolve against the timeline directory), or set `voice_id` to an \
             installed voice name; `soniccraft voices piper` lists installed voices"
                .to_owned(),
        )
    } else {
        let dirs = voice_dirs(origin, env, Some(base_dir));
        let searched_dirs = if dirs.is_empty() {
            "no voice directories are configured".to_owned()
        } else {
            dirs.iter()
                .map(|d| format!("{} ({})", d.path.display(), d.source))
                .collect::<Vec<_>>()
                .join(", ")
        };
        (
            format!("piper voice `{voice_id}` not found: no `{voice_id}.onnx` in {searched_dirs}"),
            format!(
                "download it: `python3 -m piper.download_voices {voice_id} --data-dir {}` (writes {voice_id}.onnx \
                 and {voice_id}.onnx.json), or set `voice_id` to the path of an .onnx model; `soniccraft voices \
                 piper` lists installed voices",
                download_dir(origin, env, base_dir).display()
            ),
        )
    };
    PiperModelNotFound {
        voice_id: voice_id.to_owned(),
        searched,
        message,
        hint,
    }
}

fn list_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_number(x: f64) -> String {
    format!("{x}")
}

/// Arguments after the program name. Errors name a malformed setting.
pub(crate) fn build_args(
    model: &Path,
    output: &Path,
    settings: &Map<String, Value>,
) -> Result<Vec<OsString>, String> {
    let mut args: Vec<OsString> = vec!["-m".into(), model.into(), "-f".into(), output.into()];
    let speaker_key = ["speaker", "speaker_id"]
        .into_iter()
        .find(|k| settings.contains_key(*k));
    if let Some(key) = speaker_key {
        let speaker = get_i64(settings, key)
            .filter(|s| *s >= 0)
            .ok_or_else(|| format!("piper `{key}` must be a non-negative integer speaker id"))?;
        args.push("-s".into());
        args.push(speaker.to_string().into());
    }
    let explicit_scale = settings.contains_key("length_scale") || settings.contains_key("speed");
    if explicit_scale {
        args.push("--length_scale".into());
        args.push(format_number(length_scale(settings)).into());
    }
    for (flag, keys) in [
        ("--noise_scale", &["noise_scale"][..]),
        ("--noise_w", &["noise_w", "noise_w_scale"][..]),
        ("--sentence_silence", &["sentence_silence"][..]),
    ] {
        if let Some(value) = keys
            .iter()
            .find_map(|k| settings::get_f64(settings, k))
            .filter(|v| *v >= 0.0)
        {
            args.push(flag.into());
            args.push(format_number(value).into());
        }
    }
    Ok(args)
}

pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    let voice_id = resolved
        .voice_id
        .as_deref()
        .ok_or_else(|| missing_voice(origin))?;
    let model = resolve_piper_model(origin, voice_id, ctx.env, &req.base_dir)
        .map_err(|e| TtsError::new(TtsErrorKind::NotFound, origin, e.message).with_hint(e.hint))?;
    let model_str = model.to_string_lossy().into_owned();

    if let Some(argv) = origin.command.as_deref().filter(|a| !a.is_empty()) {
        let mut templates: Vec<&str> = argv.iter().map(String::as_str).collect();
        templates.extend(origin.stdin.as_deref());
        let reads_text =
            command::mentions(&templates, "text") || command::mentions(&templates, "text_file");
        let stdin = origin
            .stdin
            .as_deref()
            .or((!reads_text).then_some("{text}"));
        let extension = command::output_extension(origin);
        return command::run_template(
            ctx,
            req,
            resolved,
            Invocation {
                argv,
                stdin,
                voice_value: &model_str,
                output_extension: &extension,
            },
        )
        .await;
    }

    let program = process::find_program("piper", ctx.env, &req.base_dir).ok_or_else(|| {
        TtsError::new(
            TtsErrorKind::Unavailable,
            origin,
            "the `piper` program was not found on PATH",
        )
        .with_hint(
            "install it (`pipx install piper-tts`) or set `command` on the provider to its argv",
        )
    })?;
    let workdir = tempfile::Builder::new()
        .prefix("soniccraft-piper-")
        .tempdir()
        .map_err(|e| {
            TtsError::new(
                TtsErrorKind::Io,
                origin,
                format!("cannot create a temporary directory: {e}"),
            )
        })?;
    let output = workdir.path().join("output.wav");
    let args = build_args(&model, &output, &resolved.settings)
        .map_err(|m| TtsError::new(TtsErrorKind::InvalidRequest, origin, m))?;
    let stdin = format!("{}\n", crate::text::collapse_whitespace(&req.text)).into_bytes();
    let cwd = req.base_dir.is_dir().then_some(req.base_dir.as_path());
    let env_remove = process::child_env_removals(origin, ctx.child_env_remove);
    let result = process::run(Spawn {
        program: &program,
        args: &args,
        cwd,
        stdin: Some(stdin),
        timeout: request_timeout(origin),
        env_remove: &env_remove,
        stdout_limit: crate::http::MAX_RESPONSE_BYTES,
    })
    .await
    .map_err(|e| command::run_error(origin, e))?;
    if !result.status.success() {
        return Err(TtsError::new(
            TtsErrorKind::Provider,
            origin,
            format!(
                "piper failed with {}: {}",
                process::describe_status(&result.status),
                process::stderr_tail(&result.stderr)
            ),
        ));
    }
    let bytes = command::read_output_file(origin, &output)
        .await?
        .ok_or_else(|| {
            TtsError::new(
                TtsErrorKind::Provider,
                origin,
                format!(
                    "piper exited successfully but wrote no audio{}",
                    match process::stderr_tail(&result.stderr) {
                        tail if tail.is_empty() => String::new(),
                        tail => format!(" (stderr: {tail})"),
                    }
                ),
            )
        })?;
    let audio = decode_encoded(origin, bytes, Some("wav")).await?;
    Ok(RawSynthesis {
        audio,
        words: None,
        notes: Vec::new(),
        output_format: None,
    })
}

/// Installed voices: `*.onnx` files in the search directories (language and quality from the name or the
/// `.onnx.json` config). Finding none is a [`TtsErrorKind::NotFound`] error naming every searched directory
/// and how to download a voice.
pub(crate) fn list_voices(
    env: &EnvSource,
    origin: &ProviderInstance,
    base_dir: Option<&Path>,
    base_label: &'static str,
) -> Result<Vec<Voice>, TtsError> {
    let dirs = voice_dirs_labeled(origin, env, base_dir, base_label);
    let mut voices: Vec<Voice> = Vec::new();
    for dir in &dirs {
        let Ok(entries) = std::fs::read_dir(&dir.path) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "onnx") && p.is_file())
            .collect();
        files.sort();
        for path in files {
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            if voices.iter().any(|v| v.id == id) {
                continue; // earlier directories win, like the lookup
            }
            let config = std::fs::read(PathBuf::from(format!("{}.json", path.display())))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let language = config
                .as_ref()
                .and_then(|c| c.pointer("/language/code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    id.split('-')
                        .next()
                        .filter(|l| l.contains('_'))
                        .map(str::to_owned)
                });
            let sample_rate = config
                .as_ref()
                .and_then(|c| c.pointer("/audio/sample_rate"))
                .and_then(Value::as_u64);
            let description = Some(match sample_rate {
                Some(sr) => format!("{} ({sr} Hz)", path.display()),
                None => path.display().to_string(),
            });
            voices.push(Voice {
                name: id.clone(),
                id,
                language,
                gender: None,
                description,
            });
        }
    }
    if voices.is_empty() {
        let searched = if dirs.is_empty() {
            "no voice directories are configured (HOME is not set)".to_owned()
        } else {
            dirs.iter()
                .map(|d| format!("{} ({})", d.path.display(), d.source))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let target = dirs
            .first()
            .map_or_else(|| "<dir>".to_owned(), |d| d.path.display().to_string());
        return Err(TtsError::new(
            TtsErrorKind::NotFound,
            origin,
            format!("no piper voices (*.onnx models) found; searched {searched}"),
        )
        .with_hint(format!(
            "download one, e.g. `python3 -m piper.download_voices en_US-lessac-medium --data-dir {target}`, or point \
             `voices_dir` (or PIPER_VOICES_DIR) at the directory that holds your .onnx models"
        )));
    }
    Ok(voices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::instance;
    use serde_json::json;
    use soniccraft_core::ir::ProviderType;

    fn s(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn args_are_portable() {
        let args = build_args(
            Path::new("/v/en_US-lessac-medium.onnx"),
            Path::new("/tmp/out.wav"),
            &s(json!({"speaker": 3, "length_scale": 1.25, "noise_scale": 0.667, "noise_w_scale": 0.8, "sentence_silence": 0.2})),
        )
        .unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-m",
                "/v/en_US-lessac-medium.onnx",
                "-f",
                "/tmp/out.wav",
                "-s",
                "3",
                "--length_scale",
                "1.25",
                "--noise_scale",
                "0.667",
                "--noise_w",
                "0.8",
                "--sentence_silence",
                "0.2"
            ]
        );
        let minimal = build_args(Path::new("m.onnx"), Path::new("o.wav"), &Map::new()).unwrap();
        assert_eq!(minimal.len(), 4);
        let speed = build_args(
            Path::new("m.onnx"),
            Path::new("o.wav"),
            &s(json!({"speed": 2.0})),
        )
        .unwrap();
        assert_eq!(speed[5], "0.5");
        assert!(
            build_args(
                Path::new("m"),
                Path::new("o"),
                &s(json!({"speaker": "alice"}))
            )
            .is_err()
        );
    }

    #[test]
    fn length_scale_rules() {
        assert_eq!(length_scale(&Map::new()), 1.0);
        assert_eq!(
            length_scale(&s(json!({"length_scale": 1.5, "speed": 4}))),
            1.5
        );
        assert_eq!(length_scale(&s(json!({"speed": 0.5}))), 2.0);
        assert_eq!(length_scale(&s(json!({"length_scale": 0}))), 1.0);
    }

    #[test]
    fn model_resolution_order() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("timeline");
        let local = base.join("voices");
        let shared = tmp.path().join("shared");
        let home = tmp.path().join("home");
        let home_voices = home.join(".local/share/piper/voices");
        for d in [&local, &shared, &home_voices] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(shared.join("b.onnx"), b"x").unwrap();
        std::fs::write(home_voices.join("c.onnx"), b"x").unwrap();
        std::fs::write(local.join("a.onnx"), b"x").unwrap();
        std::fs::write(
            local.join("a.onnx.json"),
            br#"{"language":{"code":"en_US"},"audio":{"sample_rate":22050}}"#,
        )
        .unwrap();
        std::fs::write(base.join("direct.onnx"), b"x").unwrap();
        // Next to the timeline, as `python3 -m piper.download_voices d` writes it when run there.
        std::fs::write(base.join("d.onnx"), b"x").unwrap();
        // The same name in several places: the earlier location wins.
        std::fs::write(base.join("b.onnx"), b"x").unwrap();
        std::fs::write(home_voices.join("d.onnx"), b"x").unwrap();

        let mut o = instance(ProviderType::Piper);
        o.voices_dir = Some("voices".into());
        let env = EnvSource::fixed([
            ("PIPER_VOICES_DIR", shared.to_string_lossy().into_owned()),
            ("HOME", home.to_string_lossy().into_owned()),
        ]);
        let dirs: Vec<(PathBuf, &str)> = voice_dirs(&o, &env, Some(&base))
            .into_iter()
            .map(|d| (d.path, d.source))
            .collect();
        assert_eq!(
            dirs,
            [
                (local.clone(), "voices_dir"),
                (shared.clone(), "PIPER_VOICES_DIR"),
                (base.clone(), "timeline directory"),
                (home_voices.clone(), "piper's default"),
            ]
        );
        assert_eq!(
            resolve_model("a", &o, &env, &base).unwrap(),
            local.join("a.onnx")
        );
        assert_eq!(
            resolve_model("b", &o, &env, &base).unwrap(),
            shared.join("b.onnx")
        );
        assert_eq!(
            resolve_model("c", &o, &env, &base).unwrap(),
            home_voices.join("c.onnx")
        );
        assert_eq!(
            resolve_model("d", &o, &env, &base).unwrap(),
            base.join("d.onnx")
        );
        assert_eq!(
            resolve_model("./direct.onnx", &o, &env, &base).unwrap(),
            base.join("./direct.onnx")
        );
        assert_eq!(
            resolve_model("./direct", &o, &env, &base).unwrap(),
            PathBuf::from(format!("{}.onnx", base.join("./direct").display()))
        );
        let tried = resolve_model("zzz", &o, &env, &base).unwrap_err();
        assert_eq!(tried.len(), 4);

        let voices = list_voices(&env, &o, Some(&base), TIMELINE_DIR_LABEL).unwrap();
        let ids: Vec<&str> = voices.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "d", "direct", "c"]);
        assert_eq!(voices[0].language.as_deref(), Some("en_US"));
        assert!(
            voices[0]
                .description
                .as_deref()
                .unwrap()
                .ends_with("(22050 Hz)")
        );
    }

    #[test]
    fn relative_locations_resolve_against_the_base_dir_never_the_working_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("project");
        std::fs::create_dir_all(base.join("models")).unwrap();
        std::fs::write(base.join("models/v.onnx"), b"x").unwrap();
        let mut o = instance(ProviderType::Piper);
        o.voices_dir = Some("models".into());
        let env = EnvSource::fixed([("PIPER_VOICES_DIR", "relative/voices")]);
        let dirs: Vec<PathBuf> = voice_dirs(&o, &env, Some(&base))
            .into_iter()
            .map(|d| d.path)
            .collect();
        assert_eq!(
            dirs,
            [
                base.join("models"),
                base.join("relative/voices"),
                base.clone()
            ]
        );
        // Without a base directory, relative locations are skipped rather than read from the cwd.
        assert!(voice_dirs(&o, &env, None).is_empty());
        let voices = list_voices(&env, &o, Some(&base), TIMELINE_DIR_LABEL).unwrap();
        assert_eq!(voices[0].id, "v");
        let err = list_voices(&env, &o, None, TIMELINE_DIR_LABEL).unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::NotFound);
    }

    #[test]
    fn missing_voices_list_every_searched_location_and_the_download_command() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("project");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&base).unwrap();
        let o = instance(ProviderType::Piper);
        let env = EnvSource::fixed([("HOME", home.to_string_lossy().into_owned())]);
        let err = resolve_piper_model(&o, "en_US-lessac-medium", &env, &base).unwrap_err();
        assert_eq!(err.searched.len(), 2);
        assert!(
            err.message
                .contains(&format!("{} (timeline directory)", base.display()))
        );
        assert!(err.message.contains("(piper's default)"), "{}", err.message);
        assert!(
            err.hint.contains(&format!(
                "python3 -m piper.download_voices en_US-lessac-medium --data-dir {}",
                base.display()
            )),
            "{}",
            err.hint
        );
        std::fs::write(base.join("en_US-lessac-medium.onnx"), b"x").unwrap();
        assert_eq!(
            resolve_piper_model(&o, " en_US-lessac-medium ", &env, &base).unwrap(),
            base.join("en_US-lessac-medium.onnx")
        );
        let file = resolve_piper_model(&o, "models/x.onnx", &env, &base).unwrap_err();
        assert!(
            file.message
                .contains("model file `models/x.onnx` not found")
        );
        let listing = list_voices(
            &env,
            &o,
            Some(&tmp.path().join("empty")),
            "current directory",
        )
        .unwrap_err();
        assert!(
            listing.message.contains("(piper's default)"),
            "{}",
            listing.message
        );
        // `soniccraft voices piper` searches where it runs and says so.
        assert!(
            listing.message.contains("(current directory)")
                && !listing.message.contains("timeline directory"),
            "{}",
            listing.message
        );
        assert!(listing.hint.unwrap().contains("piper.download_voices"));
    }
}
