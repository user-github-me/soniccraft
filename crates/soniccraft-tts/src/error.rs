//! The TTS error type.

use std::fmt;

use serde::Serialize;
use soniccraft_core::ir::{ProviderInstance, ProviderType};

/// Broad category of a TTS failure. Serialized in `snake_case`; stable for agents and reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsErrorKind {
    /// Missing, invalid or insufficiently privileged credentials (HTTP 401/403, or no key in the environment).
    Auth,
    /// The provider asked us to slow down (HTTP 429 rate or concurrency limits). Retried automatically.
    RateLimited,
    /// Credits, quota or spending limits are exhausted (HTTP 402, billing 429s). Needs user action.
    QuotaExceeded,
    /// The request was rejected as invalid (HTTP 400/422), or the provider configuration is invalid.
    InvalidRequest,
    /// Unknown voice, model, model file or endpoint (HTTP 404, unknown `say` voice, missing piper model).
    NotFound,
    /// Connection failures, resets and truncated responses or streams.
    Network,
    /// A request or child process exceeded the instance's `timeout_ms`.
    Timeout,
    /// The provider failed: HTTP 5xx, an error event inside a stream, a non-zero exit status, a malformed
    /// response, or no audio.
    Provider,
    /// The returned audio could not be decoded.
    Decode,
    /// The provider cannot run in this environment (wrong OS, binary not installed).
    Unavailable,
    /// A local I/O failure (temporary files).
    Io,
}

impl TtsErrorKind {
    /// The `snake_case` name.
    pub fn as_str(self) -> &'static str {
        match self {
            TtsErrorKind::Auth => "auth",
            TtsErrorKind::RateLimited => "rate_limited",
            TtsErrorKind::QuotaExceeded => "quota_exceeded",
            TtsErrorKind::InvalidRequest => "invalid_request",
            TtsErrorKind::NotFound => "not_found",
            TtsErrorKind::Network => "network",
            TtsErrorKind::Timeout => "timeout",
            TtsErrorKind::Provider => "provider",
            TtsErrorKind::Decode => "decode",
            TtsErrorKind::Unavailable => "unavailable",
            TtsErrorKind::Io => "io",
        }
    }
}

impl fmt::Display for TtsErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SonicCraft's own machine-readable [`TtsError::code`] values. They tell failures apart that share a
/// [`TtsErrorKind`] (codes reported by a provider are passed through unchanged and never collide with these).
pub mod error_codes {
    /// The API key was not sent: the request's host is not an allowed key host ([`crate::key_host_allowed`]).
    /// Kind [`super::TtsErrorKind::Unavailable`].
    pub const KEY_HOST_NOT_ALLOWED: &str = "key_host_not_allowed";
    /// The API key variable holds an unexpanded placeholder such as `${ELEVENLABS_API_KEY}` (an MCP client
    /// passed its config template through); it counts as missing and is never sent. Kind
    /// [`super::TtsErrorKind::Auth`].
    pub const PLACEHOLDER_KEY: &str = "placeholder_key";
    /// No connection could be made to the provider (refused, DNS failure, unreachable host, connect
    /// timeout). Kind [`super::TtsErrorKind::Network`].
    pub const CONNECT_FAILED: &str = "connect_failed";
    /// The cue was never sent (`attempts` is 0): an earlier cue of the same provider instance found its server
    /// unreachable ([`CONNECT_FAILED`]); the hint is that failure's. Kind [`super::TtsErrorKind::Network`].
    pub const NOT_ATTEMPTED: &str = "not_attempted";
    /// The cue's time limit (`timeout_ms`, retries and waits included) ran out. Kind
    /// [`super::TtsErrorKind::Timeout`].
    pub const DEADLINE_EXCEEDED: &str = "deadline_exceeded";
    /// A response (or the output of a program) exceeded a size limit. Kind [`super::TtsErrorKind::Provider`].
    pub const RESPONSE_TOO_LARGE: &str = "response_too_large";
    /// The provider returned far more audio than the cue's text can produce (more than ten minutes plus
    /// twenty times the dry-run estimate), e.g. a wrong endpoint or format decoded as noise. Not checked for
    /// `command` (any program) and `mock`. Kind [`super::TtsErrorKind::Provider`].
    pub const IMPLAUSIBLE_DURATION: &str = "implausible_duration";
    /// The server answered with a redirect; TTS requests never follow redirects. Kind
    /// [`super::TtsErrorKind::InvalidRequest`].
    pub const REDIRECT_REFUSED: &str = "redirect_refused";
    /// A `command` argument would start with `-` (the program would read it as an option). Kind
    /// [`super::TtsErrorKind::InvalidRequest`].
    pub const OPTION_LIKE_ARGUMENT: &str = "option_like_argument";
}

/// A synthesis, voice-listing or credential-check failure, with enough context for an actionable
/// `synthesis_failed` diagnostic. Never contains API keys: the request's key is scrubbed from provider
/// messages (servers sometimes echo it), and URLs lose their `user:password@` part.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtsError {
    /// Category.
    pub kind: TtsErrorKind,
    /// Provider instance name (the key under `providers:`, or the built-in type name).
    pub provider: String,
    /// Provider type.
    pub provider_type: ProviderType,
    /// What went wrong, including the provider's own error message when there is one.
    pub message: String,
    /// HTTP status (or the status code of an in-stream error event).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The provider's machine-readable error code, e.g. `voice_not_found`, or one of SonicCraft's own codes
    /// in [`error_codes`] (e.g. [`error_codes::KEY_HOST_NOT_ALLOWED`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// A concrete fix, e.g. "export ELEVENLABS_API_KEY=…" or "run `soniccraft voices elevenlabs`".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Whether the failure is transient (network errors, 408, 429 rate limits, 500/502/503/504). Retryable
    /// errors returned to callers have already exhausted the automatic retries.
    pub retryable: bool,
    /// Delay the provider asked for (`Retry-After`), in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_secs: Option<f64>,
    /// Attempts made, including the first (1 = no retries; 0 = never sent, see
    /// [`error_codes::NOT_ATTEMPTED`]).
    pub attempts: u32,
}

impl TtsError {
    /// A non-retryable error attributed to `provider`.
    pub fn new(
        kind: TtsErrorKind,
        provider: &ProviderInstance,
        message: impl Into<String>,
    ) -> Self {
        Self::for_provider(kind, provider.name.clone(), provider.provider_type, message)
    }

    /// A non-retryable error attributed to an instance name and type.
    pub fn for_provider(
        kind: TtsErrorKind,
        provider: impl Into<String>,
        provider_type: ProviderType,
        message: impl Into<String>,
    ) -> Self {
        TtsError {
            kind,
            provider: provider.into(),
            provider_type,
            message: message.into(),
            status: None,
            code: None,
            hint: None,
            retryable: false,
            retry_after_secs: None,
            attempts: 1,
        }
    }

    /// Set the HTTP status.
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    /// Set the provider error code (ignored when empty).
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        let code = code.into();
        if !code.is_empty() {
            self.code = Some(code);
        }
        self
    }

    /// Set the hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Mark as transient or permanent.
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Set the provider-requested retry delay.
    pub fn with_retry_after(mut self, secs: Option<f64>) -> Self {
        self.retry_after_secs = secs;
        self
    }

    /// True when [`TtsError::code`] is `code` (e.g. one of the [`error_codes`]).
    pub fn has_code(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }
}

impl fmt::Display for TtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.provider == self.provider_type.as_str() || self.provider.is_empty() {
            write!(f, "{}: {}", self.provider_type, self.message)?;
        } else {
            write!(
                f,
                "{} ({}): {}",
                self.provider, self.provider_type, self.message
            )?;
        }
        match (self.status, &self.code) {
            (Some(status), Some(code)) => write!(f, " (HTTP {status}, {code})")?,
            (Some(status), None) => write!(f, " (HTTP {status})")?,
            (None, Some(code)) => write!(f, " ({code})")?,
            (None, None) => {}
        }
        if self.attempts > 1 {
            write!(f, " after {} attempts", self.attempts)?;
        }
        Ok(())
    }
}

impl std::error::Error for TtsError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_instance_status_code_and_attempts() {
        let mut e = TtsError::for_provider(
            TtsErrorKind::Auth,
            "narrator",
            ProviderType::Elevenlabs,
            "Invalid API key",
        )
        .with_status(401)
        .with_code("invalid_api_key");
        assert_eq!(
            e.to_string(),
            "narrator (elevenlabs): Invalid API key (HTTP 401, invalid_api_key)"
        );
        e.provider = "elevenlabs".into();
        e.attempts = 3;
        e.status = None;
        assert_eq!(
            e.to_string(),
            "elevenlabs: Invalid API key (invalid_api_key) after 3 attempts"
        );
    }

    #[test]
    fn serializes_kind_in_snake_case() {
        let e = TtsError::for_provider(
            TtsErrorKind::QuotaExceeded,
            "openai",
            ProviderType::Openai,
            "out of credits",
        );
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "quota_exceeded");
        assert_eq!(v["provider_type"], "openai");
        assert!(v.get("status").is_none());
    }
}
