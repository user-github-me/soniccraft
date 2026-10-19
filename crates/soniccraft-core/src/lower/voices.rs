//! Provider instances (DESIGN.md §2.8) and voice resolution (§2.7).

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value};

use super::check::{self, fmt_num};
use crate::diagnostics::{self, Diagnostic, codes};
use crate::ir::{ProviderInstance, ProviderType, VoiceIr};
use crate::model::{ProviderSpec, SequenceItem, Track, VoiceSpec};
use crate::parse::secrets;
use crate::providers::{self, DEFAULT_TIMEOUT_MS};

/// Status of a provider name written somewhere in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RefStatus {
    /// A configured instance or a built-in provider.
    Ok,
    /// Not configured and not a built-in (`unknown_provider` reported).
    Unknown,
    /// A configured instance whose configuration has errors (reported at the instance).
    Broken,
}

/// The provider instances of a timeline.
pub(super) struct Providers {
    /// Configured instances in document order (None = invalid configuration).
    configured: IndexMap<String, Option<ProviderInstance>>,
    /// Built-in providers used by at least one cue, by the name as written, in first-use order.
    builtins: IndexMap<String, ProviderInstance>,
    /// Paths whose provider reference has been checked, with the outcome.
    checked: HashMap<String, RefStatus>,
}

/// Resolve the base URL: configured value, else the type's base-URL env var (Kokoro), else the default.
fn resolve_base_url(provider_type: ProviderType, configured: Option<&str>) -> Option<String> {
    let meta = providers::meta(provider_type);
    if let Some(url) = configured.map(str::trim).filter(|u| !u.is_empty()) {
        return Some(url.to_string());
    }
    if let Some(var) = meta.base_url_env
        && let Ok(url) = std::env::var(var)
        && !url.trim().is_empty()
    {
        return Some(url.trim().to_string());
    }
    meta.default_base_url.map(str::to_string)
}

/// A built-in provider with all defaults filled in.
pub(super) fn builtin_instance(name: &str, provider_type: ProviderType) -> ProviderInstance {
    let meta = providers::meta(provider_type);
    ProviderInstance {
        name: name.to_string(),
        provider_type,
        configured: false,
        api_key_env: meta.default_api_key_env.map(str::to_string),
        base_url: resolve_base_url(provider_type, None),
        model: meta.default_model.map(str::to_string),
        output_format: None,
        max_concurrency: meta.default_max_concurrency,
        timeout_ms: DEFAULT_TIMEOUT_MS,
        chars_per_second: None,
        words_per_minute: None,
        settings: Map::new(),
        command: None,
        stdin: None,
        voices_dir: None,
    }
}

/// Placeholders of `command` / `stdin` templates (DESIGN.md §2.8).
pub const TEMPLATE_PLACEHOLDERS: &[&str] = &[
    "text",
    "text_file",
    "output",
    "voice_id",
    "model",
    "language",
];

/// `{name}` sequences in a template whose name is a plain identifier.
fn placeholders(template: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        let name = &after[..close];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            out.push(name);
        }
        rest = &after[close + 1..];
    }
    out
}

/// Warn about `{…}` sequences that look like misspelled placeholders (others pass through literally).
fn check_template(diags: &mut Vec<Diagnostic>, template: &str, path: &str) {
    for name in placeholders(template) {
        if TEMPLATE_PLACEHOLDERS.contains(&name) {
            continue;
        }
        if let Some(suggestion) = check::suggest(name, TEMPLATE_PLACEHOLDERS.iter().copied()) {
            diags.push(
                Diagnostic::warning(
                    codes::INVALID_VALUE,
                    format!(
                        "unknown placeholder `{{{name}}}`; it is passed to the program literally"
                    ),
                )
                .with_path(path)
                .with_hint(format!(
                    "Did you mean `{{{suggestion}}}`? Placeholders: {}",
                    TEMPLATE_PLACEHOLDERS
                        .iter()
                        .map(|p| format!("`{{{p}}}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            );
        }
    }
}

fn is_env_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Providers {
    /// Validate the `providers` block.
    pub(super) fn new(diags: &mut Vec<Diagnostic>, specs: &IndexMap<String, ProviderSpec>) -> Self {
        let mut configured = IndexMap::new();
        for (name, spec) in specs {
            let instance = configured_instance(diags, name, spec);
            configured.insert(name.clone(), instance);
        }
        Providers {
            configured,
            builtins: IndexMap::new(),
            checked: HashMap::new(),
        }
    }

    /// Check a provider name written at `path`; `unknown_provider` is reported once per path.
    pub(super) fn check_ref(
        &mut self,
        diags: &mut Vec<Diagnostic>,
        name: &str,
        path: &str,
    ) -> RefStatus {
        if let Some(status) = self.checked.get(path) {
            return status.clone();
        }
        let status = match self.configured.get(name) {
            Some(Some(_)) => RefStatus::Ok,
            Some(None) => RefStatus::Broken,
            None if providers::builtin_type(name).is_some() => RefStatus::Ok,
            None => {
                let candidates: Vec<&str> = self
                    .configured
                    .keys()
                    .map(String::as_str)
                    .chain(providers::builtin_names().iter().copied())
                    .collect();
                let builtins = diagnostics::list_names(ProviderType::NAMES.iter().copied(), 8);
                let configured = if self.configured.is_empty() {
                    "no providers are configured".to_string()
                } else {
                    format!(
                        "configured: {}",
                        diagnostics::list_names(self.configured.keys().map(String::as_str), 10)
                    )
                };
                let hint = match check::suggest(name, candidates) {
                    Some(s) => format!("Did you mean `{s}`? ({configured}; built-in: {builtins})"),
                    None => format!(
                        "Use a built-in provider ({builtins}) or add an entry under `providers:` with a `type` \
                         ({configured})"
                    ),
                };
                diags.push(
                    Diagnostic::error(
                        codes::UNKNOWN_PROVIDER,
                        format!("unknown provider `{name}`"),
                    )
                    .with_path(path)
                    .with_hint(hint),
                );
                RefStatus::Unknown
            }
        };
        self.checked.insert(path.to_string(), status.clone());
        status
    }

    /// The instance for a valid provider name, instantiating built-ins on first use.
    fn instance(&mut self, name: &str) -> Option<ProviderInstance> {
        if let Some(entry) = self.configured.get(name) {
            return entry.clone();
        }
        let provider_type = providers::builtin_type(name)?;
        Some(
            self.builtins
                .entry(name.to_string())
                .or_insert_with(|| builtin_instance(name, provider_type))
                .clone(),
        )
    }

    /// Configured instances (document order) followed by the built-ins used by cues (first-use order).
    pub(super) fn into_program_providers(self) -> IndexMap<String, ProviderInstance> {
        let mut out: IndexMap<String, ProviderInstance> = self
            .configured
            .into_iter()
            .filter_map(|(name, inst)| inst.map(|i| (name, i)))
            .collect();
        for (name, inst) in self.builtins {
            out.entry(name).or_insert(inst);
        }
        out
    }

    /// Resolve the voice of one cue from its layers (lowest priority first).
    pub(super) fn resolve_voice(
        &mut self,
        diags: &mut Vec<Diagnostic>,
        layers: &[Layer<'_>],
    ) -> VoiceOutcome {
        let Some((provider, provider_path)) = layers.iter().rev().find_map(|l| l.provider.clone())
        else {
            return VoiceOutcome::MissingProvider;
        };
        if self.check_ref(diags, provider, &provider_path) != RefStatus::Ok {
            return VoiceOutcome::Unusable;
        }
        let Some(instance) = self.instance(provider) else {
            return VoiceOutcome::Unusable;
        };
        let meta = providers::meta(instance.provider_type);

        let mut settings = instance.settings.clone();
        for layer in layers {
            if let Some(s) = layer.settings {
                for (k, v) in s {
                    settings.insert(k.clone(), v.clone());
                }
            }
        }
        let model = layers
            .iter()
            .rev()
            .find_map(|l| l.model)
            .map(str::to_string)
            .or_else(|| instance.model.clone());
        let language = layers
            .iter()
            .rev()
            .find_map(|l| l.language)
            .map(str::to_string);
        let explicit_voice = layers
            .iter()
            .rev()
            .find_map(|l| l.voice_id)
            .map(str::to_string);
        let (voice_id, defaulted) = match (explicit_voice, meta.default_voice) {
            (Some(v), _) => (Some(v), None),
            (None, Some(default)) => (Some(default.to_string()), Some(default)),
            (None, None) => {
                return VoiceOutcome::MissingVoice {
                    provider: provider.to_string(),
                };
            }
        };
        let voice = VoiceIr {
            provider: provider.to_string(),
            provider_type: instance.provider_type,
            voice_id,
            model,
            language,
            settings,
        };
        match defaulted {
            Some(default) => VoiceOutcome::DefaultVoice { voice, default },
            None => VoiceOutcome::Ok(voice),
        }
    }
}

/// Result of [`Providers::resolve_voice`].
pub(super) enum VoiceOutcome {
    Ok(VoiceIr),
    /// Resolved with the provider's default voice (info `default_voice`).
    DefaultVoice {
        voice: VoiceIr,
        default: &'static str,
    },
    /// No layer sets a provider (`missing_provider`).
    MissingProvider,
    /// The provider needs an explicit voice (`missing_voice`).
    MissingVoice {
        provider: String,
    },
    /// The provider reference is broken; already reported.
    Unusable,
}

/// One layer of voice settings (DESIGN.md §2.7).
#[derive(Debug, Clone, Default)]
pub(super) struct Layer<'a> {
    /// Provider name and the path where it is written.
    pub provider: Option<(&'a str, String)>,
    pub voice_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub language: Option<&'a str>,
    pub settings: Option<&'a Map<String, Value>>,
}

fn non_empty(s: Option<&String>) -> Option<&str> {
    s.map(String::as_str).filter(|s| !s.trim().is_empty())
}

impl<'a> Layer<'a> {
    pub(super) fn from_voice_spec(spec: &'a VoiceSpec, path: &str) -> Self {
        Layer {
            provider: non_empty(spec.provider.as_ref()).map(|p| (p, format!("{path}.provider"))),
            voice_id: non_empty(spec.voice_id.as_ref()),
            model: non_empty(spec.model.as_ref()),
            language: non_empty(spec.language.as_ref()),
            settings: spec.settings.as_ref(),
        }
    }

    pub(super) fn from_track(track: &'a Track, path: &str) -> Self {
        Layer {
            provider: non_empty(track.provider.as_ref()).map(|p| (p, format!("{path}.provider"))),
            voice_id: non_empty(track.voice_id.as_ref()),
            model: non_empty(track.model.as_ref()),
            language: non_empty(track.language.as_ref()),
            settings: track.settings.as_ref(),
        }
    }

    pub(super) fn from_cue(cue: &'a SequenceItem, path: &str) -> Self {
        Layer {
            provider: non_empty(cue.provider.as_ref()).map(|p| (p, format!("{path}.provider"))),
            voice_id: non_empty(cue.voice_id.as_ref()),
            model: non_empty(cue.model.as_ref()),
            language: non_empty(cue.language.as_ref()),
            settings: cue.settings.as_ref(),
        }
    }
}

/// The object a `settings` map belongs to; the voice fields available next to it differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsOwner {
    /// A cue or a speech track: every voice field and `speaker`.
    CueOrTrack,
    /// `defaults`: every voice field, no `speaker`.
    Defaults,
    /// A `speakers` entry: every voice field; the entry itself is the speaker.
    Speaker,
    /// A `providers` entry: `model` only.
    Provider,
}

/// The voice fields set next to a `settings` map (for "already set" hints).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct VoiceFields<'a> {
    pub provider: Option<&'a str>,
    pub voice_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub language: Option<&'a str>,
    pub speaker: Option<&'a str>,
}

impl<'a> VoiceFields<'a> {
    pub(super) fn of_voice_spec(spec: &'a VoiceSpec) -> Self {
        VoiceFields {
            provider: non_empty(spec.provider.as_ref()),
            voice_id: non_empty(spec.voice_id.as_ref()),
            model: non_empty(spec.model.as_ref()),
            language: non_empty(spec.language.as_ref()),
            speaker: None,
        }
    }

    pub(super) fn of_track(track: &'a Track) -> Self {
        VoiceFields {
            provider: non_empty(track.provider.as_ref()),
            voice_id: non_empty(track.voice_id.as_ref()),
            model: non_empty(track.model.as_ref()),
            language: non_empty(track.language.as_ref()),
            speaker: non_empty(track.speaker.as_ref()),
        }
    }

    pub(super) fn of_cue(cue: &'a SequenceItem) -> Self {
        VoiceFields {
            provider: non_empty(cue.provider.as_ref()),
            voice_id: non_empty(cue.voice_id.as_ref()),
            model: non_empty(cue.model.as_ref()),
            language: non_empty(cue.language.as_ref()),
            speaker: non_empty(cue.speaker.as_ref()),
        }
    }

    fn of_provider(spec: &'a ProviderSpec) -> Self {
        VoiceFields {
            model: non_empty(spec.model.as_ref()),
            ..VoiceFields::default()
        }
    }

    fn get(&self, field: &str) -> Option<&'a str> {
        match field {
            "provider" => self.provider,
            "voice_id" => self.voice_id,
            "model" => self.model,
            "language" => self.language,
            "speaker" => self.speaker,
            _ => None,
        }
    }
}

/// The voice field a key inside `settings` belongs to, with the noun used in messages; None for keys that
/// may be provider settings. `speaker` is only flagged when it is not an integer (piper reads a numeric
/// speaker id from `settings.speaker`).
fn voice_level_key(key: &str, value: &Value) -> Option<(&'static str, &'static str)> {
    let normalized = if key.chars().any(|c| c.is_ascii_lowercase()) {
        diagnostics::normalize_key(key)
    } else {
        key.to_ascii_lowercase()
    };
    match normalized.as_str() {
        "voice" | "voice_id" | "voiceid" | "voice_name" => Some(("voice_id", "voice")),
        "model" | "model_id" => Some(("model", "model")),
        "provider" => Some(("provider", "provider")),
        "language" => Some(("language", "language")),
        "speaker" => {
            let numeric = value.as_u64().is_some()
                || value.as_str().is_some_and(|s| {
                    !s.trim().is_empty() && s.trim().chars().all(|c| c.is_ascii_digit())
                });
            (!numeric).then_some(("speaker", "speaker"))
        }
        _ => None,
    }
}

/// `value` written as a YAML scalar for a hint (`Karen`, `"Karen (Enhanced)"`); `...` for non-scalars.
fn hint_scalar(value: &Value) -> String {
    match value {
        Value::String(s) => {
            let plain = s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
                && !matches!(
                    s.to_ascii_lowercase().as_str(),
                    "true" | "false" | "null" | "yes" | "no" | "on" | "off"
                );
            if plain {
                s.clone()
            } else {
                serde_json::to_string(s).unwrap_or_else(|_| "...".to_string())
            }
        }
        Value::Number(n) => format!("\"{n}\""),
        _ => "...".to_string(),
    }
}

/// `field_not_applicable` (warning) for voice-level keys written inside `settings` (`settings: {voice:
/// nova}`): providers only read their own settings there, so the key would be ignored and the cue would
/// use another voice, model, provider, language or speaker than intended.
pub(super) fn check_settings_keys(
    diags: &mut Vec<Diagnostic>,
    settings: Option<&Map<String, Value>>,
    base: &str,
    owner: SettingsOwner,
    fields: VoiceFields<'_>,
) {
    let Some(settings) = settings else { return };
    for (key, value) in settings {
        let Some((field, noun)) = voice_level_key(key, value) else {
            continue;
        };
        let written = format!("{field}: {}", hint_scalar(value));
        let available = match owner {
            SettingsOwner::CueOrTrack => true,
            SettingsOwner::Defaults | SettingsOwner::Speaker => field != "speaker",
            SettingsOwner::Provider => field == "model",
        };
        let hint = if let Some(existing) = fields.get(field).filter(|_| available) {
            format!(
                "Remove `settings.{key}`; `{field}: {}` is already set next to it",
                hint_scalar(&Value::String(existing.to_string()))
            )
        } else if available {
            format!("Move it out of `settings`: `{written}`")
        } else {
            match (owner, field) {
                (SettingsOwner::Speaker, "speaker") => {
                    let name = base.strip_prefix("speakers.").unwrap_or(base);
                    format!(
                        "Remove `settings.{key}`; this entry already is a speaker: cues and speech tracks use it \
                         with `speaker: {name}`"
                    )
                }
                (_, "speaker") => format!(
                    "Remove `settings.{key}` and set `{written}` on the cues or speech tracks that should use \
                     that speaker"
                ),
                (_, "provider") => format!(
                    "Remove `settings.{key}`; this entry already is a provider instance (its `type:` selects \
                     the provider)"
                ),
                _ => format!(
                    "Remove `settings.{key}` and set `{written}` on the cues, tracks, speakers or `defaults` that \
                     use this provider"
                ),
            }
        };
        let extra = if field == "speaker" {
            " (piper reads only a numeric speaker id there)"
        } else {
            ""
        };
        diags.push(
            Diagnostic::warning(
                codes::FIELD_NOT_APPLICABLE,
                format!(
                    "`settings.{key}` is not a provider setting{extra}; the {noun} is chosen by `{field}`"
                ),
            )
            .with_path(format!("{base}.settings.{key}"))
            .with_hint(hint),
        );
    }
}

/// Report voice fields that are present but empty (`voice_id: ""`).
pub(super) fn check_empty_voice_fields(
    diags: &mut Vec<Diagnostic>,
    path: &str,
    fields: [(&str, Option<&String>); 4],
) {
    for (name, value) in fields {
        if value.is_some_and(|v| v.trim().is_empty()) {
            diags.push(
                Diagnostic::error(codes::INVALID_VALUE, format!("`{name}` is empty"))
                    .with_path(format!("{path}.{name}"))
                    .with_hint(format!("Set `{name}` to a value or remove the key")),
            );
        }
    }
}

/// Validate one `providers` entry.
fn configured_instance(
    diags: &mut Vec<Diagnostic>,
    name: &str,
    spec: &ProviderSpec,
) -> Option<ProviderInstance> {
    let path = format!("providers.{name}");
    let valid_types = diagnostics::list_names(ProviderType::NAMES.iter().copied(), 8);
    let provider_type = match &spec.kind {
        Some(kind) => check::enum_name(
            diags,
            Some(kind.as_str()),
            &format!("{path}.type"),
            ProviderType::parse,
            ProviderType::NAMES,
            "provider type",
        )?,
        None => match providers::builtin_type(name) {
            Some(t) => t,
            None => {
                diags.push(
                    Diagnostic::error(codes::MISSING_FIELD, format!("provider `{name}` has no `type`"))
                        .with_path(format!("{path}.type"))
                        .with_hint(format!(
                            "Add `type:` with one of {valid_types}; `type` may only be omitted when the instance is \
                             named after a built-in provider (e.g. `providers: {{elevenlabs: {{...}}}}`)"
                        )),
                );
                return None;
            }
        },
    };
    let meta = providers::meta(provider_type);
    let before = diags.iter().filter(|d| d.is_error()).count();

    // Fields that only some provider types use are ignored elsewhere (warnings).
    let only = |diags: &mut Vec<Diagnostic>,
                present: bool,
                field: &str,
                types: &[ProviderType],
                what: &str| {
        if present && !types.contains(&provider_type) {
            diags.push(
                Diagnostic::warning(
                    codes::FIELD_NOT_APPLICABLE,
                    format!("`{field}` does not apply to {provider_type} providers and is ignored"),
                )
                .with_path(format!("{path}.{field}"))
                .with_hint(format!("Remove `{field}`; it is only used by {what}")),
            );
        }
    };
    use ProviderType as P;
    only(
        diags,
        spec.command.is_some(),
        "command",
        &[P::Command, P::Piper],
        "`command` and `piper` providers",
    );
    only(
        diags,
        spec.stdin.is_some(),
        "stdin",
        &[P::Command, P::Piper],
        "`command` and `piper` providers",
    );
    only(
        diags,
        spec.voices_dir.is_some(),
        "voices_dir",
        &[P::Piper],
        "`piper` providers",
    );
    only(
        diags,
        spec.base_url.is_some(),
        "base_url",
        &[P::Elevenlabs, P::Openai, P::Cartesia, P::Kokoro],
        "HTTP providers (elevenlabs, openai, cartesia, kokoro)",
    );
    only(
        diags,
        spec.api_key_env.is_some(),
        "api_key_env",
        &[P::Elevenlabs, P::Openai, P::Cartesia, P::Kokoro],
        "HTTP providers (elevenlabs, openai, cartesia, kokoro)",
    );

    let http = matches!(
        provider_type,
        P::Elevenlabs | P::Openai | P::Cartesia | P::Kokoro
    );
    if let Some(var) = &spec.api_key_env
        && http
        && !secrets::looks_like_secret_value(var)
        && !is_env_var_name(var.trim())
    {
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!("`api_key_env` must be the name of an environment variable, got {var:?}"),
            )
            .with_path(format!("{path}.api_key_env"))
            .with_hint(format!(
                "Use letters, digits and `_` only, e.g. `api_key_env: {}`",
                meta.default_api_key_env.unwrap_or("MY_PROVIDER_API_KEY")
            )),
        );
    }
    if let Some(url) = &spec.base_url
        && http
    {
        let u = url.trim();
        let host = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"));
        if !host.is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/')) {
            diags.push(
                Diagnostic::error(
                    codes::INVALID_VALUE,
                    format!("`base_url` {url:?} is not an http(s) URL"),
                )
                .with_path(format!("{path}.base_url"))
                .with_hint(format!(
                    "Use a full URL such as `{}`",
                    meta.default_base_url.unwrap_or("http://localhost:8880")
                )),
            );
        }
    }
    if spec.max_concurrency == Some(0) {
        diags.push(
            Diagnostic::error(codes::INVALID_VALUE, "`max_concurrency` must be at least 1")
                .with_path(format!("{path}.max_concurrency"))
                .with_hint(format!(
                    "Use a positive number (default for {provider_type}: {})",
                    meta.default_max_concurrency
                )),
        );
    }
    if spec.timeout_ms == Some(0) {
        diags.push(
            Diagnostic::error(codes::INVALID_VALUE, "`timeout_ms` must be at least 1")
                .with_path(format!("{path}.timeout_ms"))
                .with_hint(format!(
                    "Use a positive number of milliseconds (default {DEFAULT_TIMEOUT_MS})"
                )),
        );
    }
    for (field, value) in [
        ("chars_per_second", spec.chars_per_second),
        ("words_per_minute", spec.words_per_minute),
    ] {
        if let Some(v) = value
            && !(v.is_finite() && v > 0.0)
        {
            diags.push(
                Diagnostic::error(
                    codes::INVALID_VALUE,
                    format!("`{field}` must be greater than 0 (got {})", fmt_num(v)),
                )
                .with_path(format!("{path}.{field}"))
                .with_hint(format!(
                    "Typical speech is about {} characters per second (≈ 150 words per minute)",
                    fmt_num(meta.default_chars_per_second)
                )),
            );
        }
    }
    if matches!(provider_type, P::Command | P::Piper) {
        for (i, arg) in spec.command.iter().flatten().enumerate() {
            check_template(diags, arg, &format!("{path}.command[{i}]"));
        }
        if let Some(stdin) = &spec.stdin {
            check_template(diags, stdin, &format!("{path}.stdin"));
        }
    }
    match (&spec.command, provider_type) {
        (None, P::Command) => diags.push(
            Diagnostic::error(codes::MISSING_FIELD, format!("command provider `{name}` has no `command` template"))
                .with_path(format!("{path}.command"))
                .with_hint(
                    "Add an argv template, e.g. `command: [\"my-tts\", \"--voice\", \"{voice_id}\", \"--out\", \
                     \"{output}\"]` (placeholders: {text}, {text_file}, {output}, {voice_id}, {model}, {language})",
                ),
        ),
        (Some(argv), P::Command | P::Piper) if argv.is_empty() || argv[0].trim().is_empty() => diags.push(
            Diagnostic::error(codes::INVALID_VALUE, "`command` must start with the program to run")
                .with_path(format!("{path}.command"))
                .with_hint("Use a non-empty list whose first element is the executable, e.g. `[\"piper\", \"-m\", \"{voice_id}\", \"-f\", \"{output}\"]`"),
        ),
        _ => {}
    }
    check_settings_keys(
        diags,
        spec.settings.as_ref(),
        &path,
        SettingsOwner::Provider,
        VoiceFields::of_provider(spec),
    );

    if diags.iter().filter(|d| d.is_error()).count() > before {
        return None;
    }
    Some(ProviderInstance {
        name: name.to_string(),
        provider_type,
        configured: true,
        api_key_env: spec
            .api_key_env
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| meta.default_api_key_env.map(str::to_string)),
        base_url: resolve_base_url(provider_type, spec.base_url.as_deref()),
        model: spec
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| meta.default_model.map(str::to_string)),
        output_format: spec.output_format.clone(),
        max_concurrency: spec
            .max_concurrency
            .map(|c| c as usize)
            .unwrap_or(meta.default_max_concurrency),
        timeout_ms: spec.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
        chars_per_second: spec.chars_per_second,
        words_per_minute: spec.words_per_minute,
        settings: spec.settings.clone().unwrap_or_default(),
        command: spec.command.clone(),
        stdin: spec.stdin.clone(),
        voices_dir: spec.voices_dir.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_placeholders() {
        assert_eq!(
            placeholders("--out={output} {voice} {\"json\": 1} {}"),
            ["output", "voice"]
        );
        let mut d = Vec::new();
        check_template(&mut d, "{voice}", "p.command[2]");
        check_template(&mut d, "{output}{text_file}{custom_thing}", "p.command[3]");
        assert_eq!(d.len(), 1);
        assert!(d[0].hint.as_deref().unwrap_or("").contains("`{voice_id}`"));
        assert_eq!(d[0].severity, crate::Severity::Warning);
    }

    #[test]
    fn voice_level_settings_keys() {
        use serde_json::json;
        assert_eq!(
            voice_level_key("voice", &json!("nova")),
            Some(("voice_id", "voice"))
        );
        assert_eq!(
            voice_level_key("voiceName", &json!("x")),
            Some(("voice_id", "voice"))
        );
        assert_eq!(
            voice_level_key("VOICE", &json!("x")),
            Some(("voice_id", "voice"))
        );
        assert_eq!(
            voice_level_key("model_id", &json!("m")),
            Some(("model", "model"))
        );
        assert_eq!(
            voice_level_key("speaker", &json!("narrator")),
            Some(("speaker", "speaker"))
        );
        // Provider settings, and piper's numeric speaker id, are not voice-level keys.
        assert_eq!(voice_level_key("speaker", &json!(3)), None);
        assert_eq!(voice_level_key("speaker", &json!("3")), None);
        for key in [
            "speed",
            "stability",
            "language_code",
            "locale",
            "lang_code",
            "instructions",
        ] {
            assert_eq!(voice_level_key(key, &json!(1)), None, "{key}");
        }
        assert_eq!(hint_scalar(&json!("Karen")), "Karen");
        assert_eq!(
            hint_scalar(&json!("Karen (Enhanced)")),
            "\"Karen (Enhanced)\""
        );
        assert_eq!(hint_scalar(&json!("no")), "\"no\"");
        assert_eq!(hint_scalar(&json!(5)), "\"5\"");
        assert_eq!(hint_scalar(&json!({"a": 1})), "...");

        let mut d = Vec::new();
        let settings = json!({"voice": "nova", "speed": 1.1});
        check_settings_keys(
            &mut d,
            settings.as_object(),
            "tracks[0].sequence[1]",
            SettingsOwner::CueOrTrack,
            VoiceFields::default(),
        );
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].path.as_deref(),
            Some("tracks[0].sequence[1].settings.voice")
        );
        assert_eq!(
            d[0].message,
            "`settings.voice` is not a provider setting; the voice is chosen by `voice_id`"
        );
        assert_eq!(
            d[0].hint.as_deref(),
            Some("Move it out of `settings`: `voice_id: nova`")
        );
        assert_eq!(d[0].severity, crate::Severity::Warning);
    }

    #[test]
    fn env_var_names() {
        assert!(is_env_var_name("ELEVENLABS_API_KEY"));
        assert!(is_env_var_name("_X1"));
        assert!(!is_env_var_name("1X"));
        assert!(!is_env_var_name("MY-KEY"));
        assert!(!is_env_var_name(""));
    }

    #[test]
    fn builtin_defaults() {
        let i = builtin_instance("eleven_labs", ProviderType::Elevenlabs);
        assert_eq!(i.name, "eleven_labs");
        assert!(!i.configured);
        assert_eq!(i.api_key_env.as_deref(), Some("ELEVENLABS_API_KEY"));
        assert_eq!(i.base_url.as_deref(), Some("https://api.elevenlabs.io"));
        assert_eq!(i.model.as_deref(), Some("eleven_multilingual_v2"));
        assert_eq!(i.max_concurrency, 2);
        assert_eq!(i.timeout_ms, 120_000);
        let k = builtin_instance("kokoro", ProviderType::Kokoro);
        let expected = std::env::var("KOKORO_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "http://localhost:8880".to_string());
        assert_eq!(k.base_url.as_deref(), Some(expected.as_str()));
        assert_eq!(k.api_key_env, None);
        let m = builtin_instance("mock", ProviderType::Mock);
        assert_eq!(m.base_url, None);
        assert_eq!(m.max_concurrency, 8);
    }
}
