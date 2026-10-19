//! Secret detection (DESIGN.md §2.8 "Secrets never live in timelines") and redaction.
//!
//! Flagged as `secret_in_timeline` errors:
//! - any key that names a secret: `api_key`, `apikey`, `token`, `secret`, `password` (also `api-key`,
//!   `apiKey`, `access_token`, `client_secret`, `aws_secret_access_key`, `Authorization`, … — case,
//!   separators and an annotation prefix are ignored, so `x-api-key` and `x_token` count) anywhere in the
//!   document, annotations included, except the user-chosen names directly under `speakers` / `providers`;
//! - any mapping key that looks like a provider key (`sk_live_…: true`), names included; such keys are
//!   also redacted from diagnostic paths;
//! - any string value that looks like a provider key: `sk-…` / `sk_…` followed by 16+ key characters
//!   (OpenAI, Anthropic, ElevenLabs, Cartesia key formats);
//! - inside provider-bound strings — a `providers` entry, or any `settings` map (both are sent to the
//!   provider): an `sk-`/`sk_` key anywhere in the string (after `Bearer `, in an `--api-key=…` argument, in
//!   a URL), a `Bearer <token>`, a string of 32+ hexadecimal characters (legacy ElevenLabs keys), the
//!   password of a URL userinfo (`https://user:pass@host`) and URL query parameters with secret names
//!   (`?api_key=…`, `?key=…`).
//!
//! Secret values are never echoed: [`redact`] replaces them in diagnostic messages, hints, related ids and
//! paths, and type errors describe them as "a string that looks like a secret". [`secret_spans`] gives
//! the CLI the source ranges to mask in snippets.

use std::collections::HashSet;
use std::ops::Range;

use serde_json::Value;

use super::SourceFormat;
use crate::diagnostics::{Diagnostic, codes};
use crate::fields;
use crate::providers;

/// Result of [`scan`].
#[derive(Debug, Default)]
pub(crate) struct SecretScan {
    /// One `secret_in_timeline` error per finding.
    pub diagnostics: Vec<Diagnostic>,
    /// Paths of secret-named and secret-shaped keys; their `unknown_field` diagnostics are redundant.
    pub key_paths: HashSet<String>,
    /// Secret strings (values, tokens inside values, secret-shaped keys), for [`redact`].
    pub values: Vec<String>,
    /// Where the secrets are, for [`secret_spans`].
    pub findings: Vec<Finding>,
}

/// One secret in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// Path of the value (or of the entry, for a secret-shaped key).
    pub path: String,
    /// True when the key itself is the secret (`sk_live_…: true`).
    pub key: bool,
}

/// Placeholder written instead of a secret value.
pub(crate) const REDACTED: &str = "<redacted>";

/// Minimum length of a value that [`redact`] replaces (shorter strings are too ambiguous to scrub).
const MIN_REDACT_LEN: usize = 8;

/// Minimum number of key characters after `sk-` / `sk_`.
const MIN_KEY_CHARS: usize = 16;

/// Lowercase words of a key: split at separators and at camelCase / acronym boundaries
/// (`AWSSecretAccessKey` → `aws`, `secret`, `access`, `key`).
fn key_words(key: &str) -> Vec<String> {
    let chars: Vec<char> = key.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let boundary = c.is_ascii_uppercase()
            && prev.is_some_and(|p| {
                p.is_ascii_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase()))
            });
        if boundary && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        word.push(c.to_ascii_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// True for key names that hold secrets (an annotation prefix `x-` / `x_` is ignored).
pub(crate) fn is_secret_key(key: &str) -> bool {
    const EXACT: &[&str] = &[
        "apikey",
        "token",
        "secret",
        "password",
        "passwd",
        "accesstoken",
        "authtoken",
        "apitoken",
        "bearertoken",
        "secretkey",
        "apisecret",
        "clientsecret",
        "privatekey",
        "xiapikey",
        "authorization",
        "auth",
        "credentials",
        "credential",
    ];
    const LAST: &[&str] = &[
        "token",
        "secret",
        "password",
        "passwd",
        "passphrase",
        "authorization",
        "credentials",
        "credential",
    ];
    const KEY_QUALIFIERS: &[&str] = &["api", "secret", "private", "access", "auth", "subscription"];
    let key = if fields::is_annotation(key) {
        &key[2..]
    } else {
        key
    };
    let words = key_words(key);
    let joined = words.concat();
    if EXACT.contains(&joined.as_str()) {
        return true;
    }
    match words.as_slice() {
        [.., last] if LAST.contains(&last.as_str()) => true,
        [.., qualifier, last] => last == "key" && KEY_QUALIFIERS.contains(&qualifier.as_str()),
        _ => false,
    }
}

fn is_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// True for strings shaped like a provider API key (`sk-…`, `sk_…`).
pub(crate) fn looks_like_provider_key(s: &str) -> bool {
    let t = s.trim();
    match t.strip_prefix("sk-").or_else(|| t.strip_prefix("sk_")) {
        Some(rest) => rest.len() >= MIN_KEY_CHARS && rest.bytes().all(is_key_char),
        None => false,
    }
}

/// Byte ranges of the `sk-…` / `sk_…` keys anywhere in `s` (at a word boundary, 16+ key characters).
pub(crate) fn provider_key_tokens(s: &str) -> Vec<Range<usize>> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let starts = bytes[i] == b's'
            && bytes[i + 1] == b'k'
            && matches!(bytes[i + 2], b'-' | b'_')
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric());
        if starts {
            let end = i
                + 3
                + bytes[i + 3..]
                    .iter()
                    .take_while(|&&b| is_key_char(b))
                    .count();
            if end - (i + 3) >= MIN_KEY_CHARS {
                out.push(i..end);
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// True for strings of 32+ hexadecimal characters.
pub(crate) fn looks_like_hex_key(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 32 && t.chars().all(|c| c.is_ascii_hexdigit())
}

/// True when a string must not be echoed in diagnostics.
pub(crate) fn looks_like_secret_value(s: &str) -> bool {
    looks_like_provider_key(s) || looks_like_hex_key(s) || !provider_key_tokens(s).is_empty()
}

/// `s` with every key-shaped token (`sk-…`, 32+ hex characters) replaced by [`REDACTED`], for echoing raw
/// source text (comments) in messages.
pub(crate) fn mask_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for r in provider_key_tokens(s) {
        out.push_str(&s[last..r.start]);
        out.push_str(REDACTED);
        last = r.end;
    }
    out.push_str(&s[last..]);
    out.split(' ')
        .map(|w| if looks_like_hex_key(w) { REDACTED } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A secret embedded in a provider-bound string, with what it is (for the message).
fn embedded_secret(s: &str) -> Option<(&'static str, Vec<String>)> {
    let tokens: Vec<String> = provider_key_tokens(s)
        .into_iter()
        .map(|r| s[r].to_string())
        .collect();
    if !tokens.is_empty() {
        return Some(("contains an API key", tokens));
    }
    if let Some(token) = bearer_token(s) {
        return Some(("contains a bearer token", vec![token.to_string()]));
    }
    if let Some(password) = url_password(s) {
        return Some(("contains a password in the URL", vec![password.to_string()]));
    }
    if let Some(value) = url_secret_param(s) {
        return Some(("contains a secret URL parameter", vec![value.to_string()]));
    }
    None
}

/// The token after `Bearer ` (8+ characters), if any.
fn bearer_token(s: &str) -> Option<&str> {
    let lower = s.to_ascii_lowercase();
    let at = lower.find("bearer ")?;
    let rest = s[at + "bearer ".len()..].trim_start();
    let token = rest.split_whitespace().next()?;
    (token.len() >= 8).then_some(token)
}

/// The authority part of a URL (`user:pass@host:port`), if `s` contains `scheme://`.
fn url_authority(s: &str) -> Option<&str> {
    let after = &s[s.find("://")? + 3..];
    let end = after.find(['/', '?', '#']).unwrap_or(after.len());
    Some(&after[..end])
}

/// The password of a URL userinfo (`https://user:pass@host`), if non-empty.
fn url_password(s: &str) -> Option<&str> {
    let authority = url_authority(s)?;
    let (userinfo, _) = authority.rsplit_once('@')?;
    let (_, password) = userinfo.split_once(':')?;
    (!password.is_empty()).then_some(password)
}

/// The value of a URL query parameter whose name holds a secret (`?api_key=…`, `?key=…`).
fn url_secret_param(s: &str) -> Option<&str> {
    url_authority(s)?;
    let query = &s[s.find('?')? + 1..];
    let query = query.split('#').next().unwrap_or(query);
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        let secret_name = is_secret_key(name) || name.eq_ignore_ascii_case("key");
        (secret_name && !value.is_empty()).then_some(value)
    })
}

/// Scan a whole document.
pub(crate) fn scan(root: &Value) -> SecretScan {
    let mut out = SecretScan::default();
    walk(root, root, "", false, &mut out);
    out
}

fn child(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// Is a value at `path` sent to a provider (a `providers` entry or a `settings` map)?
fn provider_bound(path: &str) -> bool {
    path.starts_with("providers.")
}

fn walk(root: &Value, value: &Value, path: &str, bound: bool, out: &mut SecretScan) {
    match value {
        Value::Object(map) => {
            let is_name_map = path == "speakers" || path == "providers";
            for (key, v) in map {
                let p = child(path, key);
                if looks_like_secret_value(key) {
                    out.diagnostics.push(
                        Diagnostic::error(
                            codes::SECRET_IN_TIMELINE,
                            format!(
                                "a key in `{}` looks like an API key; secrets must never be stored in a timeline",
                                if path.is_empty() { "the top level" } else { path }
                            ),
                        )
                        .with_path(p.clone())
                        .with_hint(
                            "Delete this key from the file: keep API keys in environment variables and reference \
                             them with `api_key_env` in a `providers:` entry",
                        ),
                    );
                    out.key_paths.insert(p.clone());
                    out.values.push(key.clone());
                    out.findings.push(Finding {
                        path: p.clone(),
                        key: true,
                    });
                    // The value is not the secret, but may hold other ones.
                    let child_bound = bound || provider_bound(&p);
                    walk(root, v, &p, child_bound, out);
                    continue;
                }
                let exempt_name = is_name_map && !fields::is_annotation(key);
                if !exempt_name && is_secret_key(key) {
                    out.diagnostics.push(
                        Diagnostic::error(
                            codes::SECRET_IN_TIMELINE,
                            format!(
                                "`{key}` holds a secret; API keys, tokens and passwords must never be stored in a \
                                 timeline"
                            ),
                        )
                        .with_path(p.clone())
                        .with_hint(env_hint(root, &p, key)),
                    );
                    out.key_paths.insert(p.clone());
                    collect_strings(v, &mut out.values);
                    out.findings.push(Finding {
                        path: p,
                        key: false,
                    });
                    continue;
                }
                let child_bound =
                    bound || provider_bound(&p) || (!is_name_map && key == "settings");
                walk(root, v, &p, child_bound, out);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                walk(root, v, &format!("{path}[{i}]"), bound, out);
            }
        }
        Value::String(s) => {
            let field = last_segment(path);
            let whole = looks_like_provider_key(s) || (bound && looks_like_hex_key(s));
            let embedded = if whole || !bound {
                None
            } else {
                embedded_secret(s)
            };
            if !whole && embedded.is_none() {
                return;
            }
            let what = embedded
                .as_ref()
                .map_or("looks like an API key", |(what, _)| what);
            let message =
                format!("the value of `{path}` {what}; secrets must never be stored in a timeline");
            let hint = if field == "api_key_env" {
                let var = provider_env_var(root, path).unwrap_or("ELEVENLABS_API_KEY");
                format!(
                    "`api_key_env` takes the NAME of an environment variable, not the key itself: set \
                     `api_key_env: {var}`, remove the key from the file and run `export {var}=...` in your shell"
                )
            } else if field == "base_url" {
                format!(
                    "Remove the credentials from `base_url` (keep only scheme, host and path); {}",
                    lowercase_first(&env_hint(root, path, field))
                )
            } else if path.contains(".command[") {
                format!(
                    "Remove the key from `{field}` and let the program read it from an environment variable of \
                     its own (`export MY_TTS_KEY=...` before running soniccraft): programs inherit soniccraft's \
                     environment except the built-in providers' key variables (ELEVENLABS_API_KEY, OPENAI_API_KEY, \
                     CARTESIA_API_KEY, KOKORO_API_KEY) and every `api_key_env` of the timeline"
                )
            } else {
                env_hint(root, path, field)
            };
            out.diagnostics.push(
                Diagnostic::error(codes::SECRET_IN_TIMELINE, message)
                    .with_path(path.to_string())
                    .with_hint(hint),
            );
            out.values.push(s.clone());
            if let Some((_, parts)) = embedded {
                out.values.extend(parts);
            }
            out.findings.push(Finding {
                path: path.to_string(),
                key: false,
            });
        }
        _ => {}
    }
}

/// The last element of a path for messages: `api_key`, `command[2]`.
fn last_segment(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => {
            for (k, v) in map {
                if looks_like_secret_value(k) {
                    out.push(k.clone());
                }
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

/// The provider instance name of a path inside `providers.<name>`.
fn provider_name(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("providers.")?;
    Some(rest.split(['.', '[']).next().unwrap_or(rest))
}

/// The default API-key env var of the provider block containing `path`, if it is a known type.
fn provider_env_var(root: &Value, path: &str) -> Option<&'static str> {
    let name = provider_name(path)?;
    let block = root.get("providers")?.get(name)?;
    let type_name = block.get("type").and_then(Value::as_str).unwrap_or(name);
    let provider_type = providers::builtin_type(type_name)?;
    providers::meta(provider_type).default_api_key_env
}

fn env_hint(root: &Value, path: &str, key: &str) -> String {
    match (provider_name(path), provider_env_var(root, path)) {
        (Some(name), Some(var)) => format!(
            "Delete `{key}` from the file, export the key in your shell (`export {var}=...`) and reference the \
             variable with `api_key_env: {var}` in `providers.{name}`"
        ),
        (Some(name), None) => format!(
            "Delete `{key}` from the file, store the key in an environment variable and reference it with \
             `api_key_env: <VARIABLE_NAME>` in `providers.{name}`"
        ),
        _ => format!(
            "Delete `{key}` from the file; keep API keys in environment variables and reference them with \
             `api_key_env` in a `providers:` entry, e.g. `providers: {{elevenlabs: {{api_key_env: ELEVENLABS_API_KEY}}}}`"
        ),
    }
}

/// Replace every occurrence of a secret value in diagnostic messages, hints, related ids and paths.
pub(crate) fn redact(diagnostics: &mut [Diagnostic], secrets: &[String]) {
    let mut secrets: Vec<&str> = secrets
        .iter()
        .map(|s| s.trim())
        .filter(|s| s.len() >= MIN_REDACT_LEN)
        .collect();
    if secrets.is_empty() {
        return;
    }
    // Longest first, so a secret that contains another one is replaced as a whole.
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    secrets.dedup();
    let scrub = |text: &mut String| {
        for secret in &secrets {
            if text.contains(secret) {
                *text = text.replace(secret, REDACTED);
            }
        }
    };
    for d in diagnostics {
        scrub(&mut d.message);
        if let Some(hint) = d.hint.as_mut() {
            scrub(hint);
        }
        for related in &mut d.related {
            scrub(related);
        }
        if let Some(path) = d.path.as_mut() {
            scrub(path);
        }
    }
}

/// Byte ranges in `text` of every secret that [`super::parse_timeline`] reports as `secret_in_timeline`
/// (values, secret-shaped keys, and every key and scalar inside a secret-named key's mapping or list), plus —
/// so that snippets of documents that do not parse are masked too — every `sk-…` / `sk_…` key-shaped token
/// anywhere in the raw text. Sorted and non-overlapping (adjacent or overlapping ranges are merged). YAML and
/// JSON (`format` is resolved like [`super::parse_timeline`] does, including the auto-format fallback).
///
/// Quoted scalars include their quotes; YAML block scalars cover their content lines.
pub fn secret_spans(text: &str, format: SourceFormat) -> Vec<Range<usize>> {
    let mut out = provider_key_tokens(text);
    if let Some((value, format)) = super::document_value(text, format) {
        let scan = scan(&value);
        let mut values = HashSet::new();
        let mut keys = HashSet::new();
        for f in scan.findings {
            if f.key {
                keys.insert(f.path);
            } else {
                values.insert(f.path);
            }
        }
        if !values.is_empty() || !keys.is_empty() {
            out.extend(match format {
                SourceFormat::Json => super::json::ranges(text, &values, &keys),
                SourceFormat::Yaml | SourceFormat::Auto => {
                    super::yaml::ranges(text, &values, &keys)
                }
            });
        }
    }
    out.retain(|r| r.start < r.end && r.end <= text.len());
    out.sort_by_key(|r| (r.start, r.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
    for r in out {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_names() {
        for k in [
            "api_key",
            "apikey",
            "API_KEY",
            "apiKey",
            "api-key",
            "token",
            "secret",
            "password",
            "client_secret",
            "x-api-key",
            "x-token",
            "x_secret",
            "x-password",
            "x-auth-token",
            "xi-api-key",
            "aws_secret_access_key",
            "AWSSecretAccessKey",
            "refresh_token",
            "Authorization",
            "auth",
            "db_password",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in [
            "api_key_env",
            "tokens_per_minute",
            "secretary",
            "voice_id",
            "x-note",
            "max_tokens",
            "token_limit",
            "key",
            "x-voice-notes",
            "authors",
        ] {
            assert!(!is_secret_key(k), "{k}");
        }
    }

    #[test]
    fn value_shapes() {
        assert!(looks_like_provider_key("sk-proj-abcdefghijklmnopqrstuvwx"));
        assert!(looks_like_provider_key("sk_car_0123456789abcdefABCDEF"));
        assert!(!looks_like_provider_key("sk-short"));
        assert!(!looks_like_provider_key(
            "sk-learn is a python library for ML"
        ));
        assert!(looks_like_hex_key("0123456789abcdef0123456789abcdef"));
        assert!(!looks_like_hex_key("21m00Tcm4TlvDq8ikWAM"));
        assert!(!looks_like_hex_key("6ccbfb76-1fc6-48f7-b71d-91ac6298247b"));
        // Embedded keys: only at a word boundary.
        let s = "Bearer sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789";
        assert_eq!(&s[provider_key_tokens(s)[0].clone()], &s[7..]);
        assert!(provider_key_tokens("--api-key=sk_3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b").len() == 1);
        assert!(
            provider_key_tokens("task-0123456789abcdefghij risk_0123456789abcdefghij").is_empty()
        );
        assert!(looks_like_secret_value("token: sk-AbCdEf0123456789xyzXYZ"));
    }

    #[test]
    fn url_credentials() {
        assert_eq!(
            url_password("https://user:hunter2@example.com/v1"),
            Some("hunter2")
        );
        assert_eq!(url_password("https://user@example.com/v1"), None);
        assert_eq!(url_password("http://localhost:8880/v1"), None);
        assert_eq!(url_password("https://example.com/v1?x=a:b@c"), None);
        assert_eq!(
            url_secret_param("https://e.com/v1?api-key=abc123&x=1"),
            Some("abc123")
        );
        assert_eq!(
            url_secret_param("https://e.com/v1?key=AIzaSyA"),
            Some("AIzaSyA")
        );
        assert_eq!(url_secret_param("https://e.com/v1?voice=x"), None);
        assert_eq!(
            bearer_token("Authorization: Bearer abcdefghijkl"),
            Some("abcdefghijkl")
        );
        assert_eq!(bearer_token("bearer short"), None);
    }

    #[test]
    fn scan_finds_keys_and_values() {
        let doc = json!({
            "providers": {
                "eleven": {"type": "elevenlabs", "api_key": "abcdefghijklmnop"},
                "oa": {"type": "openai", "api_key_env": "sk-proj-abcdefghijklmnopqrstuvwx"},
                "legacy": {"type": "elevenlabs", "settings": {"key": "0123456789abcdef0123456789abcdef"}}
            },
            "speakers": {"token": {"provider": "mock"}},
            "tracks": [{"id": "a", "type": "speech", "sequence": [{"text": "sk-not a key, just words here"}]}]
        });
        let s = scan(&doc);
        let paths: Vec<&str> = s
            .diagnostics
            .iter()
            .filter_map(|d| d.path.as_deref())
            .collect();
        assert_eq!(
            paths,
            [
                "providers.eleven.api_key",
                "providers.oa.api_key_env",
                "providers.legacy.settings.key"
            ]
        );
        assert!(
            s.diagnostics[0]
                .hint
                .as_deref()
                .unwrap()
                .contains("api_key_env: ELEVENLABS_API_KEY")
        );
        assert!(
            s.diagnostics[1]
                .hint
                .as_deref()
                .unwrap()
                .contains("api_key_env: OPENAI_API_KEY")
        );
        assert!(s.key_paths.contains("providers.eleven.api_key"));
        for d in &s.diagnostics {
            assert!(!d.message.contains("abcdefghijklmnop"), "{}", d.message);
        }
    }

    #[test]
    fn annotation_and_secret_shaped_keys() {
        let doc = json!({
            "x-api-key": "9f86d081884c7d659a2feaa0c55ad015",
            "x_token": "plaintextsecretvalue",
            "x-notes": {"password": "hunter2hunter2", "nested": [{"Token": "tok_ABCDEFGHIJKL"}]},
            "speakers": {"x-auth-token": "eyJhbGciOiJIUzI1NiJ9", "narrator": {}},
            "providers": {"eleven": {"type": "elevenlabs", "sk_live_0123456789abcdefABCDEF": true}},
        });
        let s = scan(&doc);
        let paths: Vec<&str> = s
            .diagnostics
            .iter()
            .filter_map(|d| d.path.as_deref())
            .collect();
        assert_eq!(
            paths,
            [
                "x-api-key",
                "x_token",
                "x-notes.password",
                "x-notes.nested[0].Token",
                "speakers.x-auth-token",
                "providers.eleven.sk_live_0123456789abcdefABCDEF"
            ]
        );
        let key = s.diagnostics.last().unwrap();
        assert!(!key.message.contains("sk_live"), "{}", key.message);
        assert!(
            s.key_paths
                .contains("providers.eleven.sk_live_0123456789abcdefABCDEF")
        );
        // Redaction covers the path too.
        let mut d = s.diagnostics.clone();
        redact(&mut d, &s.values);
        assert_eq!(
            d.last().unwrap().path.as_deref(),
            Some("providers.eleven.<redacted>")
        );
    }

    #[test]
    fn provider_bound_strings() {
        let doc = json!({
            "providers": {
                "openai": {
                    "base_url": "https://user:sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789@example.com/v1",
                    "settings": {"extra_body": {"auth_header": "Bearer sk_3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e"}}
                },
                "q": {"type": "openai", "base_url": "https://example.com/v1?api-key=abcdef123456"},
                "pw": {"type": "kokoro", "base_url": "http://soniccraft:LEAKCHECKpassw0rd@127.0.0.1:9"},
                "c": {"type": "command", "command": ["my-tts", "--api-key=sk_3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c", "{output}"]}
            },
            "defaults": {"settings": {"key": "0123456789abcdef0123456789abcdef", "h": "Bearer abcdefghijkl"}},
            "tracks": [{"id": "a", "type": "speech", "sequence": [
                {"text": "my key is sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789 ok",
                 "settings": {"instructions": "use sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123456789"}}
            ]}]
        });
        let s = scan(&doc);
        let paths: Vec<&str> = s
            .diagnostics
            .iter()
            .filter_map(|d| d.path.as_deref())
            .collect();
        assert_eq!(
            paths,
            [
                "providers.openai.base_url",
                "providers.openai.settings.extra_body.auth_header",
                "providers.q.base_url",
                "providers.pw.base_url",
                "providers.c.command[1]",
                "defaults.settings.key",
                "defaults.settings.h",
                "tracks[0].sequence[0].settings.instructions"
            ],
            "cue text is not provider-bound: only whole-value keys count there"
        );
        let cmd = &s.diagnostics[4];
        let cmd_hint = cmd.hint.as_deref().unwrap();
        assert!(cmd_hint.contains("`command[1]`"), "{cmd_hint}");
        // Programs do not see the built-in key variables and take no `api_key_env`: the hint must not
        // suggest either.
        assert!(
            cmd_hint.contains("except the built-in providers' key variables")
                && !cmd_hint.contains("api_key_env:"),
            "{cmd_hint}"
        );
        let url = &s.diagnostics[3];
        assert!(
            url.message.contains("password in the URL"),
            "{}",
            url.message
        );
        assert!(s.values.iter().any(|v| v == "LEAKCHECKpassw0rd"));
    }

    #[test]
    fn redaction() {
        let mut d = vec![
            Diagnostic::error("x", "bad value sk-proj-abcdefghijklmnopqrstuvwx here")
                .with_hint("use sk-proj-abcdefghijklmnopqrstuvwx"),
        ];
        redact(
            &mut d,
            &[
                "sk-proj-abcdefghijklmnopqrstuvwx".to_string(),
                "short".to_string(),
            ],
        );
        assert_eq!(d[0].message, "bad value <redacted> here");
        assert_eq!(d[0].hint.as_deref(), Some("use <redacted>"));
    }

    #[test]
    fn masking_raw_text() {
        assert_eq!(
            mask_tokens("#1 sk-proj-AbCdEfGhIjKlMnOpQrStUvWx and 0123456789abcdef0123456789abcdef"),
            "#1 <redacted> and <redacted>"
        );
        assert_eq!(mask_tokens("#1: always duck"), "#1: always duck");
    }
}
