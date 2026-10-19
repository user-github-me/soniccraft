//! Content-addressed TTS cache (DESIGN.md §5.1).
//!
//! The key is BLAKE3 over the canonical JSON of
//! `{cache_version, provider_type, api_version, base_url, model, voice_id, language, text, settings,
//! output_format, command, stdin}` (absent values omitted):
//!
//! - `api_version`: the `Cartesia-Version` for cartesia, the mock algorithm version for mock;
//! - `base_url`: openai and kokoro only, and only when it differs from the default;
//! - `model`, `voice_id`: effective values (provider defaults applied);
//! - `settings`: instance settings overlaid with the voice settings;
//! - `output_format`: the requested provider-native format (elevenlabs, openai, cartesia; command: the
//!   `{output}` extension when set);
//! - `command`, `stdin`: the templates (command and piper only);
//! - `program` (command and piper): the program that runs, resolved like synthesis resolves it (relative
//!   paths against the timeline directory, bare names on `PATH`), with a BLAKE3 hash of its contents, so two
//!   projects' `./tts.sh` never share an entry and editing a script re-synthesizes;
//! - `files` (command and piper with a custom `command`): hashes of the files that literal template
//!   arguments name (relative to the timeline directory), e.g. the script in `["python3", "tts.py", …]`;
//! - `base_dir` (command, and piper with a custom `command`): the directory the program runs in;
//! - `model_file` (piper): the resolved `.onnx` model and the hashes of it and its `.onnx.json` config.
//!
//! Each entry is `<dir>/<key>.wav` (32-bit float WAV of the decoded provider audio at its native rate,
//! untrimmed) plus `<dir>/<key>.json` (metadata including word timings). The WAV is written first and the
//! JSON last, both atomically, so a reader that finds valid JSON finds a complete WAV. Temporary files are
//! created with `create_new` (`O_EXCL`) at unpredictable names in the cache directory, so a planted file or
//! symlink is never written through. Entries that fail validation are ignored and overwritten by the next
//! synthesis.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use soniccraft_core::ir::{ProviderInstance, ProviderType};
use soniccraft_dsp::AudioBuffer;

use crate::canonical;
use crate::defaults::{
    self, CARTESIA_API_VERSION, ELEVENLABS_DEFAULT_FORMAT, MOCK_ALGORITHM_VERSION,
};
use crate::env::EnvSource;
use crate::error::{TtsError, TtsErrorKind};
use crate::process;
use crate::providers::{cartesia, command, kokoro, piper};
use crate::settings::{self, Resolved};
use crate::types::{CachedMeta, SynthesisRequest, WordTiming};

/// Version of the cache key document and entry format. Bump it to invalidate every cache.
pub const CACHE_VERSION: u32 = 1;

/// The cache key of a request: 64 lowercase hex characters. Programs and piper models are looked up in the
/// process environment; [`crate::TtsClient::cache_key`] uses the client's.
///
/// Fails only when a setting holds a non-finite number (impossible for values parsed from JSON or YAML).
pub fn cache_key(request: &SynthesisRequest) -> Result<String, TtsError> {
    let resolved = Resolved::new(&request.provider, &request.voice);
    key_for(request, &resolved, &EnvSource::Process)
}

pub(crate) fn key_for(
    request: &SynthesisRequest,
    resolved: &Resolved,
    env: &EnvSource,
) -> Result<String, TtsError> {
    let document = key_document(request, resolved, env);
    let canonical = canonical::canonical_json(&document).map_err(|e| {
        TtsError::new(
            TtsErrorKind::InvalidRequest,
            &request.provider,
            format!("cannot compute the cache key: {e}"),
        )
    })?;
    Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
}

/// The document hashed into the cache key (see the module docs).
pub(crate) fn key_document(
    request: &SynthesisRequest,
    resolved: &Resolved,
    env: &EnvSource,
) -> Value {
    let provider = &request.provider;
    let t = provider.provider_type;
    let mut doc = Map::new();
    let mut put = |key: &str, value: Value| {
        doc.insert(key.to_owned(), value);
    };
    put("cache_version", Value::from(CACHE_VERSION));
    put("provider_type", Value::from(t.as_str()));
    if let Some(version) = api_version(t) {
        put("api_version", Value::from(version));
    }
    if let Some(base) = keyed_base_url(provider) {
        put("base_url", Value::from(base));
    }
    if let Some(model) = &resolved.model {
        put("model", Value::from(model.as_str()));
    }
    if let Some(voice) = &resolved.voice_id {
        put("voice_id", Value::from(voice.as_str()));
    }
    if let Some(language) = &resolved.language {
        put("language", Value::from(language.as_str()));
    }
    put("text", Value::from(request.text.as_str()));
    if !resolved.settings.is_empty() {
        put("settings", Value::Object(resolved.settings.clone()));
    }
    if let Some(format) = keyed_output_format(provider) {
        put("output_format", Value::from(format));
    }
    if matches!(t, ProviderType::Command | ProviderType::Piper) {
        if let Some(argv) = &provider.command {
            put("command", Value::from(argv.clone()));
        }
        if let Some(stdin) = &provider.stdin {
            put("stdin", Value::from(stdin.as_str()));
        }
        let custom = provider
            .command
            .as_deref()
            .and_then(<[String]>::first)
            .map(String::as_str);
        let program = match (t, custom) {
            (_, Some(program)) => Some(program),
            (ProviderType::Piper, None) => Some("piper"),
            _ => None,
        };
        if let Some(program) = program {
            put("program", program_identity(program, env, &request.base_dir));
        }
        if let Some(argv) = &provider.command {
            let files = file_arguments(argv, &request.base_dir);
            if !files.is_empty() {
                put("files", Value::Array(files));
            }
        }
        if t == ProviderType::Command || custom.is_some() {
            put(
                "base_dir",
                Value::from(request.base_dir.to_string_lossy().as_ref()),
            );
        }
        if t == ProviderType::Piper
            && let Some(voice) = &resolved.voice_id
            && let Ok(model) = piper::resolve_model(voice.trim(), provider, env, &request.base_dir)
        {
            let config = PathBuf::from(format!("{}.json", model.display()));
            put(
                "model_file",
                serde_json::json!({
                    "path": model.to_string_lossy(),
                    "hash": file_hash(&model),
                    "config_hash": file_hash(&config),
                }),
            );
        }
    }
    Value::Object(doc)
}

/// The program a template runs, as synthesis would resolve it, with a hash of its contents. Templated
/// programs (`{voice_id}`) are only known at synthesis time and keyed by their template (in `command`).
fn program_identity(program: &str, env: &EnvSource, base_dir: &Path) -> Value {
    if program.contains('{') {
        return serde_json::json!({ "template": program });
    }
    match process::find_program(program, env, base_dir) {
        Some(path) => serde_json::json!({
            "path": path.to_string_lossy(),
            "hash": file_hash(&path),
        }),
        None => serde_json::json!({ "missing": program }),
    }
}

/// The literal arguments of a template (no placeholder, not an option) that name an existing file, resolved
/// against the directory the program runs in, as `{index, hash}`: a script handed to an interpreter
/// (`["python3", "tts.py", …]`) or a model file is part of what produces the audio.
fn file_arguments(argv: &[String], base_dir: &Path) -> Vec<Value> {
    argv.iter()
        .enumerate()
        .skip(1)
        .filter(|(_, arg)| !arg.is_empty() && !arg.contains('{') && !arg.starts_with('-'))
        .filter_map(|(index, arg)| {
            let path = Path::new(arg);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base_dir.join(path)
            };
            path.is_file()
                .then(|| serde_json::json!({ "index": index, "hash": file_hash(&path) }))
        })
        .collect()
}

/// Files larger than this are identified by size and modification time instead of a content hash.
const MAX_HASHED_FILE_BYTES: u64 = 1 << 30;

/// What identifies a version of a file for [`FILE_HASHES`]: its path, size and modification time.
type FileVersion = (PathBuf, u64, Option<SystemTime>);

/// Content hashes of files, memoized per [`FileVersion`] for the life of the process.
static FILE_HASHES: Mutex<BTreeMap<FileVersion, String>> = Mutex::new(BTreeMap::new());

/// Files modified more recently than this are hashed every time: file systems record modification times
/// coarsely (Linux to the kernel tick, some to the second), so two quick same-size edits can share one and
/// the memo would serve the first version's hash (git's "racy timestamp" rule).
const MEMO_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// BLAKE3 of the file at `path` (`null` when it cannot be read); hashed once per process and version.
fn file_hash(path: &Path) -> Value {
    let Ok(meta) = std::fs::metadata(path) else {
        return Value::Null;
    };
    let modified = meta.modified().ok();
    let memoizable = modified.is_some_and(|m| {
        SystemTime::now()
            .duration_since(m)
            .is_ok_and(|age| age >= MEMO_MIN_AGE)
    });
    let memo = (path.to_path_buf(), meta.len(), modified);
    if memoizable
        && let Some(hash) = FILE_HASHES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&memo)
    {
        return Value::from(hash.as_str());
    }
    let hash = if meta.len() > MAX_HASHED_FILE_BYTES {
        let nanos = modified
            .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        format!("size:{}:mtime:{nanos}", meta.len())
    } else {
        let mut hasher = blake3::Hasher::new();
        match std::fs::File::open(path).and_then(|mut file| std::io::copy(&mut file, &mut hasher)) {
            Ok(_) => hasher.finalize().to_hex().to_string(),
            Err(_) => return Value::Null,
        }
    };
    if memoizable {
        FILE_HASHES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(memo, hash.clone());
    }
    Value::from(hash)
}

fn api_version(t: ProviderType) -> Option<&'static str> {
    match t {
        ProviderType::Cartesia => Some(CARTESIA_API_VERSION),
        ProviderType::Mock => Some(MOCK_ALGORITHM_VERSION),
        _ => None,
    }
}

/// openai/kokoro base URL, only when it is not the default.
fn keyed_base_url(provider: &ProviderInstance) -> Option<String> {
    let configured = settings::non_empty(provider.base_url.as_deref())?;
    match provider.provider_type {
        ProviderType::Openai => {
            let base = crate::http::trim_base(configured);
            let default =
                defaults::default_base_url(ProviderType::Openai).map(crate::http::trim_base);
            (Some(&base) != default.as_ref()).then_some(base)
        }
        ProviderType::Kokoro => {
            let root = kokoro::normalize_root(configured);
            (root != defaults::KOKORO_DEFAULT_ROOT).then_some(root)
        }
        _ => None,
    }
}

fn keyed_output_format(provider: &ProviderInstance) -> Option<String> {
    let configured = settings::non_empty(provider.output_format.as_deref());
    match provider.provider_type {
        ProviderType::Elevenlabs => Some(
            configured
                .unwrap_or(ELEVENLABS_DEFAULT_FORMAT)
                .to_ascii_lowercase(),
        ),
        ProviderType::Openai => Some(configured.unwrap_or("pcm").to_ascii_lowercase()),
        ProviderType::Cartesia => Some(
            cartesia::parse_output_format(configured)
                .map(|f| f.name())
                .unwrap_or_else(|_| configured.unwrap_or_default().to_ascii_lowercase()),
        ),
        ProviderType::Command => configured.map(|_| command::output_extension(provider)),
        // Kokoro always requests raw PCM; local engines have no format choice.
        ProviderType::Kokoro
        | ProviderType::Piper
        | ProviderType::MacosSay
        | ProviderType::Mock => None,
    }
}

/// The cache directory DESIGN.md §5.1 prescribes: `explicit` (CLI `--cache-dir`), else
/// `SONICCRAFT_CACHE_DIR` from `env`, else `<timeline_dir>/.soniccraft/cache`. Paths are returned as given.
pub fn resolve_cache_dir(explicit: Option<&Path>, env: &EnvSource, timeline_dir: &Path) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| env.get(CACHE_DIR_ENV).map(PathBuf::from))
        .unwrap_or_else(|| timeline_dir.join(".soniccraft").join("cache"))
}

/// Environment variable naming the cache directory.
const CACHE_DIR_ENV: &str = "SONICCRAFT_CACHE_DIR";

/// Size of a cache directory (see [`cache_stats`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct CacheStats {
    /// Complete entries: a `<key>.json` with its `<key>.wav`.
    pub entries: usize,
    /// Bytes used by cache files: entries, half-written orphans and leftover temporary files.
    pub bytes: u64,
}

/// What a file in a cache directory is.
#[derive(Debug, PartialEq)]
enum CacheFile<'a> {
    Meta(&'a str),
    Audio(&'a str),
    /// `.<key>.(wav|json).<suffix>.tmp`, left behind by an interrupted write.
    Temp,
}

fn is_key(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn classify(name: &str) -> Option<CacheFile<'_>> {
    if let Some(key) = name.strip_suffix(".json").filter(|k| is_key(k)) {
        return Some(CacheFile::Meta(key));
    }
    if let Some(key) = name.strip_suffix(".wav").filter(|k| is_key(k)) {
        return Some(CacheFile::Audio(key));
    }
    let inner = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let (key, rest) = inner.split_at_checked(64)?;
    (is_key(key) && (rest.starts_with(".wav.") || rest.starts_with(".json.")))
        .then_some(CacheFile::Temp)
}

/// The cache files in `dir` with their sizes (sorted by name; other files and directories are ignored).
fn cache_files(dir: &Path) -> std::io::Result<Vec<(PathBuf, String, u64)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let metadata = entry.metadata()?;
        if metadata.is_file() && classify(&name).is_some() {
            files.push((entry.path(), name, metadata.len()));
        }
    }
    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

fn count_entries(files: &[(PathBuf, String, u64)]) -> usize {
    let mut meta = std::collections::BTreeSet::new();
    let mut audio = std::collections::BTreeSet::new();
    for (_, name, _) in files {
        match classify(name) {
            Some(CacheFile::Meta(key)) => {
                meta.insert(key);
            }
            Some(CacheFile::Audio(key)) => {
                audio.insert(key);
            }
            _ => {}
        }
    }
    meta.intersection(&audio).count()
}

/// Entry count and size of the cache in `dir` (an absent directory is an empty cache).
pub fn cache_stats(dir: &Path) -> std::io::Result<CacheStats> {
    let files = cache_files(dir)?;
    Ok(CacheStats {
        entries: count_entries(&files),
        bytes: files.iter().map(|f| f.2).sum(),
    })
}

/// Delete every cache file in `dir` (entries, orphans, temporary files); returns the number of complete
/// entries removed. Only files named like cache files are touched, so a mistyped directory is safe.
pub fn clear_cache(dir: &Path) -> std::io::Result<usize> {
    let files = cache_files(dir)?;
    let entries = count_entries(&files);
    for (path, _, _) in &files {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(entries)
}

/// The `<key>.json` sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct EntryMeta {
    pub cache_version: u32,
    pub key: String,
    pub provider_type: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub voice_id: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    pub text: String,
    /// Provider-native format actually received.
    #[serde(default)]
    pub output_format: Option<String>,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub duration_secs: f64,
    #[serde(default)]
    pub words: Option<Vec<WordTiming>>,
}

impl EntryMeta {
    pub(crate) fn to_cached_meta(&self) -> CachedMeta {
        CachedMeta {
            frames: self.frames,
            sample_rate: self.sample_rate,
            channels: self.channels,
            duration_secs: self.duration_secs,
            words: self.words.clone(),
        }
    }
}

/// A cache directory.
#[derive(Debug, Clone)]
pub(crate) struct CacheStore {
    dir: PathBuf,
}

impl CacheStore {
    pub(crate) fn new(dir: PathBuf) -> Self {
        CacheStore { dir }
    }

    pub(crate) fn wav_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.wav"))
    }

    pub(crate) fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// Metadata of a complete entry, checked cheaply (JSON contents and WAV size). `Ok(None)` = no entry;
    /// `Err(reason)` = a corrupt or stale entry.
    pub(crate) fn peek(&self, key: &str) -> Result<Option<EntryMeta>, String> {
        let meta_path = self.meta_path(key);
        let bytes = match std::fs::read(&meta_path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read {}: {e}", meta_path.display())),
        };
        let meta: EntryMeta = serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid {}: {e}", meta_path.display()))?;
        if meta.cache_version != CACHE_VERSION || meta.key != key {
            return Err(format!(
                "{} belongs to another cache version or key",
                meta_path.display()
            ));
        }
        if meta.sample_rate == 0 || meta.channels == 0 || meta.frames == 0 {
            return Err(format!("{} describes empty audio", meta_path.display()));
        }
        let wav_path = self.wav_path(key);
        let wav_len = std::fs::metadata(&wav_path)
            .map_err(|e| format!("cannot stat {}: {e}", wav_path.display()))?
            .len();
        if wav_len
            < meta
                .frames
                .saturating_mul(u64::from(meta.channels))
                .saturating_mul(4)
        {
            return Err(format!("{} is truncated", wav_path.display()));
        }
        Ok(Some(meta))
    }

    /// A complete entry with its decoded audio.
    pub(crate) fn load(&self, key: &str) -> Result<Option<(AudioBuffer, EntryMeta)>, String> {
        let Some(meta) = self.peek(key)? else {
            return Ok(None);
        };
        let wav_path = self.wav_path(key);
        let (audio, repaired) = soniccraft_dsp::wav::read_repaired(&wav_path)
            .map_err(|e| format!("cannot read {}: {e}", wav_path.display()))?;
        if repaired > 0 {
            // Synthesis zeroes non-finite samples before storing, so the entry was damaged or tampered
            // with: re-synthesize rather than silently play the repaired audio.
            return Err(format!(
                "{} contains {repaired} non-finite sample{} (NaN/±∞)",
                wav_path.display(),
                if repaired == 1 { "" } else { "s" }
            ));
        }
        if audio.sample_rate != meta.sample_rate
            || audio.num_channels() != usize::from(meta.channels)
            || audio.frames() as u64 != meta.frames
        {
            return Err(format!(
                "{} does not match its metadata",
                wav_path.display()
            ));
        }
        Ok(Some((audio, meta)))
    }

    /// Write an entry: the WAV, then the JSON, each through [`write_atomic`].
    pub(crate) fn store(
        &self,
        key: &str,
        audio: &AudioBuffer,
        meta: &EntryMeta,
    ) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let wav_path = self.wav_path(key);
        let wav =
            soniccraft_dsp::wav::encode_to_vec(audio, soniccraft_dsp::wav::WavEncoding::Float32)
                .map_err(|e| format!("cannot encode {}: {e}", wav_path.display()))?;
        write_atomic(&wav_path, &wav)
            .map_err(|e| format!("cannot write {}: {e}", wav_path.display()))?;
        let json = serde_json::to_vec_pretty(meta)
            .map_err(|e| format!("cannot serialize cache metadata: {e}"))?;
        let meta_path = self.meta_path(key);
        write_atomic(&meta_path, &json)
            .map_err(|e| format!("cannot write {}: {e}", meta_path.display()))
    }
}

/// Write `bytes` to `path` atomically: a temporary sibling `.<name>.<random>.tmp` created with `create_new`
/// (`O_EXCL`: an existing file or symlink at that name is never opened, and another random name is tried),
/// then a rename over `path` (which replaces a symlink at `path` rather than following it). The temporary
/// file is removed on failure.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!(".{file_name}.");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".tmp").rand_bytes(16);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // What `std::fs::write` gives (before the umask): cache entries are not secret.
        builder.permissions(std::fs::Permissions::from_mode(0o644));
    }
    let mut tmp = builder.tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{instance, voice};
    use serde_json::json;

    fn request(t: ProviderType, settings: Value) -> SynthesisRequest {
        SynthesisRequest {
            cue_id: "c1".into(),
            provider: instance(t),
            voice: voice(t, Some("voice-1"), None, Some("en"), settings),
            text: "Hello world.".into(),
            base_dir: PathBuf::from("/project"),
        }
    }

    #[test]
    fn key_document_contents() {
        let r = request(ProviderType::Cartesia, json!({"speed": 1.1}));
        let doc = key_document(
            &r,
            &Resolved::new(&r.provider, &r.voice),
            &EnvSource::Process,
        );
        assert_eq!(
            doc,
            json!({"cache_version": 1, "provider_type": "cartesia", "api_version": "2026-08-14", "model": "sonic-3.6",
                   "voice_id": "voice-1", "language": "en", "text": "Hello world.", "settings": {"speed": 1.1},
                   "output_format": "pcm_f32le_48000"})
        );
        let m = request(ProviderType::Mock, json!({}));
        let doc = key_document(
            &m,
            &Resolved::new(&m.provider, &m.voice),
            &EnvSource::Process,
        );
        assert_eq!(doc["api_version"], MOCK_ALGORITHM_VERSION);
        assert!(doc.get("settings").is_none());
        assert!(doc.get("output_format").is_none());
    }

    #[test]
    fn key_independent_of_cue_id_base_dir_and_settings_order() {
        let mut a = request(
            ProviderType::Elevenlabs,
            json!({"stability": 0.5, "speed": 1.0}),
        );
        let b = request(
            ProviderType::Elevenlabs,
            json!({"speed": 1, "stability": 0.5}),
        );
        assert_eq!(cache_key(&a).unwrap(), cache_key(&b).unwrap());
        a.cue_id = "other".into();
        a.base_dir = PathBuf::from("/elsewhere");
        a.provider.name = "renamed".into();
        a.provider.max_concurrency = 9;
        assert_eq!(cache_key(&a).unwrap(), cache_key(&b).unwrap());
    }

    #[test]
    fn key_changes_with_inputs() {
        let base = request(ProviderType::Elevenlabs, json!({"stability": 0.5}));
        let k = cache_key(&base).unwrap();
        let mut texts = base.clone();
        texts.text.push('!');
        let mut settings = base.clone();
        settings
            .voice
            .settings
            .insert("stability".into(), json!(0.6));
        let mut model = base.clone();
        model.voice.model = Some("eleven_v3".into());
        let mut format = base.clone();
        format.provider.output_format = Some("pcm_44100".into());
        let mut instance_settings = base.clone();
        instance_settings
            .provider
            .settings
            .insert("style".into(), json!(0.2));
        for changed in [texts, settings, model, format, instance_settings] {
            assert_ne!(cache_key(&changed).unwrap(), k);
        }
        // The default output format is explicit in the key: configuring it changes nothing.
        let mut same = base.clone();
        same.provider.output_format = Some("pcm_24000".into());
        assert_eq!(cache_key(&same).unwrap(), k);
    }

    #[test]
    fn base_url_only_when_not_default() {
        let mut r = request(ProviderType::Openai, json!({}));
        let k = cache_key(&r).unwrap();
        r.provider.base_url = Some("https://api.openai.com/v1/".into());
        assert_eq!(cache_key(&r).unwrap(), k);
        r.provider.base_url = Some("http://localhost:8880/v1".into());
        assert_ne!(cache_key(&r).unwrap(), k);
        let mut kok = request(ProviderType::Kokoro, json!({}));
        let kk = cache_key(&kok).unwrap();
        kok.provider.base_url = Some("http://localhost:8880/v1".into());
        assert_eq!(cache_key(&kok).unwrap(), kk);
        // Regional ElevenLabs endpoints produce the same audio.
        let mut el = request(ProviderType::Elevenlabs, json!({}));
        let ek = cache_key(&el).unwrap();
        el.provider.base_url = Some("https://api.eu.residency.elevenlabs.io".into());
        assert_eq!(cache_key(&el).unwrap(), ek);
    }

    #[test]
    fn command_templates_are_keyed() {
        let mut r = request(ProviderType::Command, json!({}));
        r.provider.command = Some(vec!["tts".into(), "{output}".into()]);
        let k = cache_key(&r).unwrap();
        r.provider.stdin = Some("{text}".into());
        let with_stdin = cache_key(&r).unwrap();
        assert_ne!(k, with_stdin);
        r.provider.command = Some(vec!["tts2".into(), "{output}".into()]);
        assert_ne!(cache_key(&r).unwrap(), with_stdin);
    }

    #[test]
    fn command_keys_identify_the_program_and_its_directory() {
        // The reviewer's case: two projects, each with its own `./tts.sh`, one shared cache directory.
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("projA"), tmp.path().join("projB"));
        for (dir, body) in [(&a, "echo 220"), (&b, "echo 880")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("tts.sh"), body).unwrap();
        }
        let make = |dir: &Path| {
            let mut r = request(ProviderType::Command, json!({}));
            r.provider.command = Some(vec!["./tts.sh".into(), "{output}".into()]);
            r.base_dir = dir.to_path_buf();
            r
        };
        let (ka, kb) = (cache_key(&make(&a)).unwrap(), cache_key(&make(&b)).unwrap());
        assert_ne!(ka, kb);
        assert_eq!(
            cache_key(&make(&a)).unwrap(),
            ka,
            "stable while nothing changes"
        );
        let doc = key_document(
            &make(&a),
            &Resolved::new(&make(&a).provider, &make(&a).voice),
            &EnvSource::Process,
        );
        assert_eq!(
            doc["program"]["path"],
            a.join("./tts.sh").to_string_lossy().as_ref()
        );
        assert_eq!(
            doc["program"]["hash"],
            blake3::hash(b"echo 220").to_hex().to_string()
        );
        assert_eq!(doc["base_dir"], a.to_string_lossy().as_ref());
        // Editing the script re-synthesizes, even at the same path (and same size).
        std::fs::write(a.join("tts.sh"), "echo 221").unwrap();
        assert_ne!(cache_key(&make(&a)).unwrap(), ka);
        // A script handed to an interpreter is keyed by its contents too.
        std::fs::write(a.join("tts.py"), "print(1)").unwrap();
        let interpreted = |body: &str| {
            std::fs::write(a.join("tts.py"), body).unwrap();
            let mut r = make(&a);
            r.provider.command = Some(vec![
                "/bin/sh".into(),
                "tts.py".into(),
                "-o".into(),
                "{output}".into(),
            ]);
            r
        };
        let first = interpreted("print(1)");
        let doc = key_document(
            &first,
            &Resolved::new(&first.provider, &first.voice),
            &EnvSource::Process,
        );
        assert_eq!(
            doc["files"],
            json!([{"index": 1, "hash": blake3::hash(b"print(1)").to_hex().to_string()}])
        );
        let k1 = cache_key(&first).unwrap();
        assert_ne!(cache_key(&interpreted("print(2)")).unwrap(), k1);
        // A program that cannot be found is keyed by its name.
        let mut missing = make(&a);
        missing.provider.command = Some(vec!["./nope.sh".into()]);
        let doc = key_document(
            &missing,
            &Resolved::new(&missing.provider, &missing.voice),
            &EnvSource::Process,
        );
        assert_eq!(doc["program"], json!({"missing": "./nope.sh"}));
    }

    #[test]
    fn piper_keys_identify_the_model_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        for (dir, model) in [(&a, "model A"), (&b, "model B")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("en_US-test-medium.onnx"), model).unwrap();
        }
        let env = EnvSource::fixed([("PATH", "/nonexistent")]);
        let make = |dir: &Path| {
            let mut r = request(ProviderType::Piper, json!({}));
            r.voice.voice_id = Some("en_US-test-medium".into());
            r.base_dir = dir.to_path_buf();
            r
        };
        let key =
            |r: &SynthesisRequest| key_for(r, &Resolved::new(&r.provider, &r.voice), &env).unwrap();
        assert_ne!(
            key(&make(&a)),
            key(&make(&b)),
            "same voice name, different models"
        );
        let before = key(&make(&a));
        std::fs::write(a.join("en_US-test-medium.onnx.json"), "{}").unwrap();
        assert_ne!(key(&make(&a)), before, "the model config is keyed too");
    }

    #[test]
    fn golden_key() {
        // Freezes the key format: changing it silently invalidates every user's cache, so bump
        // CACHE_VERSION instead.
        let r = request(
            ProviderType::Elevenlabs,
            json!({"stability": 0.5, "similarity_boost": 0.75, "speed": 1.05}),
        );
        assert_eq!(cache_key(&r).unwrap(), GOLDEN_ELEVENLABS_KEY);
        let reordered = request(
            ProviderType::Elevenlabs,
            json!({"speed": 1.05, "similarity_boost": 0.75, "stability": 0.5}),
        );
        assert_eq!(cache_key(&reordered).unwrap(), GOLDEN_ELEVENLABS_KEY);
        let canonical = canonical::canonical_json(&key_document(
            &r,
            &Resolved::new(&r.provider, &r.voice),
            &EnvSource::Process,
        ))
        .unwrap();
        assert_eq!(
            canonical,
            r#"{"cache_version":1,"language":"en","model":"eleven_multilingual_v2","output_format":"pcm_24000","provider_type":"elevenlabs","settings":{"similarity_boost":0.75,"speed":1.05,"stability":0.5},"text":"Hello world.","voice_id":"voice-1"}"#
        );
    }

    const GOLDEN_ELEVENLABS_KEY: &str =
        "101cbf9bfd64762aad3d5264c4e37afa6fe9f4bf94b0b5b794ff1060c64f3406";

    #[test]
    fn peek_rejects_bad_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = CacheStore::new(dir.path().to_path_buf());
        let key = "k".repeat(64);
        assert_eq!(store.peek(&key), Ok(None));
        std::fs::write(store.meta_path(&key), b"{not json").unwrap();
        assert!(store.peek(&key).is_err());
        let meta = EntryMeta {
            cache_version: CACHE_VERSION,
            key: key.clone(),
            provider_type: "mock".into(),
            model: None,
            voice_id: None,
            language: None,
            text: "x".into(),
            output_format: None,
            sample_rate: 24_000,
            channels: 1,
            frames: 1000,
            duration_secs: 1000.0 / 24_000.0,
            words: None,
        };
        std::fs::write(store.meta_path(&key), serde_json::to_vec(&meta).unwrap()).unwrap();
        assert!(
            store.peek(&key).unwrap_err().contains("cannot stat"),
            "missing wav"
        );
        std::fs::write(store.wav_path(&key), vec![0u8; 100]).unwrap();
        assert!(store.peek(&key).unwrap_err().contains("truncated"));
        std::fs::write(store.wav_path(&key), vec![0u8; 4_100]).unwrap();
        assert_eq!(store.peek(&key).unwrap().map(|m| m.frames), Some(1000));
        let mut other = meta.clone();
        other.key = "z".repeat(64);
        std::fs::write(store.meta_path(&key), serde_json::to_vec(&other).unwrap()).unwrap();
        assert!(store.peek(&key).is_err());
    }

    #[test]
    fn classifies_cache_file_names() {
        let key = "a".repeat(64);
        assert_eq!(
            classify(&format!("{key}.json")),
            Some(CacheFile::Meta(&key))
        );
        assert_eq!(
            classify(&format!("{key}.wav")),
            Some(CacheFile::Audio(&key))
        );
        assert_eq!(
            classify(&format!(".{key}.wav.123.4.tmp")),
            Some(CacheFile::Temp)
        );
        assert_eq!(
            classify(&format!(".{key}.json.9.0.tmp")),
            Some(CacheFile::Temp)
        );
        for other in [
            "notes.txt".to_owned(),
            format!("{}.wav", "A".repeat(64)),
            format!("{}.wav", "a".repeat(63)),
            format!(".{key}.mp3.1.tmp"),
            format!("{key}.wav.bak"),
            ".tmp".to_owned(),
        ] {
            assert_eq!(classify(&other), None, "{other}");
        }
    }

    #[test]
    fn stats_and_clear_touch_only_cache_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            cache_stats(&dir.path().join("missing")).unwrap(),
            CacheStats::default()
        );
        assert_eq!(clear_cache(&dir.path().join("missing")).unwrap(), 0);
        let (k1, k2) = ("1".repeat(64), "2".repeat(64));
        let write =
            |name: &str, len: usize| std::fs::write(dir.path().join(name), vec![0u8; len]).unwrap();
        write(&format!("{k1}.json"), 10);
        write(&format!("{k1}.wav"), 100);
        write(&format!("{k2}.wav"), 50); // orphan: no metadata
        write(&format!(".{k2}.json.1.2.tmp"), 5);
        write("keep-me.txt", 7);
        std::fs::create_dir(dir.path().join(format!("{k1}.wav.d"))).unwrap();
        assert_eq!(
            cache_stats(dir.path()).unwrap(),
            CacheStats {
                entries: 1,
                bytes: 165
            }
        );
        assert_eq!(clear_cache(dir.path()).unwrap(), 1);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [format!("{k1}.wav.d"), "keep-me.txt".to_owned()]);
    }

    #[test]
    fn cache_dir_resolution_order() {
        let env = EnvSource::fixed([("SONICCRAFT_CACHE_DIR", "/env/cache")]);
        let none = EnvSource::fixed([("X", "1")]);
        let timeline = Path::new("/project");
        assert_eq!(
            resolve_cache_dir(Some(Path::new("/cli")), &env, timeline),
            PathBuf::from("/cli")
        );
        assert_eq!(
            resolve_cache_dir(None, &env, timeline),
            PathBuf::from("/env/cache")
        );
        assert_eq!(
            resolve_cache_dir(None, &none, timeline),
            PathBuf::from("/project/.soniccraft/cache")
        );
    }

    #[cfg(unix)]
    #[test]
    fn temp_files_never_follow_planted_symlinks() {
        // The reviewer's attack: a writable cache directory with symlinks at the temp names a writer would
        // pick (`.<name>.<pid>.<counter>.tmp`), pointing at a file outside it.
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        std::fs::write(&victim, "precious").unwrap();
        let cache = dir.path().join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let key = "a".repeat(64);
        let pid = std::process::id();
        for name in [format!("{key}.wav"), format!("{key}.json")] {
            for counter in 0..64 {
                for sep in [".", "-"] {
                    std::os::unix::fs::symlink(
                        &victim,
                        cache.join(format!(".{name}.{pid}{sep}{counter}.tmp")),
                    )
                    .unwrap();
                }
            }
        }
        // A symlink at the destination itself is replaced, not written through.
        std::os::unix::fs::symlink(&victim, cache.join(format!("{key}.json"))).unwrap();
        let store = CacheStore::new(cache.clone());
        let audio = AudioBuffer::mono(24_000, vec![0.25; 2_400]);
        let meta = EntryMeta {
            cache_version: CACHE_VERSION,
            key: key.clone(),
            provider_type: "mock".into(),
            model: None,
            voice_id: None,
            language: None,
            text: "x".into(),
            output_format: None,
            sample_rate: 24_000,
            channels: 1,
            frames: 2_400,
            duration_secs: 0.1,
            words: None,
        };
        store.store(&key, &audio, &meta).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious");
        for name in [format!("{key}.wav"), format!("{key}.json")] {
            let path = cache.join(&name);
            assert!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_file(),
                "{name} is a regular file"
            );
        }
        let (loaded, _) = store.load(&key).unwrap().unwrap();
        assert_eq!(loaded, audio);
        // Our own temp files are gone, and their names were not the predictable ones.
        let leftovers = std::fs::read_dir(&cache)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn interrupted_writes_leave_temp_names_that_cache_clear_recognizes() {
        let dir = tempfile::tempdir().unwrap();
        let key = "b".repeat(64);
        let mut names = std::collections::BTreeSet::new();
        for entry in [format!("{key}.wav"), format!("{key}.json")] {
            for _ in 0..4 {
                // The same builder settings as `write_atomic`.
                let prefix = format!(".{entry}.");
                let mut builder = tempfile::Builder::new();
                builder.prefix(&prefix).suffix(".tmp").rand_bytes(16);
                let tmp = builder.tempfile_in(dir.path()).unwrap();
                let name = tmp
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                assert_eq!(classify(&name), Some(CacheFile::Temp), "{name}");
                names.insert(name);
            }
        }
        assert_eq!(names.len(), 8, "every temp name is different");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1);
    }
}
