//! The `command` provider: any program, run from an argv template (DESIGN.md §2.8).
//!
//! Placeholders, expanded in every argv element and in the optional `stdin` template, in one pass (expanded
//! values are never re-scanned): `{text}`, `{text_file}` (a UTF-8 temp file holding the text), `{output}` (the
//! file the program must write, any decodable audio format; its extension is the instance `output_format`,
//! default `wav`), `{voice_id}`, `{model}`, `{language}`. Unknown `{…}` sequences are left untouched.
//!
//! If no template mentions `{output}`, the program's stdout is decoded as the audio instead. The program runs
//! in the request's `base_dir`; relative program paths (`./tts.sh`) resolve against it, bare names against
//! `PATH`. Temporary files live in a private directory removed afterwards. A non-zero exit status is an error
//! carrying the tail of stderr.
//!
//! **Untrusted values.** Cue text and voice ids come from the timeline. Pass text through `{text_file}` or
//! the `stdin` template (`stdin: "{text}"`), not as an argument of its own: a value that starts with `-`
//! would be read by the program as an option. SonicCraft refuses to run a command when an argument that
//! begins with `{text}`, `{voice_id}`, `{model}` or `{language}` would start with `-`
//! ([`crate::error_codes::OPTION_LIKE_ARGUMENT`]), and [`command_argument_warnings`] reports templates that
//! pass `{text}` or `{voice_id}` as an argument of its own. The program's environment never contains the
//! provider API keys (see [`crate::TtsOptions::child_env_remove`]).

use std::ffi::OsString;
use std::path::Path;

use serde::Serialize;
use soniccraft_core::ir::{ProviderInstance, ProviderType};

use super::{Ctx, RawSynthesis, decode_encoded};
use crate::error::{TtsError, TtsErrorKind, error_codes};
use crate::http::{self, request_timeout};
use crate::process::{self, RunError, Spawn};
use crate::settings::{self, Resolved};
use crate::types::SynthesisRequest;

/// Values substituted into templates.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TemplateValues<'a> {
    pub text: &'a str,
    pub text_file: &'a str,
    pub output: &'a str,
    pub voice_id: &'a str,
    pub model: &'a str,
    pub language: &'a str,
}

const PLACEHOLDERS: [&str; 6] = [
    "text",
    "text_file",
    "output",
    "voice_id",
    "model",
    "language",
];

/// Expand known placeholders in one left-to-right pass.
pub(crate) fn expand(template: &str, values: &TemplateValues<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let replacement = after.find('}').and_then(|close| {
            let name = &after[..close];
            let value = match name {
                "text" => values.text,
                "text_file" => values.text_file,
                "output" => values.output,
                "voice_id" => values.voice_id,
                "model" => values.model,
                "language" => values.language,
                _ => return None,
            };
            Some((value, close))
        });
        match replacement {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// True when any template contains `{name}`.
pub(crate) fn mentions(templates: &[&str], name: &str) -> bool {
    debug_assert!(PLACEHOLDERS.contains(&name));
    let token = format!("{{{name}}}");
    templates.iter().any(|t| t.contains(&token))
}

/// Placeholders whose values come from the timeline (and may start with `-`), with what they are called in
/// messages.
const UNTRUSTED: [(&str, &str); 4] = [
    ("text", "the cue text"),
    ("voice_id", "`voice_id`"),
    ("model", "`model`"),
    ("language", "`language`"),
];

/// Refuse an argument that would start with `-` because a timeline value begins it (`{text}` as an argument
/// of its own, `{voice_id}.onnx`, …): the program would read it as an option.
fn check_option_like(
    origin: &ProviderInstance,
    argv: &[String],
    values: &TemplateValues<'_>,
    program: &str,
) -> Result<(), TtsError> {
    for (index, element) in argv.iter().enumerate().skip(1) {
        for (name, what) in UNTRUSTED {
            if !element.starts_with(&format!("{{{name}}}")) {
                continue;
            }
            let value = match name {
                "text" => values.text,
                "voice_id" => values.voice_id,
                "model" => values.model,
                _ => values.language,
            };
            if !value.starts_with('-') {
                continue;
            }
            let hint = if name == "text" {
                "pass the text through stdin (`stdin: \"{text}\"`) or a file (`{text_file}`) instead of `{text}` in \
                 `command`, or rephrase the cue so it does not start with `-`"
                    .to_owned()
            } else {
                format!(
                    "use a {} that does not start with `-`",
                    what.trim_matches('`')
                )
            };
            let shown = if name == "text" {
                String::new()
            } else {
                format!(" `{value}`")
            };
            return Err(TtsError::new(
                TtsErrorKind::InvalidRequest,
                origin,
                format!(
                    "{what}{shown} starts with `-`, and `command[{index}]` passes it as an argument of its own, so \
                     `{program}` would read it as an option; refusing to run the command"
                ),
            )
            .with_code(error_codes::OPTION_LIKE_ARGUMENT)
            .with_hint(hint));
        }
    }
    Ok(())
}

/// A `command` (or piper `command`) template element that passes untrusted text or a voice id as an
/// argument of its own: a value starting with `-` would be read as an option. SonicCraft refuses such
/// values at synthesis time; the engine reports these elements as warnings at validation time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandArgumentWarning {
    /// Index of the element in `command` (`providers.<name>.command[<index>]`).
    pub index: usize,
    /// The placeholder: `text` or `voice_id`.
    pub placeholder: String,
    /// What is wrong.
    pub message: String,
    /// The fix.
    pub hint: String,
}

/// The elements of `provider`'s `command` template that begin with `{text}` or `{voice_id}` (for piper,
/// `{text}` only: its `{voice_id}` is the resolved model path). Empty for other provider types.
pub fn command_argument_warnings(provider: &ProviderInstance) -> Vec<CommandArgumentWarning> {
    let placeholders: &[&str] = match provider.provider_type {
        ProviderType::Command => &["text", "voice_id"],
        ProviderType::Piper => &["text"],
        _ => return Vec::new(),
    };
    let Some(argv) = provider.command.as_deref() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (index, element) in argv.iter().enumerate() {
        for name in placeholders {
            if !element.starts_with(&format!("{{{name}}}")) {
                continue;
            }
            let (message, hint) = if *name == "text" {
                (
                    format!(
                        "`command[{index}]` passes the cue text as an argument of its own (`{{text}}`): text that \
                         starts with `-` would be read as an option (SonicCraft refuses such cues)"
                    ),
                    "use {text_file} or stdin for text: replace `{text}` with `{text_file}` if the program reads \
                     a file, or remove it and set `stdin: \"{text}\"`"
                        .to_owned(),
                )
            } else {
                (
                    format!(
                        "`command[{index}]` passes the voice id as an argument of its own (`{{voice_id}}`): a voice \
                         id that starts with `-` would be read as an option (SonicCraft refuses such voice ids)"
                    ),
                    "attach it to its option, e.g. `--voice={voice_id}`".to_owned(),
                )
            };
            out.push(CommandArgumentWarning {
                index,
                placeholder: (*name).to_owned(),
                message,
                hint,
            });
        }
    }
    out
}

/// How a template invocation is configured (shared with piper's custom-command mode).
pub(crate) struct Invocation<'a> {
    pub argv: &'a [String],
    pub stdin: Option<&'a str>,
    /// Value of `{voice_id}` (piper: the resolved model path).
    pub voice_value: &'a str,
    /// Extension of `{output}` and decoder hint.
    pub output_extension: &'a str,
}

/// Run an argv template and decode what it produced.
pub(crate) async fn run_template(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
    invocation: Invocation<'_>,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    let Some((program_template, arg_templates)) = invocation.argv.split_first() else {
        return Err(TtsError::new(TtsErrorKind::InvalidRequest, origin, "the `command` template is empty")
            .with_hint("set `command` to an argv list, e.g. [\"my-tts\", \"--voice\", \"{voice_id}\", \"--out\", \"{output}\"]"));
    };
    let workdir = tempfile::Builder::new()
        .prefix("soniccraft-tts-")
        .tempdir()
        .map_err(|e| {
            TtsError::new(
                TtsErrorKind::Io,
                origin,
                format!("cannot create a temporary directory: {e}"),
            )
        })?;
    let text_file = workdir.path().join("text.txt");
    std::fs::write(&text_file, req.text.as_bytes()).map_err(|e| {
        TtsError::new(
            TtsErrorKind::Io,
            origin,
            format!("cannot write {}: {e}", text_file.display()),
        )
    })?;
    let output = workdir
        .path()
        .join(format!("output.{}", invocation.output_extension));
    let (text_file_str, output_str) = (text_file.to_string_lossy(), output.to_string_lossy());
    let values = TemplateValues {
        text: &req.text,
        text_file: &text_file_str,
        output: &output_str,
        voice_id: invocation.voice_value,
        model: resolved.model_str(),
        language: resolved.language.as_deref().unwrap_or(""),
    };

    let program_name = expand(program_template, &values);
    check_option_like(origin, invocation.argv, &values, &program_name)?;
    let program = process::find_program(&program_name, ctx.env, &req.base_dir).ok_or_else(|| {
        TtsError::new(TtsErrorKind::Unavailable, origin, format!("program `{program_name}` not found"))
            .with_hint("install it, put it on PATH, or use a path (relative paths resolve against the timeline directory)")
    })?;
    let args: Vec<OsString> = arg_templates
        .iter()
        .map(|a| OsString::from(expand(a, &values)))
        .collect();
    let stdin = invocation.stdin.map(|t| expand(t, &values).into_bytes());
    let mut templates: Vec<&str> = invocation.argv.iter().map(String::as_str).collect();
    templates.extend(invocation.stdin);
    let writes_file = mentions(&templates, "output");

    let cwd = req.base_dir.is_dir().then_some(req.base_dir.as_path());
    let env_remove = process::child_env_removals(origin, ctx.child_env_remove);
    let result = process::run(Spawn {
        program: &program,
        args: &args,
        cwd,
        stdin,
        timeout: request_timeout(origin),
        env_remove: &env_remove,
        stdout_limit: http::MAX_RESPONSE_BYTES,
    })
    .await
    .map_err(|e| run_error(origin, e))?;
    if !result.status.success() {
        let tail = process::stderr_tail(&result.stderr);
        let mut message = format!(
            "`{program_name}` failed with {}",
            process::describe_status(&result.status)
        );
        if !tail.is_empty() {
            message.push_str(": ");
            message.push_str(&tail);
        }
        return Err(TtsError::new(TtsErrorKind::Provider, origin, message)
            .with_hint("run the command by hand with the same arguments to debug it"));
    }
    let bytes = if writes_file {
        match read_output_file(origin, &output).await? {
            Some(bytes) => bytes,
            None => {
                let tail = process::stderr_tail(&result.stderr);
                return Err(TtsError::new(
                    TtsErrorKind::Provider,
                    origin,
                    format!(
                        "`{program_name}` exited successfully but did not write audio to {{output}}{}",
                        if tail.is_empty() { String::new() } else { format!(" (stderr: {tail})") }
                    ),
                )
                .with_hint("make the command write its audio to the path passed as {output}"));
            }
        }
    } else if !result.stdout.is_empty() {
        result.stdout
    } else {
        return Err(TtsError::new(
            TtsErrorKind::Provider,
            origin,
            format!("`{program_name}` produced no audio on stdout"),
        )
        .with_hint(
            "pass {output} in the command template, or make the program write audio to stdout",
        ));
    };
    let audio = decode_encoded(origin, bytes, Some(invocation.output_extension)).await?;
    drop(workdir); // removes text.txt and the output file
    Ok(RawSynthesis {
        audio,
        words: None,
        notes: Vec::new(),
        output_format: None,
    })
}

/// The audio a program wrote to `path`: `None` when the file is missing or empty, an error when it is larger
/// than [`http::MAX_RESPONSE_BYTES`].
pub(crate) async fn read_output_file(
    origin: &ProviderInstance,
    path: &Path,
) -> Result<Option<Vec<u8>>, TtsError> {
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.len() > http::MAX_RESPONSE_BYTES as u64 => Err(http::too_large(
            origin,
            "the audio file the program wrote",
            http::MAX_RESPONSE_BYTES,
        )),
        Ok(meta) if meta.len() > 0 => {
            Ok(tokio::fs::read(path).await.ok().filter(|b| !b.is_empty()))
        }
        _ => Ok(None),
    }
}

/// Map a process failure.
pub(crate) fn run_error(origin: &ProviderInstance, error: RunError) -> TtsError {
    match &error {
        RunError::Spawn { .. } => {
            TtsError::new(TtsErrorKind::Unavailable, origin, error.to_string())
                .with_hint("check that the program exists and is executable")
        }
        RunError::Timeout { .. } => TtsError::new(TtsErrorKind::Timeout, origin, error.to_string())
            .with_hint("raise `timeout_ms` on the provider"),
        RunError::OutputTooLarge { .. } => {
            TtsError::new(TtsErrorKind::Provider, origin, error.to_string())
                .with_code(error_codes::RESPONSE_TOO_LARGE)
                .with_hint("make the program write its audio to the path passed as {output}, or check what it prints")
        }
        RunError::Io { .. } => TtsError::new(TtsErrorKind::Io, origin, error.to_string()),
    }
}

/// The `{output}` extension: the instance `output_format` (e.g. `mp3`), default `wav`.
pub(crate) fn output_extension(origin: &ProviderInstance) -> String {
    settings::non_empty(origin.output_format.as_deref())
        .map(|f| f.trim_start_matches('.').to_ascii_lowercase())
        .filter(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "wav".to_owned())
}

pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    let origin = &req.provider;
    let argv = origin.command.as_deref().filter(|a| !a.is_empty()).ok_or_else(|| {
        TtsError::new(TtsErrorKind::InvalidRequest, origin, "the command provider needs a `command` argv template")
            .with_hint("e.g. command: [\"my-tts\", \"--voice\", \"{voice_id}\", \"--text-file\", \"{text_file}\", \"--out\", \"{output}\"]")
    })?;
    let voice = resolved.voice_id.as_deref().unwrap_or("");
    let extension = output_extension(origin);
    run_template(
        ctx,
        req,
        resolved,
        Invocation {
            argv,
            stdin: origin.stdin.as_deref(),
            voice_value: voice,
            output_extension: &extension,
        },
    )
    .await
}

/// Whether the program of an argv template can be found (for credential status).
pub(crate) fn program_status(
    argv: Option<&[String]>,
    env: &crate::env::EnvSource,
    base_dir: &Path,
) -> Result<(), String> {
    let Some(program) = argv
        .and_then(<[String]>::first)
        .map(String::as_str)
        .filter(|p| !p.trim().is_empty())
    else {
        return Err("the command provider needs a `command` argv template".to_owned());
    };
    if program.contains('{') {
        return Ok(()); // templated program: only known at synthesis time
    }
    match process::find_program(program, env, base_dir) {
        Some(_) => Ok(()),
        None => Err(format!(
            "program `{program}` not found (install it, put it on PATH, or use a path)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> TemplateValues<'static> {
        TemplateValues {
            text: "Hi {output}",
            text_file: "/tmp/t.txt",
            output: "/tmp/o.wav",
            voice_id: "v1",
            model: "",
            language: "en",
        }
    }

    #[test]
    fn expands_known_placeholders_once() {
        let v = values();
        assert_eq!(expand("--out={output}", &v), "--out=/tmp/o.wav");
        assert_eq!(expand("{voice_id}:{language}:{model}", &v), "v1:en:");
        assert_eq!(
            expand("{text}", &v),
            "Hi {output}",
            "substituted text is not re-expanded"
        );
        assert_eq!(expand("{unknown} {json:1} {", &v), "{unknown} {json:1} {");
        assert_eq!(expand("{{text}}", &v), "{Hi {output}}");
        assert_eq!(expand("no placeholders", &v), "no placeholders");
    }

    #[test]
    fn detects_mentions() {
        assert!(mentions(&["say", "-o", "{output}"], "output"));
        assert!(!mentions(&["say", "{text}"], "output"));
    }

    #[test]
    fn output_extension_defaults_to_wav() {
        let mut o = crate::test_util::instance(soniccraft_core::ir::ProviderType::Command);
        assert_eq!(output_extension(&o), "wav");
        o.output_format = Some(".MP3".into());
        assert_eq!(output_extension(&o), "mp3");
        o.output_format = Some("../evil".into());
        assert_eq!(output_extension(&o), "wav");
    }

    #[test]
    fn option_like_values_are_refused_only_where_they_begin_an_argument() {
        let o = crate::test_util::instance(ProviderType::Command);
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let with = |text: &'static str, voice_id: &'static str| TemplateValues {
            text,
            voice_id,
            ..values()
        };
        let check = |a: &[&str], v: TemplateValues<'_>| check_option_like(&o, &argv(a), &v, "tts");
        let err = check(&["tts", "{text}"], with("-rf", "v")).unwrap_err();
        assert!(err.has_code(error_codes::OPTION_LIKE_ARGUMENT));
        assert!(check(&["tts", "{text}"], with("fine", "v")).is_ok());
        assert!(check(&["tts", "--text={text}"], with("-rf", "v")).is_ok());
        assert!(check(&["tts", "{voice_id}.onnx"], with("x", "-m")).is_err());
        assert!(check(&["tts", "--voice", "{voice_id}"], with("x", "-m")).is_err());
        assert!(
            check(&["{voice_id}"], with("x", "-m")).is_ok(),
            "the program is not an argument"
        );
        let model = TemplateValues {
            model: "-m",
            ..values()
        };
        assert!(check(&["tts", "{model}"], model).is_err());
    }

    #[test]
    fn templates_passing_text_or_voice_ids_as_their_own_argument_are_reported() {
        let mut o = crate::test_util::instance(ProviderType::Command);
        o.command = Some(
            [
                "tts",
                "{text}",
                "--voice",
                "{voice_id}",
                "--out={output}",
                "{text_file}",
            ]
            .map(str::to_owned)
            .to_vec(),
        );
        let warnings = command_argument_warnings(&o);
        let found: Vec<(usize, &str)> = warnings
            .iter()
            .map(|w| (w.index, w.placeholder.as_str()))
            .collect();
        assert_eq!(found, [(1, "text"), (3, "voice_id")]);
        assert!(
            warnings[0]
                .hint
                .contains("use {text_file} or stdin for text")
        );
        assert!(warnings[0].message.contains("`command[1]`"));
        let mut p = crate::test_util::instance(ProviderType::Piper);
        p.command = Some(
            ["piper", "-m", "{voice_id}", "{text}"]
                .map(str::to_owned)
                .to_vec(),
        );
        assert_eq!(
            command_argument_warnings(&p).len(),
            1,
            "piper's {{voice_id}} is the resolved model path"
        );
        assert!(
            command_argument_warnings(&crate::test_util::instance(ProviderType::Openai)).is_empty()
        );
    }

    #[test]
    fn program_status_checks() {
        let env = crate::env::EnvSource::fixed([("PATH", "/nonexistent")]);
        assert!(program_status(None, &env, Path::new("/")).is_err());
        assert!(
            program_status(
                Some(&["definitely-not-a-program".to_owned()]),
                &env,
                Path::new("/")
            )
            .is_err()
        );
        assert!(program_status(Some(&["{voice_id}".to_owned()]), &env, Path::new("/")).is_ok());
    }
}
