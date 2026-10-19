//! Shared HTTP plumbing: client construction, retries with backoff under a per-cue deadline (DESIGN.md
//! §5.2), size-capped bodies, error-envelope parsing and status classification with actionable hints.
//!
//! Hardening: redirects are never followed (a key must not travel to another origin; ElevenLabs'
//! `xi-api-key` is not a header `reqwest` strips on cross-origin hops), every response body is read with a
//! size cap ([`MAX_RESPONSE_BYTES`], [`MAX_JSON_BYTES`], [`MAX_ERROR_BODY_BYTES`]), each logical request
//! has a deadline covering all its attempts and the waits between them, and errors are scrubbed of the
//! request's key and of URL userinfo before they are logged or returned.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue, LOCATION};
use serde_json::Value;
use soniccraft_core::ir::{ProviderInstance, ProviderType};

use crate::defaults;
use crate::error::{TtsError, TtsErrorKind, error_codes};
use crate::{hosts, keys};

const USER_AGENT: &str = concat!("soniccraft/", env!("CARGO_PKG_VERSION"));
/// Longest provider-requested `Retry-After` we wait for; longer requests fail fast with a hint.
const MAX_RETRY_AFTER_SECS: f64 = 60.0;
/// Longest excerpt of a non-JSON error body copied into messages.
const MAX_BODY_EXCERPT: usize = 400;
/// Largest successful response body read into memory: raw audio, or JSON carrying base64 audio.
pub(crate) const MAX_RESPONSE_BYTES: usize = 256 * 1024 * 1024;
/// Largest JSON metadata response (voice lists, credential checks).
pub(crate) const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
/// Bytes of an error body that are read; the rest is ignored (messages keep a short excerpt anyway).
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;
/// Largest SSE stream (every byte, base64 audio and keep-alives included).
pub(crate) const MAX_STREAM_BYTES: usize = 512 * 1024 * 1024;

/// Retry behaviour: `max_retries` retries after the first attempt, waiting `base_delay × 2^(n-1)` (1 s, 2 s,
/// 4 s by default) or the provider's `Retry-After`, whichever is longer. `Retry-After` is scaled by
/// `base_delay / 1 s` so tests can shrink every wait proportionally; production uses 1 s, i.e. exact values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RetryPolicy {
    pub base_delay: Duration,
    pub max_retries: u32,
}

impl RetryPolicy {
    /// Wait before the retry following failed attempt number `attempt` (1-based). `None` = do not retry.
    pub(crate) fn delay(&self, attempt: u32, retry_after_secs: Option<f64>) -> Option<Duration> {
        if attempt > self.max_retries {
            return None;
        }
        let exponent = attempt.saturating_sub(1).min(16) as i32;
        let backoff = self.base_delay.mul_f64(2f64.powi(exponent));
        match retry_after_secs {
            Some(secs) if secs > MAX_RETRY_AFTER_SECS => None,
            Some(secs) if secs.is_finite() && secs > 0.0 => {
                Some(backoff.max(self.base_delay.mul_f64(secs)))
            }
            _ => Some(backoff),
        }
    }
}

/// The time limit of one logical request (a cue's synthesis, a voice-list page, a credential check): every
/// attempt, every wait between attempts and every byte of a streamed response must fit in it. Its length
/// is the instance's `timeout_ms` (default 120 s).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    at: tokio::time::Instant,
    limit: Duration,
}

impl Deadline {
    /// A deadline `limit` from now.
    pub(crate) fn after(limit: Duration) -> Self {
        Deadline {
            at: tokio::time::Instant::now() + limit,
            limit,
        }
    }

    /// A deadline of the instance's `timeout_ms` from now.
    pub(crate) fn for_instance(origin: &ProviderInstance) -> Self {
        Self::after(request_timeout(origin))
    }

    /// Time left (zero once passed).
    pub(crate) fn remaining(&self) -> Duration {
        self.at
            .saturating_duration_since(tokio::time::Instant::now())
    }

    /// The error of a request cut off by the deadline.
    pub(crate) fn exceeded(&self, origin: &ProviderInstance) -> TtsError {
        TtsError::new(
            TtsErrorKind::Timeout,
            origin,
            format!(
                "no complete response within {} ms (`timeout_ms`, retries and waits included)",
                self.limit.as_millis()
            ),
        )
        .with_code(error_codes::DEADLINE_EXCEEDED)
        .with_hint(
            "raise `timeout_ms` on the provider, split long cues, or check that the server is responding",
        )
    }
}

/// Run `op` until it succeeds, fails permanently, retries are exhausted, or `deadline` passes. `op` gets the
/// attempt number (1-based) and the time left (use it as the request timeout); each attempt is also cut off
/// at the deadline, so a server that trickles bytes forever cannot stall a cue. A retry whose wait would not
/// leave time for another attempt is not made. Errors are scrubbed of `secrets` and URL userinfo before they
/// are logged or returned, and carry the number of attempts made.
pub(crate) async fn with_retries<T, F, Fut>(
    origin: &ProviderInstance,
    policy: RetryPolicy,
    deadline: Deadline,
    secrets: &[String],
    mut op: F,
) -> Result<T, TtsError>
where
    F: FnMut(u32, Duration) -> Fut,
    Fut: Future<Output = Result<T, TtsError>>,
{
    let mut attempt = 1u32;
    loop {
        let left = deadline.remaining();
        let result = if left.is_zero() {
            Err(deadline.exceeded(origin))
        } else {
            match tokio::time::timeout(left, op(attempt, left)).await {
                Ok(result) => result,
                Err(_) => Err(deadline.exceeded(origin)),
            }
        };
        let mut error = match result {
            Ok(value) => return Ok(value),
            Err(error) => keys::scrub_error(error, secrets),
        };
        error.attempts = attempt;
        let delay = if error.retryable {
            policy.delay(attempt, error.retry_after_secs)
        } else {
            None
        };
        let Some(delay) = delay.filter(|d| *d < deadline.remaining()) else {
            // The provider's own wait is the most useful hint when it is why no retry was made.
            if error.retryable
                && let Some(secs) = error.retry_after_secs.filter(|s| *s > 0.0)
            {
                if secs > MAX_RETRY_AFTER_SECS {
                    error.hint = Some(format!(
                        "the provider asked to retry after {} (SonicCraft waits at most {MAX_RETRY_AFTER_SECS:.0} s); \
                         try again then",
                        describe_wait(secs)
                    ));
                } else if delay.is_some() {
                    error.hint = Some(format!(
                        "the provider asked to retry after {}, more than is left of the cue's `timeout_ms` ({} ms); \
                         raise `timeout_ms` on the provider or try again later",
                        describe_wait(secs),
                        deadline.limit.as_millis()
                    ));
                }
            }
            return Err(error);
        };
        tracing::debug!(
            provider = %error.provider,
            attempt,
            delay_ms = delay.as_millis() as u64,
            "retrying after transient TTS error: {error}"
        );
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// `45 s`, `12 min` or `33.3 h`.
fn describe_wait(secs: f64) -> String {
    if secs < 120.0 {
        format!("{secs:.0} s")
    } else if secs < 7_200.0 {
        format!("{:.0} min", secs / 60.0)
    } else {
        format!("{:.1} h", secs / 3_600.0)
    }
}

/// Lazily built `reqwest` clients, one per (idle timeout, direct) pair. Loopback URLs bypass proxies so a
/// local Kokoro or test server is never routed through `HTTP(S)_PROXY`. Redirects are never followed.
#[derive(Debug, Default)]
pub(crate) struct HttpClients {
    clients: Mutex<BTreeMap<(u64, bool), reqwest::Client>>,
}

impl HttpClients {
    /// A client for `url` whose connect timeout is `min(timeout, 15 s)` and whose idle read timeout is
    /// `timeout`. Callers add a per-request `.timeout()` (the time left before their [`Deadline`]).
    pub(crate) fn get(
        &self,
        origin: &ProviderInstance,
        url: &str,
        timeout: Duration,
    ) -> Result<reqwest::Client, TtsError> {
        let direct = hosts::is_loopback_url(url);
        let key = (timeout.as_millis() as u64, direct);
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(timeout.min(Duration::from_secs(15)))
            .read_timeout(timeout);
        if direct {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(|e| {
            TtsError::new(
                TtsErrorKind::Unavailable,
                origin,
                format!("cannot initialise the HTTP client: {}", chain(&e)),
            )
        })?;
        clients.insert(key, client.clone());
        Ok(client)
    }
}

/// `base` without trailing slashes.
pub(crate) fn trim_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_owned()
}

/// Percent-encode one URL path segment (voice ids are interpolated into paths).
pub(crate) fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Per-cue time limit of an instance (`timeout_ms`, default 120 s).
pub(crate) fn request_timeout(origin: &ProviderInstance) -> Duration {
    let ms = if origin.timeout_ms == 0 {
        defaults::DEFAULT_TIMEOUT_MS
    } else {
        origin.timeout_ms
    };
    Duration::from_millis(ms)
}

/// A header value marked sensitive (redacted from `Debug` output), or an auth error naming the variable.
pub(crate) fn secret_header(
    origin: &ProviderInstance,
    value: &str,
) -> Result<HeaderValue, TtsError> {
    let mut header = HeaderValue::from_str(value).map_err(|_| {
        let var = keys::api_key_env(origin).unwrap_or_default();
        TtsError::new(
            TtsErrorKind::Auth,
            origin,
            format!("the API key in {var} contains characters not allowed in HTTP headers"),
        )
        .with_hint(format!(
            "re-export {var} without quotes, spaces or newlines"
        ))
    })?;
    header.set_sensitive(true);
    Ok(header)
}

/// Display of an error and all its sources (`reqwest` hides the cause in `Display`).
pub(crate) fn chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut current = error.source();
    while let Some(cause) = current {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        current = cause.source();
    }
    out
}

/// Map a transport-level `reqwest` failure (no HTTP status) to an error. Connection failures (refused, DNS,
/// unreachable, connect timeout) get [`error_codes::CONNECT_FAILED`]; they are retried for remote hosts
/// only: a local server that refuses connections stays down for the seconds a retry ladder lasts.
pub(crate) fn transport_error(
    origin: &ProviderInstance,
    url: &str,
    error: &reqwest::Error,
) -> TtsError {
    let shown = hosts::redact_url(url);
    let detail = hosts::scrub_urls(&chain(error));
    let local = hosts::is_local_url(url);
    let connect = error.is_connect();
    let (kind, retryable) = if error.is_builder() {
        (TtsErrorKind::InvalidRequest, false)
    } else if connect {
        (TtsErrorKind::Network, !local)
    } else if error.is_timeout() {
        (TtsErrorKind::Timeout, true)
    } else {
        (TtsErrorKind::Network, true)
    };
    let hint = match kind {
        TtsErrorKind::InvalidRequest => {
            format!("check the provider's `base_url` (`{shown}` is not a valid URL)")
        }
        TtsErrorKind::Timeout => format!(
            "the request took longer than timeout_ms ({} ms); raise `timeout_ms` on the provider or split long cues",
            request_timeout(origin).as_millis()
        ),
        _ if connect && hosts::is_loopback_url(url) => format!(
            "nothing answered at {shown}; start the local server{}",
            if origin.provider_type == ProviderType::Kokoro {
                " (docker run -p 8880:8880 ghcr.io/remsky/kokoro-fastapi-cpu:latest) or set KOKORO_BASE_URL"
            } else {
                " or fix `base_url`"
            }
        ),
        _ if connect && local => format!(
            "nothing answered at {shown}; check that the server is running and reachable from this machine, or fix `base_url`"
        ),
        _ if local => format!(
            "the server at {shown} closed the connection without a complete response; check its logs, then retry"
        ),
        _ => format!("check your network connection and the provider's `base_url` ({shown})"),
    };
    let mut out = TtsError::new(kind, origin, format!("request failed: {detail}"))
        .with_retryable(retryable)
        .with_hint(hint);
    if connect {
        out = out.with_code(error_codes::CONNECT_FAILED);
    }
    out
}

/// `Retry-After` (delta seconds or an HTTP-date) or `retry-after-ms`, in seconds. An HTTP-date is measured
/// from the response's `Date` header when it has one (immune to clock skew), else from the local clock; a
/// date in the past means "now".
pub(crate) fn retry_after_secs(headers: &HeaderMap) -> Option<f64> {
    let text = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    let valid = |s: f64| (s.is_finite() && s >= 0.0).then_some(s);
    if let Some(ms) = text("retry-after-ms").and_then(|s| s.parse::<f64>().ok()) {
        return valid(ms / 1000.0);
    }
    let value = text("retry-after")?;
    if let Ok(secs) = value.parse::<f64>() {
        return valid(secs);
    }
    let at = parse_http_date(value)?;
    let now = text("date")
        .and_then(parse_http_date)
        .unwrap_or_else(SystemTime::now);
    Some(at.duration_since(now).map_or(0.0, |d| d.as_secs_f64()))
}

fn month_number(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let lower = name.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|m| *m == lower)
        .map(|i| i as u32 + 1)
}

/// Days from 1970-01-01 to the civil date `year-month-day` (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parse an HTTP-date (RFC 9110 §5.6.7) in all three forms recipients must accept: IMF-fixdate
/// `Sun, 06 Nov 1994 08:49:37 GMT`, the obsolete RFC 850 form `Sunday, 06-Nov-94 08:49:37 GMT`, and
/// asctime `Sun Nov  6 08:49:37 1994`.
pub(crate) fn parse_http_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let (day, month, year, time): (u32, u32, i64, &str) = match s.split_once(',') {
        Some((_, rest)) => match rest.split_whitespace().collect::<Vec<_>>().as_slice() {
            [day, month, year, time, zone] if zone.eq_ignore_ascii_case("GMT") => (
                day.parse().ok()?,
                month_number(month)?,
                year.parse().ok()?,
                time,
            ),
            [date, time, zone] if zone.eq_ignore_ascii_case("GMT") => {
                let mut parts = date.split('-');
                let day = parts.next()?.parse().ok()?;
                let month = month_number(parts.next()?)?;
                let short: i64 = parts.next()?.parse().ok()?;
                if parts.next().is_some() {
                    return None;
                }
                let year = match short {
                    0..=69 => 2000 + short,
                    70..=99 => 1900 + short,
                    _ => short,
                };
                (day, month, year, time)
            }
            _ => return None,
        },
        None => match s.split_whitespace().collect::<Vec<_>>().as_slice() {
            [_, month, day, time, year] => (
                day.parse().ok()?,
                month_number(month)?,
                year.parse().ok()?,
                time,
            ),
            _ => return None,
        },
    };
    let mut hms = time.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, sec) = (hms.next()??, hms.next()??, hms.next()??);
    if hms.next().is_some() || h > 23 || m > 59 || sec > 60 || !(1..=31).contains(&day) {
        return None;
    }
    let days = u64::try_from(days_from_civil(year, month, day)).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(days * 86_400 + h * 3_600 + m * 60 + sec))
}

/// What a provider error body says, whatever its envelope.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ErrorInfo {
    /// Human-readable message.
    pub message: Option<String>,
    /// Machine-readable identifiers found in the body (`code`, `error_code`, legacy `status`, `type`, …),
    /// lower-cased, most specific first.
    pub codes: Vec<String>,
}

impl ErrorInfo {
    pub(crate) fn has_code(&self, code: &str) -> bool {
        self.codes.iter().any(|c| c == code)
    }

    pub(crate) fn mentions(&self, needle: &str) -> bool {
        self.codes.iter().any(|c| c.contains(needle))
            || self
                .message
                .as_deref()
                .is_some_and(|m| m.to_ascii_lowercase().contains(needle))
    }
}

/// Parse a provider error body. Understands `{"error": {message, code, type}}` (OpenAI), `{"detail": {...}}`,
/// `{"detail": [...]}` and `{"detail": "..."}` (ElevenLabs, FastAPI servers such as Kokoro), `{"error_code",
/// "message", "title"}` (Cartesia), and plain text (used verbatim, truncated) — regardless of `Content-Type`.
pub(crate) fn parse_error_body(body: &str) -> ErrorInfo {
    let trimmed = body.trim();
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => {
            let mut info = ErrorInfo::default();
            collect_error(&value, &mut info, 0);
            if info.message.is_none() && info.codes.is_empty() {
                info.message = Some(excerpt(trimmed));
            }
            info
        }
        Err(_) if trimmed.is_empty() => ErrorInfo::default(),
        Err(_) => ErrorInfo {
            message: Some(excerpt(trimmed)),
            codes: Vec::new(),
        },
    }
}

fn excerpt(text: &str) -> String {
    let collapsed = crate::text::collapse_whitespace(text);
    if collapsed.chars().count() <= MAX_BODY_EXCERPT {
        collapsed
    } else {
        let cut: String = collapsed.chars().take(MAX_BODY_EXCERPT).collect();
        format!("{cut}…")
    }
}

fn push_code(info: &mut ErrorInfo, value: Option<&Value>) {
    if let Some(code) = value
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
        && !code.is_empty()
        && code.len() <= 80
        && !code.contains(' ')
        && !info.codes.contains(&code)
    {
        info.codes.push(code);
    }
}

fn collect_error(value: &Value, info: &mut ErrorInfo, depth: usize) {
    if depth > 4 {
        return;
    }
    match value {
        Value::Object(obj) => {
            // Most specific identifiers first: `code`/`error_code`, Kokoro's `error` id, the legacy ElevenLabs
            // `status` string, then the generic `type`.
            push_code(info, obj.get("code"));
            push_code(info, obj.get("error_code"));
            if obj
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.trim().contains(' '))
            {
                push_code(info, obj.get("error"));
            }
            push_code(info, obj.get("status"));
            push_code(info, obj.get("type"));
            if info.message.is_none() {
                for key in ["message", "msg"] {
                    if let Some(m) = obj
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|m| !m.trim().is_empty())
                    {
                        info.message = Some(m.trim().to_owned());
                        break;
                    }
                }
            }
            for key in ["error", "detail"] {
                match obj.get(key) {
                    // `{"detail": {"error": "invalid_model", ...}}` (Kokoro): an identifier, collected above.
                    Some(Value::String(s)) if key == "error" && !s.trim().contains(' ') => {}
                    Some(Value::String(s)) => {
                        if info.message.is_none() && !s.trim().is_empty() {
                            info.message = Some(s.trim().to_owned());
                        }
                    }
                    Some(nested @ (Value::Object(_) | Value::Array(_))) => {
                        collect_error(nested, info, depth + 1)
                    }
                    _ => {}
                }
            }
            if info.message.is_none()
                && let Some(title) = obj.get("title").and_then(Value::as_str)
            {
                info.message = Some(title.trim().to_owned());
            }
        }
        // FastAPI/pydantic validation errors: [{"loc": [...], "msg": "...", "type": "..."}].
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items.iter().take(5) {
                let Some(obj) = item.as_object() else {
                    continue;
                };
                let msg = obj
                    .get("msg")
                    .or_else(|| obj.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("invalid");
                let loc = obj.get("loc").and_then(Value::as_array).map(|l| {
                    l.iter()
                        .map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_owned))
                        .collect::<Vec<_>>()
                        .join(".")
                });
                parts.push(match loc {
                    Some(loc) if !loc.is_empty() => format!("{loc}: {msg}"),
                    _ => msg.to_owned(),
                });
                push_code(info, obj.get("type"));
            }
            if info.message.is_none() && !parts.is_empty() {
                info.message = Some(parts.join("; "));
            }
        }
        _ => {}
    }
}

/// Codes meaning "out of credits / quota / spending limit" (never retried).
fn is_quota(info: &ErrorInfo) -> bool {
    info.codes.iter().any(|c| {
        matches!(
            c.as_str(),
            "credit_balance_exhausted"
                | "insufficient_quota"
                | "quota_exceeded"
                | "insufficient_credits"
                | "payment_required"
        ) || (c.ends_with("_limit_exceeded") && (c.contains("spend") || c.contains("usage")))
    })
}

/// Turn a non-2xx status plus parsed body into a [`TtsError`] with kind, retryability and a hint
/// (DESIGN.md §5.2): 408/429/500/502/503/504 are retryable, every other status fails fast.
pub(crate) fn status_error(
    origin: &ProviderInstance,
    status: u16,
    info: &ErrorInfo,
    retry_after: Option<f64>,
) -> TtsError {
    if let Some(redirect) = redirect_error(origin, status, None) {
        return redirect;
    }
    let t = origin.provider_type;
    let name = defaults::display_name(t);
    let (kind, retryable) = match status {
        401 | 403 => (TtsErrorKind::Auth, false),
        402 => (TtsErrorKind::QuotaExceeded, false),
        404 => (TtsErrorKind::NotFound, false),
        408 => (TtsErrorKind::Timeout, true),
        429 if is_quota(info) => (TtsErrorKind::QuotaExceeded, false),
        429 => (TtsErrorKind::RateLimited, true),
        500 | 502 | 503 | 504 => (TtsErrorKind::Provider, true),
        s if s >= 500 => (TtsErrorKind::Provider, false),
        _ => (TtsErrorKind::InvalidRequest, false),
    };
    // Unknown voices are reported as 400 by some providers.
    let kind = if kind == TtsErrorKind::InvalidRequest
        && info.codes.iter().any(|c| {
            matches!(
                c.as_str(),
                "voice_not_found" | "invalid_voice_id" | "model_not_found"
            )
        }) {
        TtsErrorKind::NotFound
    } else {
        kind
    };
    let message = info.message.clone().unwrap_or_else(|| {
        reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("request failed")
            .to_owned()
    });
    let voice_hint = format!("run `soniccraft voices {t}` to list valid voice ids for `voice_id`");
    let hint = match kind {
        TtsErrorKind::Auth => {
            let var =
                keys::api_key_env(origin).unwrap_or_else(|| "the API key variable".to_owned());
            if info.mentions("permission")
                || info.mentions("subscription")
                || info.mentions("feature")
            {
                format!(
                    "your {name} key or plan lacks access to this feature, model or voice; check the key's permissions and your plan"
                )
            } else {
                format!("check that {var} holds a valid {name} API key")
            }
        }
        TtsErrorKind::QuotaExceeded => {
            format!(
                "your {name} account is out of credits or over its spending limit; top it up, then retry"
            )
        }
        TtsErrorKind::NotFound if info.mentions("voice") => voice_hint,
        TtsErrorKind::NotFound if info.mentions("model") => {
            format!("check the `model` id; it is not available on this {name} account or endpoint")
        }
        TtsErrorKind::NotFound => {
            format!("check `voice_id`, `model` and `base_url` ({voice_hint})")
        }
        TtsErrorKind::RateLimited => {
            "lower `max_concurrency` on the provider (or `--jobs`), then retry".to_owned()
        }
        TtsErrorKind::Timeout => {
            "the provider timed out; retry, or raise `timeout_ms` on the provider".to_owned()
        }
        TtsErrorKind::Provider => format!("{name} reported a server error; retry later"),
        _ if info.mentions("voice") => voice_hint,
        _ => format!(
            "{name} rejected the request; fix the field named in the message (see the provider's API docs)"
        ),
    };
    let mut error = TtsError::new(kind, origin, message)
        .with_status(status)
        .with_hint(hint)
        .with_retryable(retryable)
        .with_retry_after(retry_after);
    if let Some(code) = info.codes.first() {
        error = error.with_code(code.clone());
    }
    error
}

/// The error of a 3xx answer (`None` for other statuses): TTS requests never follow redirects.
pub(crate) fn redirect_error(
    origin: &ProviderInstance,
    status: u16,
    location: Option<&str>,
) -> Option<TtsError> {
    if !(300..400).contains(&status) {
        return None;
    }
    let target = location
        .map(|l| format!(" to {}", hosts::redact_url(l)))
        .unwrap_or_default();
    Some(
        TtsError::new(
            TtsErrorKind::InvalidRequest,
            origin,
            format!(
                "the server answered with a redirect{target}; TTS requests never follow redirects, so an API key \
                 cannot travel to another host"
            ),
        )
        .with_status(status)
        .with_code(error_codes::REDIRECT_REFUSED)
        .with_hint(
            "set the provider's `base_url` to the final address (the one the server redirects to) if you trust it",
        ),
    )
}

/// A non-success response, read with a size cap.
#[derive(Debug)]
pub(crate) struct ErrorResponse {
    pub status: u16,
    pub retry_after: Option<f64>,
    pub location: Option<String>,
    /// The first [`MAX_ERROR_BODY_BYTES`] of the body (empty for redirects).
    pub body: String,
}

/// Read a non-success response: status, `Retry-After`, `Location` and the start of the body.
pub(crate) async fn read_error(mut response: reqwest::Response) -> ErrorResponse {
    let status = response.status().as_u16();
    let retry_after = retry_after_secs(response.headers());
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    if !(300..400).contains(&status) {
        while bytes.len() < MAX_ERROR_BODY_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    let take = chunk.len().min(MAX_ERROR_BODY_BYTES - bytes.len());
                    bytes.extend_from_slice(&chunk[..take]);
                }
                Ok(None) | Err(_) => break,
            }
        }
    }
    ErrorResponse {
        status,
        retry_after,
        location,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Read a non-success response (body included, capped) into a [`TtsError`].
pub(crate) async fn response_error(
    origin: &ProviderInstance,
    response: reqwest::Response,
) -> TtsError {
    let e = read_error(response).await;
    redirect_error(origin, e.status, e.location.as_deref()).unwrap_or_else(|| {
        status_error(origin, e.status, &parse_error_body(&e.body), e.retry_after)
    })
}

/// The error of a response (or program output) larger than `limit` bytes.
pub(crate) fn too_large(origin: &ProviderInstance, what: &str, limit: usize) -> TtsError {
    TtsError::new(
        TtsErrorKind::Provider,
        origin,
        format!("{what} is larger than the {} MiB limit", limit >> 20),
    )
    .with_code(error_codes::RESPONSE_TOO_LARGE)
    .with_hint(
        "check `base_url` (a proxy, captive portal or another server may be answering), and split very long cues \
         into shorter ones",
    )
}

/// The body of a successful response, refusing more than `limit` bytes (the declared `Content-Length`
/// fails fast; a body without one is cut off as soon as it passes the limit).
pub(crate) async fn read_body(
    origin: &ProviderInstance,
    url: &str,
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, TtsError> {
    let declared = response.content_length();
    if declared.is_some_and(|len| len > limit as u64) {
        return Err(too_large(origin, "the response", limit));
    }
    let initial = declared.map_or(0, |len| len as usize).min(16 * 1024 * 1024);
    let mut body = Vec::with_capacity(initial);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| transport_error(origin, url, &e))?
    {
        if body.len() + chunk.len() > limit {
            return Err(too_large(origin, "the response", limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A successful JSON response, capped at [`MAX_JSON_BYTES`].
pub(crate) async fn read_json(
    origin: &ProviderInstance,
    url: &str,
    response: reqwest::Response,
) -> Result<Value, TtsError> {
    let bytes = read_body(origin, url, response, MAX_JSON_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(|e| {
        TtsError::new(
            TtsErrorKind::Provider,
            origin,
            format!("unexpected response from {}: {e}", hosts::redact_url(url)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::EnvSource;
    use crate::test_util::instance;

    #[test]
    fn retry_delays_follow_backoff_and_retry_after() {
        let p = RetryPolicy {
            base_delay: Duration::from_secs(1),
            max_retries: 3,
        };
        assert_eq!(p.delay(1, None), Some(Duration::from_secs(1)));
        assert_eq!(p.delay(2, None), Some(Duration::from_secs(2)));
        assert_eq!(p.delay(3, None), Some(Duration::from_secs(4)));
        assert_eq!(p.delay(4, None), None);
        assert_eq!(p.delay(1, Some(3.0)), Some(Duration::from_secs(3)));
        assert_eq!(p.delay(3, Some(1.0)), Some(Duration::from_secs(4)));
        assert_eq!(p.delay(1, Some(120.0)), None);
        let fast = RetryPolicy {
            base_delay: Duration::from_millis(10),
            max_retries: 3,
        };
        assert_eq!(fast.delay(1, Some(2.0)), Some(Duration::from_millis(20)));
    }

    #[test]
    fn http_dates_in_all_three_forms() {
        let expected = UNIX_EPOCH + Duration::from_secs(784_111_777); // 1994-11-06 08:49:37 UTC
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(expected)
        );
        assert_eq!(
            parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(expected)
        );
        assert_eq!(parse_http_date("Sun Nov  6 08:49:37 1994"), Some(expected));
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2026 07:28:00 GMT"),
            Some(UNIX_EPOCH + Duration::from_secs(1_792_567_680))
        );
        for bad in [
            "",
            "soon",
            "Sun, 06 Nov 1994 08:49:37 CET",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Foo 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:49 GMT",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn retry_after_forms() {
        let headers = |pairs: &[(&'static str, &str)]| {
            let mut h = HeaderMap::new();
            for (k, v) in pairs {
                h.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            h
        };
        assert_eq!(
            retry_after_secs(&headers(&[("retry-after", "4")])),
            Some(4.0)
        );
        assert_eq!(
            retry_after_secs(&headers(&[
                ("retry-after-ms", "1500"),
                ("retry-after", "9")
            ])),
            Some(1.5)
        );
        assert_eq!(
            retry_after_secs(&headers(&[
                ("date", "Wed, 21 Oct 2026 07:28:00 GMT"),
                ("retry-after", "Wed, 21 Oct 2026 07:28:03 GMT"),
            ])),
            Some(3.0)
        );
        assert_eq!(
            retry_after_secs(&headers(&[
                ("date", "Wed, 21 Oct 2026 07:28:09 GMT"),
                ("retry-after", "Wed, 21 Oct 2026 07:28:03 GMT"),
            ])),
            Some(0.0),
            "a date in the past means now"
        );
        // Without a Date header the local clock is the reference: a date years ahead is far beyond the
        // 60 s the client waits, so it fails fast instead of falling back to plain backoff.
        let far = retry_after_secs(&headers(&[(
            "retry-after",
            "Fri, 01 Jan 2100 00:00:00 GMT",
        )]));
        assert!(far.is_some_and(|s| s > MAX_RETRY_AFTER_SECS), "{far:?}");
        assert_eq!(retry_after_secs(&headers(&[("retry-after", "-3")])), None);
        assert_eq!(
            retry_after_secs(&headers(&[("retry-after", "later")])),
            None
        );
        assert_eq!(retry_after_secs(&HeaderMap::new()), None);
    }

    #[test]
    fn parses_elevenlabs_envelopes() {
        let obj = parse_error_body(
            r#"{"detail":{"type":"authentication_error","code":"unauthorized","message":"Invalid API key","status":"invalid_api_key","request_id":"x"}}"#,
        );
        assert_eq!(obj.message.as_deref(), Some("Invalid API key"));
        assert_eq!(
            obj.codes,
            ["unauthorized", "invalid_api_key", "authentication_error"]
        );
        let arr = parse_error_body(
            r#"{"detail":[{"loc":["body","text"],"msg":"field required","type":"missing"}]}"#,
        );
        assert_eq!(arr.message.as_deref(), Some("body.text: field required"));
        assert_eq!(arr.codes, ["missing"]);
        let s = parse_error_body(r#"{"detail":"Not Found"}"#);
        assert_eq!(s.message.as_deref(), Some("Not Found"));
        assert!(s.codes.is_empty());
    }

    #[test]
    fn parses_openai_cartesia_kokoro_and_text() {
        let o = parse_error_body(
            r#"{"error":{"message":"Incorrect API key provided: sk-inval*******-000.","type":"invalid_request_error","code":"invalid_api_key","param":null},"status":401}"#,
        );
        assert_eq!(
            o.message.as_deref(),
            Some("Incorrect API key provided: sk-inval*******-000.")
        );
        assert_eq!(o.codes, ["invalid_api_key", "invalid_request_error"]);
        let c = parse_error_body(
            r#"{"error_code":null,"message":"Invalid API key.","title":"Unauthorized","request_id":"r"}"#,
        );
        assert_eq!(c.message.as_deref(), Some("Invalid API key."));
        assert!(c.codes.is_empty());
        let k = parse_error_body(
            r#"{"detail":{"error":"invalid_model","message":"Unsupported model: x","type":"invalid_request_error"}}"#,
        );
        assert_eq!(k.message.as_deref(), Some("Unsupported model: x"));
        assert_eq!(k.codes, ["invalid_model", "invalid_request_error"]);
        let t = parse_error_body("Invalid Cartesia-Version header: required");
        assert_eq!(
            t.message.as_deref(),
            Some("Invalid Cartesia-Version header: required")
        );
        assert_eq!(parse_error_body(""), ErrorInfo::default());
        let long = parse_error_body(&"x ".repeat(1000));
        assert!(long.message.unwrap().ends_with('…'));
    }

    #[test]
    fn classifies_statuses() {
        let o = instance(ProviderType::Elevenlabs);
        let auth = status_error(
            &o,
            401,
            &parse_error_body(r#"{"detail":{"message":"bad"}}"#),
            None,
        );
        assert_eq!((auth.kind, auth.retryable), (TtsErrorKind::Auth, false));
        assert!(auth.hint.as_deref().unwrap().contains("ELEVENLABS_API_KEY"));
        let rl = status_error(
            &o,
            429,
            &parse_error_body(r#"{"detail":{"code":"concurrent_limit_exceeded"}}"#),
            Some(2.0),
        );
        assert_eq!(
            (rl.kind, rl.retryable, rl.retry_after_secs),
            (TtsErrorKind::RateLimited, true, Some(2.0))
        );
        let quota = status_error(
            &instance(ProviderType::Openai),
            429,
            &parse_error_body(r#"{"error":{"code":"insufficient_quota"}}"#),
            None,
        );
        assert_eq!(
            (quota.kind, quota.retryable),
            (TtsErrorKind::QuotaExceeded, false)
        );
        let rate = status_error(
            &instance(ProviderType::Openai),
            429,
            &parse_error_body(r#"{"error":{"code":"rate_limit_exceeded"}}"#),
            None,
        );
        assert_eq!(
            (rate.kind, rate.retryable),
            (TtsErrorKind::RateLimited, true)
        );
        let voice = status_error(
            &o,
            400,
            &parse_error_body(
                r#"{"detail":{"code":"invalid_voice_id","message":"no such voice"}}"#,
            ),
            None,
        );
        assert_eq!(voice.kind, TtsErrorKind::NotFound);
        assert!(
            voice
                .hint
                .as_deref()
                .unwrap()
                .contains("soniccraft voices elevenlabs")
        );
        for s in [500, 502, 503, 504, 408] {
            assert!(
                status_error(&o, s, &ErrorInfo::default(), None).retryable,
                "{s}"
            );
        }
        for s in [400, 401, 402, 403, 404, 422, 501, 307] {
            assert!(
                !status_error(&o, s, &ErrorInfo::default(), None).retryable,
                "{s}"
            );
        }
        assert_eq!(
            status_error(&o, 422, &ErrorInfo::default(), None).message,
            "Unprocessable Entity"
        );
        let redirect = redirect_error(&o, 307, Some("http://u:secret@other.example/x")).unwrap();
        assert!(redirect.has_code(error_codes::REDIRECT_REFUSED));
        assert!(
            redirect.message.contains("to http://other.example/x")
                && !redirect.message.contains("secret"),
            "{}",
            redirect.message
        );
        assert!(redirect_error(&o, 200, None).is_none());
    }

    #[test]
    fn path_encoding() {
        assert_eq!(
            encode_path_segment("JBFqnCBsd6RMkjVDRZzb"),
            "JBFqnCBsd6RMkjVDRZzb"
        );
        assert_eq!(encode_path_segment("a/b c?"), "a%2Fb%20c%3F");
    }

    #[test]
    fn missing_key_error_names_the_variable() {
        let mut o = instance(ProviderType::Cartesia);
        let url = "https://api.cartesia.ai";
        let err = keys::require(&EnvSource::fixed([("X", "y")]), &o, url).unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Auth);
        assert!(err.message.contains("CARTESIA_API_KEY"));
        o.api_key_env = Some("MY_CARTESIA".into());
        let key = keys::require(&EnvSource::fixed([("MY_CARTESIA", " k ")]), &o, url).unwrap();
        assert_eq!(key, "k");
        let header = secret_header(&o, &key).unwrap();
        assert!(header.is_sensitive());
        assert!(secret_header(&o, "bad\nkey").is_err());
    }

    /// Serve one canned HTTP response on a local port; returns the base URL.
    async fn serve_once(response: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        url
    }

    #[tokio::test]
    async fn bodies_are_capped() {
        let o = instance(ProviderType::Openai);
        let client = reqwest::Client::new();
        // A declared length over the limit fails before the body is read.
        let url = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\nabc"
                .to_vec(),
        )
        .await;
        let response = client.get(&url).send().await.unwrap();
        let err = read_body(&o, &url, response, 1_000).await.unwrap_err();
        assert!(err.has_code(error_codes::RESPONSE_TOO_LARGE), "{err:?}");
        // A chunked body is cut off once it passes the limit.
        let mut chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for _ in 0..4 {
            chunked.extend_from_slice(format!("{:x}\r\n{}\r\n", 600, "x".repeat(600)).as_bytes());
        }
        chunked.extend_from_slice(b"0\r\n\r\n");
        let url = serve_once(chunked).await;
        let response = client.get(&url).send().await.unwrap();
        let err = read_body(&o, &url, response, 1_000).await.unwrap_err();
        assert_eq!(err.kind, TtsErrorKind::Provider);
        assert!(err.message.contains("limit"), "{}", err.message);
        // Within the limit the body is returned whole.
        let url = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc".to_vec()).await;
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(read_body(&o, &url, response, 1_000).await.unwrap(), b"abc");
        // Error bodies are read up to their own cap, never more.
        let huge = "y".repeat(MAX_ERROR_BODY_BYTES + 10_000);
        let url = serve_once(
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{huge}",
                huge.len()
            )
            .into_bytes(),
        )
        .await;
        let response = client.get(&url).send().await.unwrap();
        let e = read_error(response).await;
        assert_eq!((e.status, e.body.len()), (500, MAX_ERROR_BODY_BYTES));
    }

    #[tokio::test]
    async fn deadline_cuts_attempts_and_skips_retries_that_cannot_fit() {
        let o = instance(ProviderType::Openai);
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(400),
            max_retries: 3,
        };
        let started = std::time::Instant::now();
        let mut calls = 0;
        let err = with_retries(
            &o,
            policy,
            Deadline::after(Duration::from_millis(300)),
            &[],
            |_, _| {
                calls += 1;
                async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok::<(), TtsError>(())
                }
            },
        )
        .await
        .unwrap_err();
        assert!(err.has_code(error_codes::DEADLINE_EXCEEDED), "{err:?}");
        assert_eq!((err.kind, calls), (TtsErrorKind::Timeout, 1));
        assert!(started.elapsed() < Duration::from_secs(2));
        // A retryable failure whose backoff would overrun the deadline is returned at once.
        let started = std::time::Instant::now();
        let err = with_retries(
            &o,
            policy,
            Deadline::after(Duration::from_millis(200)),
            &["sk-secret-1234567890".to_owned()],
            |_, _| async {
                Err::<(), _>(
                    TtsError::new(TtsErrorKind::Provider, &o, "echo sk-secret-1234567890")
                        .with_retryable(true),
                )
            },
        )
        .await
        .unwrap_err();
        assert_eq!((err.attempts, err.message.as_str()), (1, "echo <redacted>"));
        assert!(started.elapsed() < Duration::from_millis(150));
        // The provider's own wait explains why no retry was made.
        let rate_limited = |secs: f64| {
            let o = o.clone();
            async move {
                with_retries(
                    &o,
                    policy,
                    Deadline::after(Duration::from_secs(5)),
                    &[],
                    |_, _| {
                        let error = status_error(&o, 429, &ErrorInfo::default(), Some(secs));
                        async move { Err::<(), _>(error) }
                    },
                )
                .await
                .unwrap_err()
            }
        };
        let far = rate_limited(120_000.0).await;
        assert_eq!(far.attempts, 1);
        assert!(
            far.hint
                .as_deref()
                .unwrap()
                .starts_with("the provider asked to retry after 33.3 h"),
            "{far:?}"
        );
        let long = rate_limited(30.0).await;
        assert!(
            long.hint
                .as_deref()
                .unwrap()
                .contains("more than is left of the cue's `timeout_ms` (5000 ms)"),
            "{long:?}"
        );
    }
}
