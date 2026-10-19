//! API keys: reading them from the environment (an unexpanded placeholder such as `${ELEVENLABS_API_KEY}`
//! counts as missing), the key-sending host policy, and scrubbing a request's key out of provider messages.
//!
//! **Host policy.** A key read from the environment is sent only to
//! 1. the provider's own API host: exactly the host of its built-in default base URL (`api.elevenlabs.io`,
//!    `api.openai.com`, `api.cartesia.ai`). Other hosts of the same company (regional or data-residency
//!    endpoints such as `api.eu.residency.elevenlabs.io`) are not assumed safe: a subdomain may be served by
//!    a third party, so they are listed in `SONICCRAFT_ALLOWED_HOSTS` like any other host;
//! 2. this machine (`localhost`, `*.localhost`, `127/8`, `::1`), but only the provider type's own key
//!    variable (`OPENAI_API_KEY` for an `openai` instance pointed at a local server, `KOKORO_API_KEY` for
//!    Kokoro). A timeline is data: it must not be able to send a variable of its choosing (`api_key_env:
//!    GITHUB_TOKEN`) to whatever listens on this machine, nor any key to a private-network host (RFC 1918,
//!    link-local, IPv6 unique-local, `*.local`), where another device on the network may listen. Both need
//!    rule 3;
//! 3. hosts listed in the environment variable `SONICCRAFT_ALLOWED_HOSTS` (comma-separated host names,
//!    `*.example.com` for subdomains, `*` for any host). It is read from SonicCraft's own environment, so a
//!    timeline can never widen the policy.
//!
//! Any other host gets no request at all: the call fails with [`crate::error_codes::KEY_HOST_NOT_ALLOWED`]
//! before anything is sent. Requests without a key (an optional key that is not set) may go anywhere.
//! Credential checks and voice listing follow the same policy as synthesis.

use soniccraft_core::ir::{ProviderInstance, ProviderType};

use crate::defaults;
use crate::env::EnvSource;
use crate::error::{TtsError, TtsErrorKind, error_codes};
use crate::hosts;

/// Environment variable listing extra hosts that may receive API keys (see the module docs).
pub const ALLOWED_HOSTS_ENV: &str = "SONICCRAFT_ALLOWED_HOSTS";

/// Keys shorter than this are not scrubbed from messages (real provider keys are far longer, and
/// replacing a two-letter test value would garble the message).
const MIN_SCRUBBED_KEY_LEN: usize = 8;

/// What the key variable of a provider instance holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyLookup {
    /// A usable key.
    Key { var: String, value: String },
    /// The variable is unset or empty.
    Unset { var: String },
    /// The variable holds an unexpanded placeholder (`${VAR}`, `$VAR`, `${env:VAR}`, `%VAR%`).
    Placeholder { var: String },
    /// The instance names no key variable and its type has no default.
    NoVariable,
}

/// The environment variable holding the instance's API key (instance `api_key_env`, else the default).
pub(crate) fn api_key_env(origin: &ProviderInstance) -> Option<String> {
    crate::settings::non_empty(origin.api_key_env.as_deref())
        .or_else(|| defaults::default_api_key_env(origin.provider_type))
        .map(str::to_owned)
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True for an environment-variable reference that was never expanded: `${NAME}`, `${NAME:-default}` (and
/// the other shell operators), `${env:NAME}` (VS Code style), `$NAME` and `%NAME%`. No real API key has this
/// shape, so such a value is a client config template passed through verbatim.
pub(crate) fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
        let inner = inner.strip_prefix("env:").unwrap_or(inner);
        let name_end = inner
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(inner.len());
        let (name, rest) = inner.split_at(name_end);
        return is_identifier(name)
            && (rest.is_empty() || rest.starts_with([':', '-', '=', '?', '+']));
    }
    if let Some(name) = value.strip_prefix('$') {
        return is_identifier(name);
    }
    if let Some(name) = value.strip_prefix('%').and_then(|v| v.strip_suffix('%')) {
        return is_identifier(name);
    }
    false
}

/// Look up the instance's key in `env`.
pub(crate) fn lookup(env: &EnvSource, origin: &ProviderInstance) -> KeyLookup {
    let Some(var) = api_key_env(origin) else {
        return KeyLookup::NoVariable;
    };
    match env.get(&var) {
        None => KeyLookup::Unset { var },
        Some(value) if is_placeholder(&value) => KeyLookup::Placeholder { var },
        Some(value) => KeyLookup::Key { var, value },
    }
}

/// Hint for a placeholder key (the value itself is never shown: a `${VAR:-default}` may hold a real key).
pub(crate) fn placeholder_hint(var: &str) -> String {
    format!(
        "{var} holds text like `${{{var}}}` instead of a key: the MCP client passed an unexpanded placeholder; \
         remove the env entry from the client config or set the variable (a stdio server inherits the \
         environment it is started from)"
    )
}

/// The key to send to `url`, or an [`TtsErrorKind::Auth`] error naming the variable to set, or a
/// [`error_codes::KEY_HOST_NOT_ALLOWED`] refusal.
pub(crate) fn require(
    env: &EnvSource,
    origin: &ProviderInstance,
    url: &str,
) -> Result<String, TtsError> {
    let display = defaults::display_name(origin.provider_type);
    match lookup(env, origin) {
        KeyLookup::Key { var, value } => {
            check_host(env, origin, &var, url, false)?;
            Ok(value)
        }
        KeyLookup::Placeholder { var } => Err(TtsError::new(
            TtsErrorKind::Auth,
            origin,
            format!("missing API key: environment variable {var} holds an unexpanded placeholder"),
        )
        .with_code(error_codes::PLACEHOLDER_KEY)
        .with_hint(placeholder_hint(&var))),
        KeyLookup::Unset { var } => Err(missing_key(origin, &var, display)),
        KeyLookup::NoVariable => Err(missing_key(origin, "API_KEY", display)),
    }
}

fn missing_key(origin: &ProviderInstance, var: &str, display: &str) -> TtsError {
    TtsError::new(
        TtsErrorKind::Auth,
        origin,
        format!("missing API key: environment variable {var} is not set"),
    )
    .with_hint(format!(
        "export {var}=<your {display} API key> (or point `api_key_env` on the provider at another variable); \
         keys never go in the timeline"
    ))
}

/// The optional key to send to `url`: `None` when the variable is unset or holds a placeholder (the request
/// goes without a key), a [`error_codes::KEY_HOST_NOT_ALLOWED`] refusal when a key is set but `url`'s host
/// may not receive it.
pub(crate) fn optional(
    env: &EnvSource,
    origin: &ProviderInstance,
    url: &str,
) -> Result<Option<String>, TtsError> {
    match lookup(env, origin) {
        KeyLookup::Key { var, value } => {
            check_host(env, origin, &var, url, true)?;
            Ok(Some(value))
        }
        KeyLookup::Placeholder { var } => {
            tracing::warn!("{}", placeholder_hint(&var));
            Ok(None)
        }
        KeyLookup::Unset { .. } | KeyLookup::NoVariable => Ok(None),
    }
}

/// The provider's own API host (`api.openai.com`), if it has a remote default.
fn official_host(t: ProviderType) -> Option<String> {
    let host = hosts::url_host(defaults::default_base_url(t)?)?;
    (!hosts::is_local_host(&host)).then_some(host)
}

/// True for exactly the provider's API host (`api.openai.com`), never a look-alike or a sibling host.
fn is_official_host(t: ProviderType, host: &str) -> bool {
    official_host(t).is_some_and(|official| host == official)
}

/// One `SONICCRAFT_ALLOWED_HOSTS` entry as a host pattern (lenient: URLs and `host:port` are accepted).
fn normalize_entry(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.contains("://") {
        return hosts::url_host(raw);
    }
    let without_port = match raw.rsplit_once(':') {
        // `host:8880` or `[::1]:8880`, but not a bare IPv6 address.
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && (!host.contains(':') || host.ends_with(']')) =>
        {
            host
        }
        _ => raw,
    };
    let host = without_port
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The `SONICCRAFT_ALLOWED_HOSTS` entries, normalized.
fn allowed_entries(env: &EnvSource) -> Vec<String> {
    env.get(ALLOWED_HOSTS_ENV)
        .map(|list| {
            list.split(|c: char| c == ',' || c.is_whitespace())
                .filter_map(normalize_entry)
                .collect()
        })
        .unwrap_or_default()
}

fn env_allows(env: &EnvSource, host: &str) -> bool {
    allowed_entries(env).iter().any(|entry| {
        entry == "*"
            || entry == host
            || entry
                .strip_prefix("*.")
                .is_some_and(|domain| host.ends_with(&format!(".{domain}")))
    })
}

/// Whether the key in `var` may be sent to `url` (see the module docs).
pub(crate) fn check_host(
    env: &EnvSource,
    origin: &ProviderInstance,
    var: &str,
    url: &str,
    optional_key: bool,
) -> Result<(), TtsError> {
    // An unparsable URL is never contacted (the request fails as an invalid URL), so nothing leaks.
    let Some(host) = hosts::url_host(url) else {
        return Ok(());
    };
    let t = origin.provider_type;
    let own_var = defaults::default_api_key_env(t);
    let own_key_to_this_machine = own_var == Some(var) && hosts::is_loopback_host(&host);
    if is_official_host(t, &host) || own_key_to_this_machine || env_allows(env, &host) {
        return Ok(());
    }
    let display = defaults::display_name(t);
    let this_machine = match own_var {
        Some(own) => format!("servers on this machine (only {own})"),
        None => "no other host".to_string(),
    };
    let destinations = match official_host(t) {
        Some(official) => format!(
            "{display}'s own API host ({official}), {this_machine}, or hosts listed in {ALLOWED_HOSTS_ENV}"
        ),
        None => format!("{this_machine}, or hosts listed in {ALLOWED_HOSTS_ENV}"),
    };
    let mut entries = allowed_entries(env);
    entries.push(host.clone());
    let mut hint = format!(
        "if you trust `{host}`, set {ALLOWED_HOSTS_ENV}={} in soniccraft's environment (never in the timeline)",
        entries.join(",")
    );
    if optional_key {
        hint.push_str(&format!(
            ", or unset {var} to send the request without a key"
        ));
    } else {
        hint.push_str(&format!(
            "; otherwise remove `base_url` from provider `{}` to use {display}'s own API",
            origin.name
        ));
    }
    Err(TtsError::new(
        TtsErrorKind::Unavailable,
        origin,
        format!(
            "refusing to send the API key in {var} to `{host}`: keys go only to {destinations}"
        ),
    )
    .with_code(error_codes::KEY_HOST_NOT_ALLOWED)
    .with_hint(hint))
}

/// Whether `provider` would send an API key to a host the key-sending policy allows (see the module docs),
/// checked offline: `Ok(())` when no key would be sent (local providers, no key in `env`, a placeholder) or
/// its destination is allowed; otherwise the [`error_codes::KEY_HOST_NOT_ALLOWED`] error synthesis, voice
/// listing and credential checks fail with (kind [`TtsErrorKind::Unavailable`]; message naming the variable
/// and host; hint naming `SONICCRAFT_ALLOWED_HOSTS`).
pub fn key_host_allowed(provider: &ProviderInstance, env: &EnvSource) -> Result<(), TtsError> {
    let Some(url) = crate::providers::endpoint_url(provider, env) else {
        return Ok(());
    };
    match lookup(env, provider) {
        KeyLookup::Key { var, .. } => {
            let optional_key = crate::providers::key_is_optional(provider);
            check_host(env, provider, &var, &url, optional_key)
        }
        _ => Ok(()),
    }
}

/// The values to scrub from messages about requests of `origin`: its key, when one is set.
pub(crate) fn secrets(env: &EnvSource, origin: &ProviderInstance) -> Vec<String> {
    match lookup(env, origin) {
        KeyLookup::Key { value, .. } => secrets_of(&value),
        _ => Vec::new(),
    }
}

/// `[key]` when it is long enough to scrub.
pub(crate) fn secrets_of(key: &str) -> Vec<String> {
    if key.len() >= MIN_SCRUBBED_KEY_LEN {
        vec![key.to_owned()]
    } else {
        Vec::new()
    }
}

/// `text` with every secret replaced by `<redacted>` and URL userinfo removed.
pub(crate) fn scrub_text(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() && out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    hosts::scrub_urls(&out)
}

/// `error` with its message, hint and code scrubbed (see [`scrub_text`]).
pub(crate) fn scrub_error(mut error: TtsError, secrets: &[String]) -> TtsError {
    error.message = scrub_text(&error.message, secrets);
    error.hint = error.hint.map(|h| scrub_text(&h, secrets));
    error.code = error.code.map(|c| {
        if secrets
            .iter()
            .any(|s| !s.is_empty() && c.contains(s.as_str()))
        {
            "<redacted>".to_owned()
        } else {
            c
        }
    });
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::instance;

    #[test]
    fn placeholders_are_recognized() {
        for p in [
            "${ELEVENLABS_API_KEY}",
            "${ELEVENLABS_API_KEY:-}",
            "${ELEVENLABS_API_KEY:-sk-default-1234567890}",
            "${OPENAI_API_KEY-x}",
            "${env:ELEVENLABS_API_KEY}",
            "$ELEVENLABS_API_KEY",
            "%OPENAI_API_KEY%",
            " ${_X1} ",
        ] {
            assert!(is_placeholder(p), "{p}");
        }
        for key in [
            "sk-proj-abc123",
            "sk_3f9a0000000000000000",
            "el-test-key",
            "$",
            "${}",
            "${1A}",
            "${A B}",
            "$not a var",
            "abc$DEF",
            "%%",
        ] {
            assert!(!is_placeholder(key), "{key}");
        }
    }

    #[test]
    fn lookup_and_required_keys() {
        let o = instance(ProviderType::Elevenlabs);
        let url = "https://api.elevenlabs.io";
        let env = |v: &str| EnvSource::fixed([("ELEVENLABS_API_KEY", v)]);
        assert_eq!(
            lookup(&env("sk_real"), &o),
            KeyLookup::Key {
                var: "ELEVENLABS_API_KEY".into(),
                value: "sk_real".into()
            }
        );
        let err = require(&env("${ELEVENLABS_API_KEY}"), &o, url).unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Auth);
        assert!(err.has_code(error_codes::PLACEHOLDER_KEY));
        let hint = err.hint.unwrap();
        assert!(hint.contains("unexpanded placeholder") && hint.contains("client config"));
        let err = require(&EnvSource::fixed([("X", "1")]), &o, url).unwrap_err();
        assert!(err.message.contains("ELEVENLABS_API_KEY is not set"));
        // Optional keys: placeholders and unset variables mean "send no key".
        let k = instance(ProviderType::Kokoro);
        let kokoro = |v: &str| EnvSource::fixed([("KOKORO_API_KEY", v)]);
        assert_eq!(
            optional(&kokoro("${KOKORO_API_KEY}"), &k, "http://localhost:8880").unwrap(),
            None
        );
        assert_eq!(
            optional(&kokoro("local-key-123"), &k, "http://localhost:8880").unwrap(),
            Some("local-key-123".into())
        );
    }

    #[test]
    fn host_policy() {
        let mut o = instance(ProviderType::Openai);
        let key = || EnvSource::fixed([("OPENAI_API_KEY", "sk-test-0123456789")]);
        let allowed = |o: &ProviderInstance, env: &EnvSource| key_host_allowed(o, env).is_ok();
        assert!(allowed(&o, &key()), "default base URL");
        for ok in [
            "https://api.openai.com/v1",
            "https://API.OpenAI.com:443/v1/",
            "http://localhost:8880/v1",
            "http://127.0.0.1:9/v1",
        ] {
            o.base_url = Some(ok.into());
            assert!(allowed(&o, &key()), "{ok}");
        }
        for refused in [
            // Another device on the local network may listen there.
            "http://10.0.0.5:8000/v1",
            "http://192.168.1.20:8000/v1",
            "http://gpu.local:8000/v1",
            "https://exfil.example/v1",
            "https://api.openai.com.exfil.example/v1",
            "https://evilopenai.com/v1",
            // Sibling hosts of the official one may be run by third parties (status pages, CDNs).
            "https://eu.api.openai.com/v1",
            "https://status.openai.com/v1",
        ] {
            o.base_url = Some(refused.into());
            let err = key_host_allowed(&o, &key()).unwrap_err();
            assert_eq!(err.kind, TtsErrorKind::Unavailable, "{refused}");
            assert!(err.has_code(error_codes::KEY_HOST_NOT_ALLOWED));
            assert!(err.message.contains("OPENAI_API_KEY"), "{}", err.message);
            let host = url_host_of(refused);
            assert!(
                err.message.contains(&format!("`{host}`")),
                "{}",
                err.message
            );
            let hint = err.hint.unwrap();
            assert!(
                hint.contains(&format!("SONICCRAFT_ALLOWED_HOSTS={host}"))
                    && hint.contains("never in the timeline"),
                "{hint}"
            );
            assert!(!err.message.contains("sk-test-0123456789"));
        }
        o.base_url = Some("https://tts.example.net:8443/v1".into());
        assert!(!allowed(&o, &key()));
        let with = |list: &str| {
            EnvSource::fixed([
                ("OPENAI_API_KEY", "sk-test-0123456789"),
                (ALLOWED_HOSTS_ENV, list),
            ])
        };
        for list in [
            "tts.example.net",
            "other.org, TTS.EXAMPLE.NET",
            "*.example.net",
            "*",
            "https://tts.example.net:8443",
            "tts.example.net:8443",
        ] {
            assert!(allowed(&o, &with(list)), "{list}");
        }
        assert!(!allowed(&o, &with("example.net")));
        let err = key_host_allowed(&o, &with("a.org")).unwrap_err();
        assert!(
            err.hint
                .unwrap()
                .contains("SONICCRAFT_ALLOWED_HOSTS=a.org,tts.example.net")
        );
        // No key, no policy: keyless requests may go anywhere.
        assert!(allowed(&o, &EnvSource::fixed([("X", "1")])));
        assert!(allowed(
            &o,
            &EnvSource::fixed([("OPENAI_API_KEY", "${OPENAI_API_KEY}")])
        ));
        // A key chosen by the timeline is held to the same policy (the reviewer's exfiltration case), and it
        // does not even go to this machine: only the provider type's own variable does.
        o.api_key_env = Some("GITHUB_TOKEN".into());
        let token = |extra: &[(&str, &str)]| {
            let mut vars = vec![("GITHUB_TOKEN", "ghp_FAKE_LEAKCHECK")];
            vars.extend_from_slice(extra);
            EnvSource::fixed(vars)
        };
        for host in [
            "http://exfil.example:18732/v1",
            "http://127.0.0.1:18732/v1",
            "http://localhost:9/v1",
        ] {
            o.base_url = Some(host.into());
            let err = key_host_allowed(&o, &token(&[])).unwrap_err();
            assert!(
                err.message.contains("GITHUB_TOKEN"),
                "{host}: {}",
                err.message
            );
            assert!(
                err.message.contains("only OPENAI_API_KEY"),
                "{}",
                err.message
            );
            assert!(!err.message.contains("ghp_FAKE_LEAKCHECK"));
        }
        // Listing the host is the explicit opt-in for a custom variable on a local server.
        o.base_url = Some("http://127.0.0.1:18732/v1".into());
        assert!(allowed(&o, &token(&[(ALLOWED_HOSTS_ENV, "127.0.0.1")])));
        o.api_key_env = Some("OPENAI_API_KEY".into());
        assert!(
            allowed(&o, &key()),
            "the provider's own variable may go to this machine"
        );
        // Regional ElevenLabs endpoints are listed explicitly; Kokoro has no official remote host.
        let mut el = instance(ProviderType::Elevenlabs);
        el.base_url = Some("https://api.eu.residency.elevenlabs.io".into());
        let el_key = |extra: &[(&str, &str)]| {
            let mut vars = vec![("ELEVENLABS_API_KEY", "sk_123456789")];
            vars.extend_from_slice(extra);
            EnvSource::fixed(vars)
        };
        let err = key_host_allowed(&el, &el_key(&[])).unwrap_err();
        assert!(
            err.hint
                .as_deref()
                .unwrap()
                .contains("SONICCRAFT_ALLOWED_HOSTS=api.eu.residency.elevenlabs.io"),
            "{err:?}"
        );
        assert!(allowed(
            &el,
            &el_key(&[(ALLOWED_HOSTS_ENV, "api.eu.residency.elevenlabs.io")])
        ));
        let mut k = instance(ProviderType::Kokoro);
        k.base_url = Some("http://gpu-box:8880".into());
        let err = key_host_allowed(&k, &EnvSource::fixed([("KOKORO_API_KEY", "kokoro-key-1")]))
            .unwrap_err();
        assert!(err.hint.unwrap().contains("unset KOKORO_API_KEY"));
        assert!(
            key_host_allowed(
                &k,
                &EnvSource::fixed([("KOKORO_BASE_URL", "http://x.example")])
            )
            .is_ok(),
            "no key set"
        );
        // Local processes never send keys.
        assert!(key_host_allowed(&instance(ProviderType::Piper), &key()).is_ok());
    }

    fn url_host_of(url: &str) -> String {
        hosts::url_host(url).unwrap()
    }

    #[test]
    fn scrubbing() {
        let secrets = secrets_of("sk-test-LEAKCHECK-0123456789");
        let error = TtsError::for_provider(
            TtsErrorKind::Auth,
            "openai",
            ProviderType::Openai,
            "Incorrect API key provided: Bearer sk-test-LEAKCHECK-0123456789 at http://u:pw@127.0.0.1:9/v1",
        )
        .with_code("sk-test-LEAKCHECK-0123456789")
        .with_hint("check http://u:pw@127.0.0.1:9/v1");
        let clean = scrub_error(error, &secrets);
        assert_eq!(
            clean.message,
            "Incorrect API key provided: Bearer <redacted> at http://127.0.0.1:9/v1"
        );
        assert_eq!(clean.code.as_deref(), Some("<redacted>"));
        assert_eq!(clean.hint.as_deref(), Some("check http://127.0.0.1:9/v1"));
        assert!(secrets_of("k").is_empty(), "short values are not scrubbed");
    }
}
