//! macOS `say`: `say -v <voice> [-r <rate>] -o <tmp>/output.wav --file-format=WAVE --data-format=LEI16@48000
//! -f <tmp>/text.txt`, stdin from `/dev/null` (a TTY would make `say` keep only the last line).
//!
//! `say` silently falls back to the default voice for unknown names, so the voice is validated against
//! `say -v '?'` first (listed once per process, and listed again, at most every 2 s, when a voice is not
//! found, in case it was installed since); short names (`Eddy`) match their full display names
//! (`Eddy (English (US))`), case-insensitively. [`resolve_macos_voice`] applies exactly these rules for
//! validation. Settings: `rate` (words per minute), else `speed` (→ `rate = 175 × speed`). The produced WAV
//! carries a 4 KB `FLLR` chunk, which the DSP decoder skips.
//!
//! On other platforms the provider reports [`TtsErrorKind::Unavailable`]; the parsing helpers stay
//! compiled (and unit-tested) everywhere.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use soniccraft_core::ir::ProviderInstance;

use super::{Ctx, RawSynthesis};
use crate::error::{TtsError, TtsErrorKind};
use crate::settings::{Resolved, get_positive};
use crate::types::{SynthesisRequest, Voice};

/// Nominal default speaking rate of `say`, in words per minute.
pub(crate) const DEFAULT_RATE_WPM: f64 = 175.0;

/// One line of `say -v '?'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SayVoice {
    /// Display name, e.g. `Eddy (English (US))` or `Samantha`.
    pub name: String,
    /// Locale, e.g. `en_US`.
    pub locale: String,
    /// Sample sentence.
    pub sample: String,
}

impl SayVoice {
    /// The name before ` (`, e.g. `Eddy` for `Eddy (English (US))`.
    pub(crate) fn short_name(&self) -> &str {
        self.name.split(" (").next().unwrap_or(&self.name).trim()
    }
}

fn is_locale(token: &str) -> bool {
    let Some((lang, region)) = token.split_once('_') else {
        return false;
    };
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|b| b.is_ascii_lowercase())
        && (2..=4).contains(&region.len())
        && region
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Parse one line, equivalent to `^(?P<name>.+?)\s+(?P<locale>[a-z]{2,3}_[A-Z0-9]{2,4})\s+#\s?(?P<sample>.*)$`.
pub(crate) fn parse_voice_line(line: &str) -> Option<SayVoice> {
    let line = line.trim_end();
    // Byte offsets of whitespace-separated tokens.
    let mut tokens: Vec<(usize, &str)> = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                tokens.push((s, &line[s..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push((s, &line[s..]));
    }
    for i in 1..tokens.len().saturating_sub(1) {
        let (locale_at, locale) = tokens[i];
        let (hash_at, after) = tokens[i + 1];
        if is_locale(locale) && after.starts_with('#') {
            let name = line[..locale_at].trim();
            if name.is_empty() {
                return None;
            }
            let sample = line[hash_at + 1..]
                .strip_prefix(char::is_whitespace)
                .unwrap_or(&line[hash_at + 1..]);
            return Some(SayVoice {
                name: name.to_owned(),
                locale: locale.to_owned(),
                sample: sample.to_owned(),
            });
        }
    }
    None
}

/// Parse `say -v '?'` output, dropping duplicate (name, locale) pairs (quality variants) in order.
pub(crate) fn parse_voice_list(output: &str) -> Vec<SayVoice> {
    let mut voices: Vec<SayVoice> = Vec::new();
    for voice in output.lines().filter_map(parse_voice_line) {
        if !voices
            .iter()
            .any(|v| v.name == voice.name && v.locale == voice.locale)
        {
            voices.push(voice);
        }
    }
    voices
}

/// Find `wanted` (full or short name, case-insensitive). Returns the argument to pass to `-v`, or the closest
/// names as suggestions.
pub(crate) fn resolve_voice(voices: &[SayVoice], wanted: &str) -> Result<String, Vec<String>> {
    let wanted = wanted.trim();
    if let Some(v) = voices.iter().find(|v| v.name.eq_ignore_ascii_case(wanted)) {
        return Ok(v.name.clone());
    }
    if let Some(v) = voices
        .iter()
        .find(|v| v.short_name().eq_ignore_ascii_case(wanted))
    {
        // Pass the short name so `say` applies its own locale preference among variants.
        return Ok(v.short_name().to_owned());
    }
    let mut short_names: Vec<&str> = Vec::new();
    for v in voices {
        if !short_names.contains(&v.short_name()) {
            short_names.push(v.short_name());
        }
    }
    let mut suggestions: Vec<String> = Vec::new();
    if let Some(best) =
        soniccraft_core::diagnostics::did_you_mean(wanted, short_names.iter().copied())
    {
        suggestions.push(best.to_owned());
    }
    let lower = wanted.to_ascii_lowercase();
    for name in &short_names {
        if suggestions.len() >= 5 {
            break;
        }
        let candidate = name.to_ascii_lowercase();
        let related =
            candidate.starts_with(lower.get(..2).unwrap_or(&lower)) || candidate.contains(&lower);
        if related && !suggestions.iter().any(|s| s == name) {
            suggestions.push((*name).to_owned());
        }
    }
    Err(suggestions)
}

/// `-r` argument: `rate`, else `175 × speed`.
pub(crate) fn rate_arg(settings: &serde_json::Map<String, Value>) -> Option<u32> {
    get_positive(settings, "rate")
        .or_else(|| {
            get_positive(settings, "speed")
                .filter(|s| (*s - 1.0).abs() > 1e-9)
                .map(|s| s * DEFAULT_RATE_WPM)
        })
        .map(|r| r.round().clamp(1.0, 1_000.0) as u32)
}

/// `say -v '?'`, listed once per process (see [`installed_voices`]), and when it was listed.
static SAY_VOICES: Mutex<Option<(Instant, Arc<Vec<SayVoice>>)>> = Mutex::new(None);

/// A listing younger than this is not refreshed: many cues with the same unknown voice cost one listing.
const REFRESH_MIN_AGE: Duration = Duration::from_secs(2);

/// The installed voices, listed once per process (`refresh` lists again unless the listing is younger than
/// [`REFRESH_MIN_AGE`], e.g. after a voice was installed while a long-running server was up). Blocking:
/// runs `say -v '?'` (about 0.2 s) when nothing usable is cached.
pub(crate) fn installed_voices(refresh: bool) -> Result<Arc<Vec<SayVoice>>, String> {
    let mut cache = SAY_VOICES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((listed_at, voices)) = cache.as_ref()
        && (!refresh || listed_at.elapsed() < REFRESH_MIN_AGE)
    {
        return Ok(voices.clone());
    }
    let voices = Arc::new(parse_voice_list(&list_installed()?));
    *cache = Some((Instant::now(), voices.clone()));
    Ok(voices)
}

/// `say -v '?'` output.
#[cfg(target_os = "macos")]
fn list_installed() -> Result<String, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let say = std::path::Path::new("/usr/bin/say");
    let program = if say.is_file() {
        say.to_path_buf()
    } else {
        which::which("say").map_err(|_| "the `say` program was not found".to_owned())?
    };
    let mut child = Command::new(&program)
        .args(["-v", "?"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run `say -v '?'`: {e}"))?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut out);
            }
            out
        })
    };
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`say -v '?'` did not finish within 30 s".to_owned());
            }
            Err(e) => return Err(format!("waiting for `say -v '?'` failed: {e}")),
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if !status.success() {
        return Err(format!(
            "`say -v '?'` failed: {}",
            crate::process::stderr_tail(&stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(not(target_os = "macos"))]
fn list_installed() -> Result<String, String> {
    Err("the macos_say provider only works on macOS".to_owned())
}

/// Resolve `wanted` against the installed voices, listing them again once when it is not found.
fn resolve_installed(wanted: &str) -> Result<Result<String, Vec<String>>, String> {
    let voices = installed_voices(false)?;
    match resolve_voice(&voices, wanted) {
        Ok(voice) => Ok(Ok(voice)),
        // Not in the (possibly stale) list: list once more before giving up.
        Err(_) => Ok(resolve_voice(&installed_voices(true)?, wanted)),
    }
}

/// The voices installed on this Mac (`say -v '?'`, listed once per process; `id` is the value to put in
/// `voice_id`). `None` off macOS or when `say` cannot list them. Blocking: call it from a blocking context.
pub fn macos_installed_voices() -> Option<Vec<Voice>> {
    installed_voices(false)
        .ok()
        .map(|voices| to_voices(&voices))
}

/// Resolve a macos_say `voice_id` exactly as synthesis does (full or short name, case-insensitive; listed
/// again once when not found): `Some(Ok(name to pass to say -v))`, `Some(Err(close matches, best first,
/// possibly empty))` for a voice that is not installed, or `None` when the voices cannot be listed here
/// (not macOS, `say` failing). Blocking: call it from a blocking context.
pub fn resolve_macos_voice(name: &str) -> Option<Result<String, Vec<String>>> {
    resolve_installed(name).ok()
}

fn to_voices(voices: &[SayVoice]) -> Vec<Voice> {
    voices
        .iter()
        .map(|v| Voice {
            id: v.name.clone(),
            name: v.name.clone(),
            language: Some(v.locale.clone()),
            gender: None,
            description: (!v.sample.is_empty()).then(|| v.sample.clone()),
        })
        .collect()
}

/// The error of a voice that is not installed.
pub(crate) fn unknown_voice(
    origin: &ProviderInstance,
    wanted: &str,
    suggestions: &[String],
) -> TtsError {
    let hint = if suggestions.is_empty() {
        "run `soniccraft voices macos_say` (or `say -v '?'`) to list installed voices".to_owned()
    } else {
        format!(
            "did you mean {}? `soniccraft voices macos_say` lists installed voices",
            suggestions
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    TtsError::new(
        TtsErrorKind::NotFound,
        origin,
        format!(
            "macOS voice `{wanted}` is not installed (say would silently use the default voice)"
        ),
    )
    .with_hint(hint)
}

#[cfg(target_os = "macos")]
fn say_program(ctx: &Ctx<'_>, origin: &ProviderInstance) -> Result<std::path::PathBuf, TtsError> {
    let fixed = std::path::PathBuf::from("/usr/bin/say");
    if fixed.is_file() {
        return Ok(fixed);
    }
    crate::process::find_program("say", ctx.env, std::path::Path::new("/")).ok_or_else(|| {
        TtsError::new(
            TtsErrorKind::Unavailable,
            origin,
            "the `say` program was not found",
        )
    })
}

#[cfg(not(target_os = "macos"))]
fn unavailable(origin: &ProviderInstance) -> TtsError {
    TtsError::new(
        TtsErrorKind::Unavailable,
        origin,
        "the macos_say provider only works on macOS",
    )
    .with_hint("use another provider (e.g. piper, kokoro or mock) on this platform")
}

#[cfg(target_os = "macos")]
pub(crate) async fn synthesize(
    ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    use crate::process::{self, Spawn};
    let origin = &req.provider;
    let wanted = resolved
        .voice_id
        .clone()
        .unwrap_or_else(|| "Samantha".to_owned());
    let lookup = wanted.clone();
    let found = tokio::task::spawn_blocking(move || resolve_installed(&lookup))
        .await
        .map_err(|e| {
            TtsError::new(
                TtsErrorKind::Provider,
                origin,
                format!("listing macOS voices failed: {e}"),
            )
        })?
        .map_err(|reason| TtsError::new(TtsErrorKind::Provider, origin, reason))?;
    let voice = found.map_err(|suggestions| unknown_voice(origin, &wanted, &suggestions))?;
    let say = say_program(ctx, origin)?;
    let workdir = tempfile::Builder::new()
        .prefix("soniccraft-say-")
        .tempdir()
        .map_err(|e| {
            TtsError::new(
                TtsErrorKind::Io,
                origin,
                format!("cannot create a temporary directory: {e}"),
            )
        })?;
    let text_file = workdir.path().join("text.txt");
    let output = workdir.path().join("output.wav");
    std::fs::write(&text_file, req.text.as_bytes()).map_err(|e| {
        TtsError::new(
            TtsErrorKind::Io,
            origin,
            format!("cannot write {}: {e}", text_file.display()),
        )
    })?;
    let mut args: Vec<std::ffi::OsString> = vec!["-v".into(), voice.into()];
    if let Some(rate) = rate_arg(&resolved.settings) {
        args.push("-r".into());
        args.push(rate.to_string().into());
    }
    args.extend(["-o".into(), output.clone().into_os_string()]);
    args.extend([
        "--file-format=WAVE".into(),
        "--data-format=LEI16@48000".into(),
        "-f".into(),
    ]);
    args.push(text_file.into_os_string());
    let env_remove = process::child_env_removals(origin, ctx.child_env_remove);
    let result = process::run(Spawn {
        program: &say,
        args: &args,
        cwd: None,
        stdin: None,
        timeout: crate::http::request_timeout(origin),
        env_remove: &env_remove,
        stdout_limit: crate::http::MAX_RESPONSE_BYTES,
    })
    .await
    .map_err(|e| super::command::run_error(origin, e))?;
    if !result.status.success() {
        return Err(TtsError::new(
            TtsErrorKind::Provider,
            origin,
            format!(
                "say failed with {}: {}",
                process::describe_status(&result.status),
                process::stderr_tail(&result.stderr)
            ),
        ));
    }
    let bytes = super::command::read_output_file(origin, &output)
        .await?
        .ok_or_else(|| {
            TtsError::new(
                TtsErrorKind::Provider,
                origin,
                format!("say did not write {}", output.display()),
            )
        })?;
    let audio = super::decode_encoded(origin, bytes, Some("wav")).await?;
    Ok(RawSynthesis {
        audio,
        words: None,
        notes: Vec::new(),
        output_format: Some("LEI16@48000".to_owned()),
    })
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn synthesize(
    _ctx: &Ctx<'_>,
    req: &SynthesisRequest,
    _resolved: &Resolved,
) -> Result<RawSynthesis, TtsError> {
    Err(unavailable(&req.provider))
}

#[cfg(target_os = "macos")]
pub(crate) async fn list_voices(origin: &ProviderInstance) -> Result<Vec<Voice>, TtsError> {
    let voices = tokio::task::spawn_blocking(|| installed_voices(true))
        .await
        .map_err(|e| {
            TtsError::new(
                TtsErrorKind::Provider,
                origin,
                format!("listing macOS voices failed: {e}"),
            )
        })?
        .map_err(|reason| TtsError::new(TtsErrorKind::Provider, origin, reason))?;
    Ok(to_voices(&voices))
}

#[cfg(not(target_os = "macos"))]
pub(crate) async fn list_voices(origin: &ProviderInstance) -> Result<Vec<Voice>, TtsError> {
    Err(unavailable(origin))
}

/// Offline availability for credential status.
pub(crate) fn availability() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err("the macos_say provider only works on macOS".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LISTING: &str = "Albert              en_US    # Hello! My name is Albert.\n\
Amélie              fr_CA    # Bonjour! Je m’appelle Amélie.\n\
Bad News            en_US    # Hello! My name is Bad News.\n\
Daniel (English (UK)) en_GB    # Hello! My name is Daniel.\n\
Daniel (English (UK)) en_GB    # Hello! My name is Daniel.\n\
Eddy (English (US)) en_US    # Hello! My name is Eddy.\n\
Eddy (German (Germany)) de_DE    # Hallo! Ich heiße Eddy.\n\
Grandma (Chinese (China mainland)) zh_CN    # 你好！我叫Grandma。\n\
Majed               ar_001   # مرحبًا! اسمي ماجد.\n\
Samantha (English (US)) en_US    # Hello! My name is Samantha.\n\
not a voice line\n";

    #[test]
    fn parses_listing_and_dedupes() {
        let voices = parse_voice_list(LISTING);
        let names: Vec<&str> = voices.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Albert",
                "Amélie",
                "Bad News",
                "Daniel (English (UK))",
                "Eddy (English (US))",
                "Eddy (German (Germany))",
                "Grandma (Chinese (China mainland))",
                "Majed",
                "Samantha (English (US))"
            ]
        );
        assert_eq!(voices[1].locale, "fr_CA");
        assert_eq!(voices[7].locale, "ar_001");
        assert_eq!(voices[0].sample, "Hello! My name is Albert.");
        assert_eq!(voices[3].short_name(), "Daniel");
    }

    #[test]
    fn resolves_full_and_short_names() {
        let voices = parse_voice_list(LISTING);
        assert_eq!(resolve_voice(&voices, "Albert").unwrap(), "Albert");
        assert_eq!(resolve_voice(&voices, "samantha").unwrap(), "Samantha");
        assert_eq!(
            resolve_voice(&voices, "Eddy (German (Germany))").unwrap(),
            "Eddy (German (Germany))"
        );
        assert_eq!(resolve_voice(&voices, "bad news").unwrap(), "Bad News");
        let suggestions = resolve_voice(&voices, "Samanta").unwrap_err();
        assert_eq!(suggestions.first().map(String::as_str), Some("Samantha"));
        assert!(resolve_voice(&voices, "Zzzz").unwrap_err().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn installed_voices_resolve_like_synthesis() {
        let voices = macos_installed_voices().expect("`say -v '?'` works on macOS");
        assert!(!voices.is_empty());
        let first = &voices[0];
        let short = first.id.split(" (").next().unwrap().to_owned();
        assert_eq!(
            resolve_macos_voice(&short.to_uppercase()),
            Some(Ok(short.clone())),
            "short names match case-insensitively"
        );
        assert_eq!(resolve_macos_voice(&first.id), Some(Ok(first.id.clone())));
        let typo = format!("{short}x");
        let suggestions = resolve_macos_voice(&typo).unwrap().unwrap_err();
        assert!(suggestions.contains(&short), "{typo}: {suggestions:?}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn installed_voices_are_unknown_off_macos() {
        assert_eq!(macos_installed_voices(), None);
        assert_eq!(resolve_macos_voice("Samantha"), None);
    }

    #[test]
    fn rate_from_settings() {
        let s = |v: Value| v.as_object().cloned().unwrap_or_default();
        assert_eq!(rate_arg(&s(json!({}))), None);
        assert_eq!(rate_arg(&s(json!({"rate": 200}))), Some(200));
        assert_eq!(rate_arg(&s(json!({"speed": 1.2}))), Some(210));
        assert_eq!(rate_arg(&s(json!({"speed": 1.0}))), None);
        assert_eq!(rate_arg(&s(json!({"rate": 150, "speed": 2}))), Some(150));
    }
}
