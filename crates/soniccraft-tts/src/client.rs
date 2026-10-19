//! [`TtsClient`]: cache-aware, concurrency-limited synthesis plus credential checks and voice listing.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use soniccraft_core::ir::{ProviderInstance, ProviderType, VoiceIr};
use soniccraft_dsp::AudioBuffer;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cache::{self, CacheStore, EntryMeta};
use crate::env::EnvSource;
use crate::error::{TtsError, TtsErrorKind, error_codes};
use crate::hosts;
use crate::http::{HttpClients, RetryPolicy};
use crate::keys::{self, KeyLookup};
use crate::process;
use crate::providers::piper::{self, PiperModelNotFound};
use crate::providers::{self, Ctx, ProviderState, command, macos_say, openai};
use crate::settings::{self, Resolved};
use crate::types::{
    CachedMeta, CredentialStatus, SynthesisProgress, SynthesisRequest, SynthesisResult, Voice,
    WordTiming,
};

/// Configuration of a [`TtsClient`].
#[derive(Debug, Clone)]
pub struct TtsOptions {
    /// Cache directory (DESIGN.md §5.1). `None` disables caching. The engine resolves CLI `--cache-dir` >
    /// `SONICCRAFT_CACHE_DIR` > `<timeline dir>/.soniccraft/cache` before constructing the client.
    pub cache_dir: Option<PathBuf>,
    /// Serve cached audio. `false` (`--no-cache`) re-synthesizes every cue; results are still written when
    /// `write_cache` is set.
    pub read_cache: bool,
    /// Store synthesized audio in the cache.
    pub write_cache: bool,
    /// Global limit on concurrent syntheses (default 4; 0 is treated as 1). Each instance is additionally
    /// limited to its `max_concurrency`.
    pub jobs: usize,
    /// First retry delay; later retries double it (1 s → 2 s → 4 s by default). It also scales `Retry-After`
    /// (waits are `retry_after × retry_base_delay / 1 s`), so tests can shrink every wait.
    pub retry_base_delay: Duration,
    /// Retries after the first attempt (default 3). All attempts of a cue, and the waits between them, fit in
    /// the instance's `timeout_ms`.
    pub max_retries: u32,
    /// Where API keys and other variables are read from.
    pub env: EnvSource,
    /// Extra environment variables never passed to child processes (piper, macos_say, command), e.g. every
    /// `api_key_env` the timeline's providers name. `ELEVENLABS_API_KEY`, `OPENAI_API_KEY`,
    /// `CARTESIA_API_KEY`, `KOKORO_API_KEY`, `SONICCRAFT_ALLOWED_HOSTS` and the instance's own `api_key_env`
    /// are always removed.
    pub child_env_remove: Vec<String>,
}

impl Default for TtsOptions {
    fn default() -> Self {
        TtsOptions {
            cache_dir: None,
            read_cache: true,
            write_cache: true,
            jobs: 4,
            retry_base_delay: Duration::from_secs(1),
            max_retries: 3,
            env: EnvSource::Process,
            child_env_remove: Vec::new(),
        }
    }
}

/// The TTS client. Cheap to clone (shared state); safe to use from many tasks at once.
///
/// All async methods must run inside a Tokio runtime (decoding and cache I/O use `spawn_blocking`).
#[derive(Debug, Clone)]
pub struct TtsClient {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    options: TtsOptions,
    cache: Option<CacheStore>,
    global: Arc<Semaphore>,
    /// Per-instance limiters keyed by (instance name, max_concurrency).
    instances: Mutex<BTreeMap<(String, usize), Arc<Semaphore>>>,
    http: HttpClients,
    state: ProviderState,
}

impl TtsClient {
    /// A client with `options`.
    pub fn new(options: TtsOptions) -> Self {
        let jobs = options.jobs.clamp(1, Semaphore::MAX_PERMITS);
        let cache = options.cache_dir.clone().map(CacheStore::new);
        TtsClient {
            inner: Arc::new(Inner {
                options,
                cache,
                global: Arc::new(Semaphore::new(jobs)),
                instances: Mutex::new(BTreeMap::new()),
                http: HttpClients::default(),
                state: ProviderState::default(),
            }),
        }
    }

    /// The options the client was built with.
    pub fn options(&self) -> &TtsOptions {
        &self.inner.options
    }

    /// The cache key of `request` (see [`crate::cache_key`]), with programs and piper models looked up in the
    /// client's environment.
    pub fn cache_key(&self, request: &SynthesisRequest) -> Result<String, TtsError> {
        let resolved = Resolved::new(&request.provider, &request.voice);
        cache::key_for(request, &resolved, &self.inner.options.env)
    }

    /// What the cache holds for `request`, read cheaply (metadata and file sizes, no decoding). `None` when
    /// caching is disabled, `read_cache` is false, or there is no valid entry. Used by dry runs for exact
    /// durations (`duration_source: cache`).
    pub fn cached(&self, request: &SynthesisRequest) -> Option<CachedMeta> {
        if !self.inner.options.read_cache {
            return None;
        }
        let store = self.inner.cache.as_ref()?;
        let key = self.cache_key(request).ok()?;
        match store.peek(&key) {
            Ok(meta) => meta.map(|m| m.to_cached_meta()),
            Err(reason) => {
                tracing::debug!(cue = %request.cue_id, "ignoring TTS cache entry: {reason}");
                None
            }
        }
    }

    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            http: &self.inner.http,
            env: &self.inner.options.env,
            retry: RetryPolicy {
                base_delay: self.inner.options.retry_base_delay,
                max_retries: self.inner.options.max_retries,
            },
            state: &self.inner.state,
            child_env_remove: &self.inner.options.child_env_remove,
        }
    }

    /// Synthesize one cue, or serve it from the cache. Respects the global `jobs` limit and the instance's
    /// `max_concurrency`; transient HTTP failures are retried (DESIGN.md §5.2).
    pub async fn synthesize(
        &self,
        request: &SynthesisRequest,
    ) -> Result<SynthesisResult, TtsError> {
        let resolved = Resolved::new(&request.provider, &request.voice);
        let key = cache::key_for(request, &resolved, &self.inner.options.env)?;
        self.synthesize_keyed(request, &resolved, key).await
    }

    /// Synthesize many cues concurrently. Results are in input order; requests with identical cache keys are
    /// synthesized (or read from the cache) once and copied, with [`SynthesisResult::duplicate_of`] pointing at
    /// the first one. `progress` is called once per request as it completes.
    pub async fn synthesize_all<F>(
        &self,
        requests: Vec<SynthesisRequest>,
        progress: F,
    ) -> Vec<Result<SynthesisResult, TtsError>>
    where
        F: Fn(&SynthesisProgress) + Send + Sync,
    {
        let total = requests.len();
        let resolved: Vec<Resolved> = requests
            .iter()
            .map(|r| Resolved::new(&r.provider, &r.voice))
            .collect();
        let keys: Vec<Result<String, TtsError>> = requests
            .iter()
            .zip(&resolved)
            .map(|(r, res)| cache::key_for(r, res, &self.inner.options.env))
            .collect();

        let mut followers: Vec<Vec<usize>> = vec![Vec::new(); total];
        let mut is_follower = vec![false; total];
        let mut first_index: HashMap<&str, usize> = HashMap::new();
        for (i, key) in keys.iter().enumerate() {
            if let Ok(key) = key {
                match first_index.get(key.as_str()) {
                    Some(&leader) => {
                        followers[leader].push(i);
                        is_follower[i] = true;
                    }
                    None => {
                        first_index.insert(key.as_str(), i);
                    }
                }
            }
        }

        let mut results: Vec<Option<Result<SynthesisResult, TtsError>>> =
            (0..total).map(|_| None).collect();
        let mut done = 0usize;
        let mut report = |index: usize, result: &Result<SynthesisResult, TtsError>| {
            done += 1;
            progress(&SynthesisProgress {
                index,
                cue_id: requests[index].cue_id.clone(),
                cache_hit: result.as_ref().is_ok_and(|r| r.cache_hit),
                success: result.is_ok(),
                done,
                total,
            });
        };

        let mut pending = FuturesUnordered::new();
        for (i, key) in keys.iter().enumerate() {
            match key {
                Err(error) => {
                    let result = Err(error.clone());
                    report(i, &result);
                    results[i] = Some(result);
                }
                Ok(_) if is_follower[i] => {}
                Ok(key) => {
                    let (request, resolved, key) = (&requests[i], &resolved[i], key.clone());
                    pending.push(async move {
                        (i, self.synthesize_keyed(request, resolved, key).await)
                    });
                }
            }
        }
        while let Some((leader, result)) = pending.next().await {
            report(leader, &result);
            for &follower in &followers[leader] {
                let copy = match &result {
                    Ok(r) => Ok(SynthesisResult {
                        duplicate_of: Some(leader),
                        ..r.clone()
                    }),
                    Err(e) => Err(e.clone()),
                };
                report(follower, &copy);
                results[follower] = Some(copy);
            }
            results[leader] = Some(result);
        }
        drop(pending);
        results
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                r.unwrap_or_else(|| {
                    Err(TtsError::new(
                        TtsErrorKind::Provider,
                        &requests[i].provider,
                        "internal error: request was not scheduled",
                    ))
                })
            })
            .collect()
    }

    async fn synthesize_keyed(
        &self,
        request: &SynthesisRequest,
        resolved: &Resolved,
        key: String,
    ) -> Result<SynthesisResult, TtsError> {
        let mut notes = providers::static_notes(&request.provider, resolved);
        let first_lookup = self.load_cached(&key).await;
        if let Lookup::Hit(hit) = first_lookup {
            return Ok(cached_result(*hit, key, notes));
        }
        let endpoint = providers::endpoint_url(&request.provider, &self.inner.options.env);
        self.check_reachable(&request.provider, endpoint.as_deref())?;
        let permits = self.acquire(&request.provider).await?;
        // Another task may have stored this entry while we waited for a slot (a known-bad entry is not
        // re-read: it will be overwritten).
        if matches!(first_lookup, Lookup::Miss)
            && let Lookup::Hit(hit) = self.load_cached(&key).await
        {
            return Ok(cached_result(*hit, key, notes));
        }
        // An earlier cue of this instance may have found its server unreachable while we waited.
        self.check_reachable(&request.provider, endpoint.as_deref())?;
        tracing::debug!(cue = %request.cue_id, provider = %request.provider.name, key = %key, "synthesizing");
        let raw = match providers::synthesize(&self.ctx(), request, resolved).await {
            Ok(raw) => raw,
            Err(error) => {
                if error.has_code(error_codes::CONNECT_FAILED)
                    && let Some(url) = &endpoint
                {
                    self.inner
                        .state
                        .unreachable
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(
                            (request.provider.name.clone(), url.clone()),
                            error.hint.clone(),
                        );
                }
                return Err(error);
            }
        };
        drop(permits);
        for note in raw.notes {
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        let (audio, words) = finish_audio(&request.provider, raw.audio, raw.words)?;
        check_plausible_duration(request, &audio)?;
        let cache_write_error = match self
            .store_cached(&key, request, resolved, &audio, &words, raw.output_format)
            .await
        {
            Ok(()) => None,
            Err(reason) => {
                tracing::warn!(cue = %request.cue_id, "could not write TTS cache entry: {reason}");
                Some(reason)
            }
        };
        Ok(SynthesisResult {
            audio,
            words,
            cache_hit: false,
            cache_key: key,
            duplicate_of: None,
            notes,
            cache_write_error,
        })
    }

    /// Fail at once when an earlier cue of `provider` could not connect to `endpoint`
    /// ([`error_codes::NOT_ATTEMPTED`]): one fast failure per cue instead of a connection attempt each.
    fn check_reachable(
        &self,
        provider: &ProviderInstance,
        endpoint: Option<&str>,
    ) -> Result<(), TtsError> {
        let Some(url) = endpoint else {
            return Ok(());
        };
        let unreachable = self
            .inner
            .state
            .unreachable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(hint) = unreachable.get(&(provider.name.clone(), url.to_owned())) else {
            return Ok(());
        };
        let mut error = TtsError::new(
            TtsErrorKind::Network,
            provider,
            format!(
                "not attempted: {} unreachable at {}",
                provider.name,
                hosts::redact_url(url)
            ),
        )
        .with_code(error_codes::NOT_ATTEMPTED);
        error.attempts = 0;
        if let Some(hint) = hint {
            error = error.with_hint(hint.clone());
        }
        Err(error)
    }

    async fn acquire(
        &self,
        provider: &ProviderInstance,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), TtsError> {
        let closed = |_| {
            TtsError::new(
                TtsErrorKind::Provider,
                provider,
                "the TTS concurrency limiter was closed",
            )
        };
        // Instance slot first, so waiting on a busy instance never holds a global slot other providers could use.
        let instance = self
            .instance_semaphore(provider)
            .acquire_owned()
            .await
            .map_err(closed)?;
        let global = self
            .inner
            .global
            .clone()
            .acquire_owned()
            .await
            .map_err(closed)?;
        Ok((instance, global))
    }

    fn instance_semaphore(&self, provider: &ProviderInstance) -> Arc<Semaphore> {
        let permits = provider.max_concurrency.clamp(1, Semaphore::MAX_PERMITS);
        let mut map = self
            .inner
            .instances
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        map.entry((provider.name.clone(), permits))
            .or_insert_with(|| Arc::new(Semaphore::new(permits)))
            .clone()
    }

    async fn load_cached(&self, key: &str) -> Lookup {
        if !self.inner.options.read_cache {
            return Lookup::Disabled;
        }
        let Some(store) = self.inner.cache.clone() else {
            return Lookup::Disabled;
        };
        let key = key.to_owned();
        match tokio::task::spawn_blocking(move || store.load(&key)).await {
            Ok(Ok(Some(entry))) => Lookup::Hit(Box::new(entry)),
            Ok(Ok(None)) => Lookup::Miss,
            Ok(Err(reason)) => {
                tracing::warn!(
                    "ignoring unusable TTS cache entry (it will be re-synthesized): {reason}"
                );
                Lookup::Unusable
            }
            Err(join) => {
                tracing::warn!("reading the TTS cache failed: {join}");
                Lookup::Unusable
            }
        }
    }

    async fn store_cached(
        &self,
        key: &str,
        request: &SynthesisRequest,
        resolved: &Resolved,
        audio: &AudioBuffer,
        words: &Option<Vec<WordTiming>>,
        output_format: Option<String>,
    ) -> Result<(), String> {
        if !self.inner.options.write_cache {
            return Ok(());
        }
        let Some(store) = self.inner.cache.clone() else {
            return Ok(());
        };
        let meta = EntryMeta {
            cache_version: cache::CACHE_VERSION,
            key: key.to_owned(),
            provider_type: request.provider.provider_type.as_str().to_owned(),
            model: resolved.model.clone(),
            voice_id: resolved.voice_id.clone(),
            language: resolved.language.clone(),
            text: request.text.clone(),
            output_format,
            sample_rate: audio.sample_rate,
            channels: u16::try_from(audio.num_channels()).unwrap_or(u16::MAX),
            frames: audio.frames() as u64,
            duration_secs: audio.duration_secs(),
            words: words.clone(),
        };
        let (key, audio) = (key.to_owned(), audio.clone());
        tokio::task::spawn_blocking(move || store.store(&key, &audio, &meta))
            .await
            .map_err(|e| format!("cache writer failed: {e}"))?
    }

    /// Whether `provider` can be used here, checked offline: API key variables present (an unexpanded
    /// placeholder such as `${ELEVENLABS_API_KEY}` counts as missing: see [`TtsClient::credential_hint`]),
    /// binaries found, platform supported. `base_dir` resolves relative program and `voices_dir` paths. A key
    /// that the key-sending host policy would refuse to send ([`crate::key_host_allowed`]) makes the provider
    /// [`CredentialStatus::Unavailable`] as configured; the reason names the variable, the host and the
    /// `SONICCRAFT_ALLOWED_HOSTS` fix.
    pub fn credential_status(
        &self,
        provider: &ProviderInstance,
        base_dir: &Path,
    ) -> CredentialStatus {
        let env = &self.inner.options.env;
        let unavailable = |reason: String| CredentialStatus::Unavailable { reason };
        let status = match provider.provider_type {
            ProviderType::Elevenlabs | ProviderType::Cartesia => key_status(env, provider, true),
            ProviderType::Openai => {
                let required =
                    openai::is_official(&openai::base_url(provider)) || explicit_key_env(provider);
                key_status(env, provider, required)
            }
            ProviderType::Kokoro => key_status(env, provider, explicit_key_env(provider)),
            ProviderType::Piper => match provider.command.as_deref().filter(|c| !c.is_empty()) {
                Some(argv) => command::program_status(Some(argv), env, base_dir)
                    .map_or_else(unavailable, |()| CredentialStatus::NotRequired),
                None if process::find_program("piper", env, base_dir).is_some() => {
                    CredentialStatus::NotRequired
                }
                None => unavailable(
                    "`piper` not found on PATH (install it with `pipx install piper-tts`, or set `command`)"
                        .to_owned(),
                ),
            },
            ProviderType::MacosSay => macos_say::availability()
                .map_or_else(unavailable, |()| CredentialStatus::NotRequired),
            ProviderType::Command => {
                command::program_status(provider.command.as_deref(), env, base_dir)
                    .map_or_else(unavailable, |()| CredentialStatus::NotRequired)
            }
            ProviderType::Mock => CredentialStatus::NotRequired,
        };
        if !status.is_usable() {
            return status;
        }
        match keys::key_host_allowed(provider, env) {
            Ok(()) => status,
            Err(refused) => unavailable(match refused.hint {
                Some(hint) => format!("{}; {hint}", refused.message),
                None => refused.message,
            }),
        }
    }

    /// Why `provider`'s key counts as missing beyond "the variable is not set", for the hint of a
    /// `missing_credentials` diagnostic: today, the variable holds an unexpanded placeholder such as
    /// `${ELEVENLABS_API_KEY}` (an MCP client passed its config template through). `None` otherwise.
    pub fn credential_hint(&self, provider: &ProviderInstance) -> Option<String> {
        match keys::lookup(&self.inner.options.env, provider) {
            KeyLookup::Placeholder { var } => Some(keys::placeholder_hint(&var)),
            _ => None,
        }
    }

    /// [`crate::key_host_allowed`] with the client's environment: whether `provider` would send its API key
    /// to a host the key-sending policy allows. Synthesis, voice listing and credential checks refuse the
    /// request with this error before anything is sent, and [`TtsClient::credential_status`] reports such an
    /// instance as unavailable.
    pub fn key_host_allowed(&self, provider: &ProviderInstance) -> Result<(), TtsError> {
        keys::key_host_allowed(provider, &self.inner.options.env)
    }

    /// [`crate::endpoint_url`] with the client's environment (`KOKORO_BASE_URL`).
    pub fn endpoint_url(&self, provider: &ProviderInstance) -> Option<String> {
        providers::endpoint_url(provider, &self.inner.options.env)
    }

    /// [`crate::resolve_piper_model`] with the client's environment (`PIPER_VOICES_DIR`, `HOME`).
    pub fn resolve_piper_model(
        &self,
        provider: &ProviderInstance,
        voice_id: &str,
        base_dir: &Path,
    ) -> Result<PathBuf, PiperModelNotFound> {
        piper::resolve_piper_model(provider, voice_id, &self.inner.options.env, base_dir)
    }

    /// Verify credentials with a cheap authenticated request (network): ElevenLabs `GET /v1/user`, OpenAI
    /// `GET {base}/models`, Cartesia `GET /voices?limit=1`, Kokoro `GET /health`. Local providers succeed
    /// without a request. Keys go only to hosts the key-sending policy allows.
    pub async fn check_credentials_online(
        &self,
        provider: &ProviderInstance,
    ) -> Result<(), TtsError> {
        providers::check_credentials(&self.ctx(), provider).await
    }

    /// Voices offered by `provider` (may call its API; local providers scan their voice lists). Piper's
    /// relative `voices_dir` and the timeline directory are only searched by [`TtsClient::list_voices_in`].
    pub async fn list_voices(&self, provider: &ProviderInstance) -> Result<Vec<Voice>, TtsError> {
        providers::list_voices(&self.ctx(), provider, None, piper::TIMELINE_DIR_LABEL).await
    }

    /// [`TtsClient::list_voices`] for a timeline in `base_dir`: piper's relative `voices_dir` (and
    /// `PIPER_VOICES_DIR`) resolve against it, and it is searched for models itself, exactly as synthesis
    /// searches (never the process's working directory).
    pub async fn list_voices_in(
        &self,
        provider: &ProviderInstance,
        base_dir: &Path,
    ) -> Result<Vec<Voice>, TtsError> {
        providers::list_voices(
            &self.ctx(),
            provider,
            Some(base_dir),
            piper::TIMELINE_DIR_LABEL,
        )
        .await
    }

    /// [`TtsClient::list_voices_in`] for a command run in `dir` (`soniccraft voices`): the same search, but
    /// messages and listings call `dir` the current directory instead of the timeline directory.
    pub async fn list_voices_in_current_dir(
        &self,
        provider: &ProviderInstance,
        dir: &Path,
    ) -> Result<Vec<Voice>, TtsError> {
        providers::list_voices(&self.ctx(), provider, Some(dir), "current directory").await
    }
}

/// True when the instance names a key variable other than its type's default (so the key is required even for
/// providers where it is normally optional).
fn explicit_key_env(provider: &ProviderInstance) -> bool {
    settings::non_empty(provider.api_key_env.as_deref()).is_some_and(|var| {
        Some(var) != crate::defaults::default_api_key_env(provider.provider_type)
    })
}

fn key_status(env: &EnvSource, provider: &ProviderInstance, required: bool) -> CredentialStatus {
    match keys::lookup(env, provider) {
        KeyLookup::Key { .. } => CredentialStatus::Ready,
        KeyLookup::Unset { var } | KeyLookup::Placeholder { var } if required => {
            CredentialStatus::Missing { env_var: var }
        }
        KeyLookup::NoVariable if required => CredentialStatus::Missing {
            env_var: "API key variable".to_owned(),
        },
        _ => CredentialStatus::NotRequired,
    }
}

/// Exactly what [`TtsClient::synthesize`] returns for a `mock` cue that is not cached: the same audio
/// (24 kHz mono), word timings, notes and cache key, computed in-process without I/O. Dry runs use it for
/// exact durations (trim it like a render does). `None` for any other provider type.
pub fn render_mock(
    provider: &ProviderInstance,
    voice: &VoiceIr,
    text: &str,
) -> Option<SynthesisResult> {
    if provider.provider_type != ProviderType::Mock {
        return None;
    }
    let request = SynthesisRequest {
        cue_id: String::new(),
        provider: provider.clone(),
        voice: voice.clone(),
        text: text.to_owned(),
        base_dir: PathBuf::new(),
    };
    let resolved = Resolved::new(provider, voice);
    // Mock keys depend on nothing outside the request.
    let cache_key = cache::key_for(&request, &resolved, &EnvSource::Process).ok()?;
    let raw = providers::mock_raw(text, &resolved);
    let mut notes = providers::static_notes(provider, &resolved);
    for note in raw.notes {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    let (audio, words) = finish_audio(provider, raw.audio, raw.words).ok()?;
    Some(SynthesisResult {
        audio,
        words,
        cache_hit: false,
        cache_key,
        duplicate_of: None,
        notes,
        cache_write_error: None,
    })
}

/// Outcome of a cache lookup.
enum Lookup {
    Hit(Box<(AudioBuffer, EntryMeta)>),
    Miss,
    /// An entry exists but is corrupt or stale.
    Unusable,
    /// No cache directory, or `read_cache` is off.
    Disabled,
}

fn cached_result(
    (audio, meta): (AudioBuffer, EntryMeta),
    key: String,
    notes: Vec<String>,
) -> SynthesisResult {
    SynthesisResult {
        audio,
        words: meta.words,
        cache_hit: true,
        cache_key: key,
        duplicate_of: None,
        notes,
        cache_write_error: None,
    }
}

/// Validate provider audio (non-empty, finite samples) and clamp word timings to it.
fn finish_audio(
    origin: &ProviderInstance,
    mut audio: AudioBuffer,
    words: Option<Vec<WordTiming>>,
) -> Result<(AudioBuffer, Option<Vec<WordTiming>>), TtsError> {
    if audio.sample_rate == 0 || audio.num_channels() == 0 || audio.is_empty() {
        return Err(TtsError::new(
            TtsErrorKind::Provider,
            origin,
            "the provider returned no audio",
        )
        .with_hint("check the text and voice; retry if the provider is unstable"));
    }
    for channel in &mut audio.channels {
        for sample in channel.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }
    }
    let words = words.and_then(|w| clamp_words(w, audio.duration_secs()));
    Ok((audio, words))
}

/// Audio up to this long is accepted for any cue, whatever its estimate.
const PLAUSIBLE_FLOOR_SECS: f64 = 600.0;
/// Audio may run this many times the cue's dry-run estimate (beyond the floor).
const PLAUSIBLE_FACTOR: f64 = 20.0;

/// Refuse audio far longer than the cue's text can produce ([`error_codes::IMPLAUSIBLE_DURATION`]): a wrong
/// `base_url` or `output_format` can turn megabytes of something else into noise that would otherwise be
/// mixed, cached and rendered. The limit (ten minutes plus twenty times the estimate) leaves room for slow
/// settings and long pauses. `command` programs may render anything, and the mock voice is exact.
fn check_plausible_duration(
    request: &SynthesisRequest,
    audio: &AudioBuffer,
) -> Result<(), TtsError> {
    if matches!(
        request.provider.provider_type,
        ProviderType::Command | ProviderType::Mock
    ) {
        return Ok(());
    }
    let estimate = crate::estimate_duration_secs(&request.provider, &request.voice, &request.text);
    let actual = audio.duration_secs();
    if actual <= PLAUSIBLE_FLOOR_SECS + PLAUSIBLE_FACTOR * estimate {
        return Ok(());
    }
    Err(TtsError::new(
        TtsErrorKind::Provider,
        &request.provider,
        format!(
            "the provider returned {} of audio for a cue of {} characters (about {} expected); refusing it",
            describe_secs(actual),
            request.text.chars().count(),
            describe_secs(estimate)
        ),
    )
    .with_code(error_codes::IMPLAUSIBLE_DURATION)
    .with_hint(
        "check `base_url` and `output_format` on the provider: a wrong endpoint or audio format decodes as noise",
    ))
}

/// `1.5 s` or `23 min 18 s`.
fn describe_secs(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let whole = secs.round() as u64;
        format!("{} min {} s", whole / 60, whole % 60)
    }
}

/// Drop empty or non-finite words, clamp to `[0, duration]`, and keep starts non-decreasing and `end ≥ start`.
pub(crate) fn clamp_words(words: Vec<WordTiming>, duration: f64) -> Option<Vec<WordTiming>> {
    let mut out: Vec<WordTiming> = Vec::with_capacity(words.len());
    let mut previous_start = 0.0f64;
    for word in words {
        let text = word.text.trim();
        if text.is_empty() || !word.start_secs.is_finite() || !word.end_secs.is_finite() {
            continue;
        }
        let start = word.start_secs.clamp(0.0, duration).max(previous_start);
        let end = word.end_secs.clamp(0.0, duration).max(start);
        previous_start = start;
        out.push(WordTiming {
            text: text.to_owned(),
            start_secs: start,
            end_secs: end,
        });
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, voice};
    use serde_json::json;

    fn mock_request(text: &str, voice_id: &str) -> SynthesisRequest {
        SynthesisRequest {
            cue_id: format!("cue-{text}"),
            provider: instance(ProviderType::Mock),
            voice: voice(ProviderType::Mock, Some(voice_id), None, None, json!({})),
            text: text.to_owned(),
            base_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn client_is_shareable_and_futures_are_send() {
        fn assert_send<T: Send>(_: &T) {}
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TtsClient>();
        let client = TtsClient::new(TtsOptions::default());
        let request = mock_request("x", "y");
        let provider = instance(ProviderType::Mock);
        assert_send(&client.synthesize(&request));
        assert_send(&client.synthesize_all(Vec::new(), |_| {}));
        assert_send(&client.list_voices(&provider));
        assert_send(&client.check_credentials_online(&provider));
    }

    #[tokio::test]
    async fn synthesize_all_handles_empty_input() {
        let client = TtsClient::new(TtsOptions::default());
        assert!(client.synthesize_all(Vec::new(), |_| {}).await.is_empty());
    }

    #[test]
    fn word_clamping() {
        let w = |t: &str, s: f64, e: f64| WordTiming {
            text: t.into(),
            start_secs: s,
            end_secs: e,
        };
        let out = clamp_words(
            vec![
                w("a", -1.0, 0.5),
                w(" ", 0.5, 0.6),
                w("b", 0.4, 0.3),
                w("c", 0.9, 5.0),
                w("d", f64::NAN, 1.0),
            ],
            1.0,
        )
        .unwrap();
        assert_eq!(
            out,
            vec![w("a", 0.0, 0.5), w("b", 0.4, 0.4), w("c", 0.9, 1.0)]
        );
        assert_eq!(clamp_words(vec![w("", 0.0, 1.0)], 1.0), None);
    }

    #[tokio::test]
    async fn mock_synthesis_without_cache() {
        let client = TtsClient::new(TtsOptions::default());
        let request = mock_request("Hello there, world.", "narrator");
        assert_eq!(client.cached(&request), None);
        let result = client.synthesize(&request).await.unwrap();
        assert!(!result.cache_hit);
        assert_eq!(result.cache_key, crate::cache_key(&request).unwrap());
        assert_eq!(result.audio.sample_rate, 24_000);
        let words = result.words.unwrap();
        assert_eq!(words.len(), 3);
        assert_eq!(
            result.audio.duration_secs(),
            crate::estimate_duration_secs(&request.provider, &request.voice, &request.text)
        );
    }

    #[tokio::test]
    async fn synthesize_all_preserves_order_and_dedupes() {
        let client = TtsClient::new(TtsOptions {
            jobs: 2,
            ..TtsOptions::default()
        });
        let requests = vec![
            mock_request("one", "a"),
            mock_request("two", "a"),
            mock_request("one", "a"),
            mock_request("three", "b"),
            mock_request("two", "a"),
        ];
        let events = Mutex::new(Vec::new());
        let results = client
            .synthesize_all(requests.clone(), |p| events.lock().unwrap().push(p.clone()))
            .await;
        assert_eq!(results.len(), 5);
        let results: Vec<SynthesisResult> = results.into_iter().map(Result::unwrap).collect();
        assert_eq!(results[0].duplicate_of, None);
        assert_eq!(results[2].duplicate_of, Some(0));
        assert_eq!(results[4].duplicate_of, Some(1));
        assert_eq!(results[0].audio, results[2].audio);
        assert_ne!(results[0].audio, results[1].audio);
        for (r, req) in results.iter().zip(&requests) {
            assert_eq!(r.cache_key, crate::cache_key(req).unwrap());
        }
        let events = events.into_inner().unwrap();
        assert_eq!(events.len(), 5);
        assert_eq!(
            events.iter().map(|e| e.done).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5]
        );
        let mut indices: Vec<usize> = events.iter().map(|e| e.index).collect();
        indices.sort_unstable();
        assert_eq!(indices, [0, 1, 2, 3, 4]);
        assert!(events.iter().all(|e| e.success && e.total == 5));
    }

    #[test]
    fn credential_statuses() {
        let env = EnvSource::fixed([("ELEVENLABS_API_KEY", "k"), ("PATH", "/nonexistent")]);
        let client = TtsClient::new(TtsOptions {
            env,
            ..TtsOptions::default()
        });
        let base = Path::new("/");
        assert_eq!(
            client.credential_status(&instance(ProviderType::Elevenlabs), base),
            CredentialStatus::Ready
        );
        assert_eq!(
            client.credential_status(&instance(ProviderType::Cartesia), base),
            CredentialStatus::Missing {
                env_var: "CARTESIA_API_KEY".into()
            }
        );
        assert_eq!(
            client.credential_status(&instance(ProviderType::Openai), base),
            CredentialStatus::Missing {
                env_var: "OPENAI_API_KEY".into()
            }
        );
        let mut local_openai = instance(ProviderType::Openai);
        local_openai.base_url = Some("http://localhost:8880/v1".into());
        assert_eq!(
            client.credential_status(&local_openai, base),
            CredentialStatus::NotRequired
        );
        local_openai.api_key_env = Some("MY_LOCAL_KEY".into());
        assert_eq!(
            client.credential_status(&local_openai, base),
            CredentialStatus::Missing {
                env_var: "MY_LOCAL_KEY".into()
            }
        );
        assert_eq!(
            client.credential_status(&instance(ProviderType::Kokoro), base),
            CredentialStatus::NotRequired
        );
        assert_eq!(
            client.credential_status(&instance(ProviderType::Mock), base),
            CredentialStatus::NotRequired
        );
        assert!(matches!(
            client.credential_status(&instance(ProviderType::Piper), base),
            CredentialStatus::Unavailable { .. }
        ));
        assert!(matches!(
            client.credential_status(&instance(ProviderType::Command), base),
            CredentialStatus::Unavailable { .. }
        ));
        let say = client.credential_status(&instance(ProviderType::MacosSay), base);
        assert_eq!(say.is_usable(), cfg!(target_os = "macos"));
        // A key the host policy would refuse makes the instance unusable, with the fix in the reason.
        let mut exfil = instance(ProviderType::Elevenlabs);
        exfil.base_url = Some("https://exfil.example".into());
        match client.credential_status(&exfil, base) {
            CredentialStatus::Unavailable { reason } => assert!(
                reason.contains(
                    "refusing to send the API key in ELEVENLABS_API_KEY to `exfil.example`"
                ) && reason.contains("SONICCRAFT_ALLOWED_HOSTS=exfil.example"),
                "{reason}"
            ),
            other => panic!("{other:?}"),
        }
        let allowed = TtsClient::new(TtsOptions {
            env: EnvSource::fixed([
                ("ELEVENLABS_API_KEY", "k"),
                ("SONICCRAFT_ALLOWED_HOSTS", "exfil.example"),
            ]),
            ..TtsOptions::default()
        });
        assert_eq!(
            allowed.credential_status(&exfil, base),
            CredentialStatus::Ready
        );
    }
}
