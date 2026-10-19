//! `providers()` and `voices()`: the built-in provider registry (`soniccraft_core::providers`) combined
//! with what this environment offers (credentials, binaries, platform, local servers that answer).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Map;
use soniccraft_core::ir::{ProviderInstance, ProviderType};
use soniccraft_core::{diagnostics, providers};
use soniccraft_tts::{CredentialStatus, TtsClient, TtsOptions};

use crate::checks::{ALLOW_COMMAND_HINT, SERVER_PROBE_TIMEOUT, server_name};
use crate::context::absolute;
use crate::report::{ProviderStatus, ProvidersReport, VoiceInfo, VoicesReport};
use crate::{Engine, EngineError};

/// A TTS client without a cache, reading the engine's environment.
fn client(engine: &Engine) -> TtsClient {
    TtsClient::new(TtsOptions {
        cache_dir: None,
        jobs: engine.options.jobs,
        env: engine.env.clone(),
        ..TtsOptions::default()
    })
}

/// The instance a timeline gets when it names a built-in provider without configuring it.
pub(crate) fn default_instance(engine: &Engine, provider_type: ProviderType) -> ProviderInstance {
    let meta = providers::meta(provider_type);
    let base_url = meta
        .base_url_env
        .and_then(|var| engine.env.get(var))
        .or_else(|| meta.default_base_url.map(str::to_string));
    ProviderInstance {
        name: provider_type.as_str().to_string(),
        provider_type,
        configured: false,
        api_key_env: meta.default_api_key_env.map(str::to_string),
        base_url,
        model: meta.default_model.map(str::to_string),
        output_format: None,
        max_concurrency: meta.default_max_concurrency,
        timeout_ms: providers::DEFAULT_TIMEOUT_MS,
        chars_per_second: None,
        words_per_minute: None,
        settings: Map::new(),
        command: None,
        stdin: None,
        voices_dir: None,
    }
}

/// [`crate::Engine::providers`]. Local servers (Kokoro-FastAPI, OpenAI-compatible servers on this machine or a
/// private network) are probed once per distinct URL, for at most [`SERVER_PROBE_TIMEOUT`] each and all at
/// once; a provider whose server does not answer is not available.
pub(crate) fn report(engine: &Engine) -> ProvidersReport {
    let client = client(engine);
    let cwd = absolute(std::path::Path::new("."));
    let instances: Vec<(ProviderType, ProviderInstance)> = providers::all()
        .iter()
        .map(|&t| (t, default_instance(engine, t)))
        .collect();
    let statuses: Vec<CredentialStatus> = instances
        .iter()
        .map(|(t, instance)| {
            if *t == ProviderType::Command && !engine.options.allow_command {
                CredentialStatus::Unavailable {
                    reason: format!("not allowed here: {ALLOW_COMMAND_HINT}"),
                }
            } else {
                client.credential_status(instance, &cwd)
            }
        })
        .collect();
    // Only usable providers with a local endpoint are worth a probe.
    let mut urls: Vec<String> = instances
        .iter()
        .zip(&statuses)
        .filter(|(_, status)| status.is_usable())
        .filter_map(|((_, instance), _)| client.endpoint_url(instance))
        .filter(|url| soniccraft_tts::is_local_url(url))
        .collect();
    urls.sort();
    urls.dedup();
    let reachable = probe_all(&urls, SERVER_PROBE_TIMEOUT);
    let providers = instances
        .iter()
        .zip(statuses)
        .map(|((provider_type, instance), status)| {
            let provider_type = *provider_type;
            let meta = providers::meta(provider_type);
            let probed = client
                .endpoint_url(instance)
                .and_then(|url| reachable.get(&url).map(|&answered| (url, answered)))
                .filter(|_| status.is_usable());
            let (available, described) = match probed {
                Some((url, false)) => (
                    false,
                    format!(
                        "no {} answered at {}",
                        server_name(provider_type),
                        soniccraft_tts::redact_url(&url)
                    ),
                ),
                Some((url, true)) if provider_type == ProviderType::Kokoro => (
                    true,
                    format!(
                        "ready (a Kokoro-FastAPI server answers at {}; no key needed)",
                        soniccraft_tts::redact_url(&url)
                    ),
                ),
                _ => (status.is_usable(), describe(provider_type, &status)),
            };
            ProviderStatus {
                name: provider_type.as_str().to_string(),
                aliases: aliases(provider_type),
                display_name: meta.display_name.to_string(),
                local: meta.local,
                word_timestamps: meta.word_timestamps,
                available,
                status: described,
                api_key_env: meta.default_api_key_env.map(str::to_string),
                default_voice: meta.default_voice.map(str::to_string),
                default_model: meta.default_model.map(str::to_string),
                notes: meta.notes.to_string(),
            }
        })
        .collect();
    ProvidersReport { providers }
}

/// Probe every URL with [`soniccraft_tts::probe_reachable`], each on a thread of its own with its own
/// small runtime, so this works from synchronous code inside or outside a Tokio runtime. A probe that
/// does not finish in time (a hung name lookup) counts as unreachable.
fn probe_all(urls: &[String], timeout: Duration) -> HashMap<String, bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    for url in urls {
        let tx = tx.clone();
        let url = url.clone();
        let spawned = std::thread::Builder::new()
            .name("soniccraft-probe".into())
            .spawn(move || {
                let answered = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .is_ok_and(|rt| rt.block_on(soniccraft_tts::probe_reachable(&url, timeout)));
                let _ = tx.send((url, answered));
            });
        if spawned.is_err() {
            continue;
        }
    }
    drop(tx);
    let deadline = std::time::Instant::now() + timeout + Duration::from_millis(200);
    let mut out: HashMap<String, bool> = urls.iter().map(|u| (u.clone(), false)).collect();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok((url, answered)) => {
                out.insert(url, answered);
            }
            Err(_) => break,
        }
    }
    out
}

/// The accepted spellings of `provider_type` other than its canonical name.
fn aliases(provider_type: ProviderType) -> Vec<String> {
    ProviderType::SPELLINGS
        .iter()
        .filter(|s| **s != provider_type.as_str() && ProviderType::parse(s) == Some(provider_type))
        .map(|s| s.to_string())
        .collect()
}

/// One-line status for humans and agents.
fn describe(provider_type: ProviderType, status: &CredentialStatus) -> String {
    match status {
        CredentialStatus::Ready => match providers::meta(provider_type).default_api_key_env {
            Some(var) => format!("ready ({var} is set)"),
            None => "ready (API key set)".to_string(),
        },
        CredentialStatus::NotRequired => match provider_type {
            ProviderType::Command => {
                "ready (configure `command` on a provider instance)".to_string()
            }
            ProviderType::Kokoro => {
                "ready if a Kokoro-FastAPI server is running (no key needed)".to_string()
            }
            _ => "ready (no credentials needed)".to_string(),
        },
        CredentialStatus::Missing { env_var } => format!("missing credentials: set {env_var}"),
        CredentialStatus::Unavailable { reason } => format!("unavailable: {reason}"),
    }
}

/// [`crate::Engine::voices`]. The provider name is a command argument, not the DSL: case and separator
/// variants (`ElevenLabs`, `macos-say`) are accepted.
pub(crate) async fn voices(engine: &Engine, provider: &str) -> Result<VoicesReport, EngineError> {
    let lenient = providers::builtin_type(provider)
        .or_else(|| ProviderType::spelling_of(provider).and_then(providers::builtin_type));
    let Some(provider_type) = lenient else {
        // Suggest the canonical name, also when the closest spelling is an alias.
        let suggestion = diagnostics::did_you_mean(provider, ProviderType::NAMES.iter().copied())
            .or_else(|| {
                diagnostics::did_you_mean(provider, providers::builtin_names().iter().copied())
                    .and_then(providers::builtin_type)
                    .map(ProviderType::as_str)
            })
            .map(|s| format!("did you mean `{s}`? "))
            .unwrap_or_default();
        return Err(EngineError::InvalidArgument(format!(
            "unknown provider `{provider}`; {suggestion}built-in providers: {}",
            ProviderType::NAMES.join(", ")
        )));
    };
    let instance = default_instance(engine, provider_type);
    // Piper looks for models next to where the command runs, like synthesis looks next to the timeline.
    let cwd = absolute(std::path::Path::new("."));
    let voices = client(engine)
        .list_voices_in_current_dir(&instance, &cwd)
        .await
        .map_err(|e| {
            let hint = e
                .hint
                .as_deref()
                .map(|h| format!(" ({h})"))
                .unwrap_or_default();
            EngineError::Provider(format!("{e}{hint}"))
        })?;
    Ok(VoicesReport {
        provider: provider_type.as_str().to_string(),
        voices: voices
            .into_iter()
            .map(|v| VoiceInfo {
                id: v.id,
                name: v.name,
                language: v.language,
                gender: v.gender,
                description: v.description,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EngineOptions, EnvSource};

    fn engine() -> Engine {
        Engine::new(EngineOptions::default()).with_env(EnvSource::fixed([("UNRELATED", "1")]))
    }

    #[test]
    fn report_lists_every_builtin() {
        let report = engine().providers();
        let names: Vec<&str> = report.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ProviderType::NAMES);
        let eleven = &report.providers[0];
        assert_eq!(eleven.aliases, ["eleven_labs", "11labs"]);
        assert!(!eleven.available);
        assert!(
            eleven.status.contains("ELEVENLABS_API_KEY"),
            "{}",
            eleven.status
        );
        let mock = report.providers.iter().find(|p| p.name == "mock").unwrap();
        assert!(mock.available && mock.local && mock.word_timestamps);
        assert_eq!(mock.default_voice.as_deref(), Some("mock"));
        let ready = Engine::new(EngineOptions::default())
            .with_env(EnvSource::fixed([("ELEVENLABS_API_KEY", "k")]))
            .providers();
        assert!(ready.providers[0].available);
        assert_eq!(
            ready.providers[0].status,
            "ready (ELEVENLABS_API_KEY is set)"
        );
    }

    #[tokio::test]
    async fn voices_of_builtins() {
        let mock = engine().voices("mock").await.unwrap();
        assert_eq!(mock.provider, "mock");
        assert_eq!(mock.voices[0].id, "mock");
        let err = engine().voices("elevenlab").await.unwrap_err();
        let message = err.to_string();
        assert!(message.contains("did you mean `elevenlabs`?"), "{message}");
        assert!(matches!(err, EngineError::InvalidArgument(_)));
        assert!(engine().voices("polly").await.is_err());
    }
}
