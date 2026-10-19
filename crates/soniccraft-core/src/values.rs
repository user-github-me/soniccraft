//! Leaf value types of the DSL that accept more than one surface form (numbers or strings, flags or
//! objects). They deserialize permissively and are interpreted during lowering, so malformed values become
//! diagnostics instead of parse failures.

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Deserialize;

/// A number or a string, as written in the document.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum NumOrString {
    /// A number as written.
    Num(f64),
    /// A string as written.
    Str(String),
}

impl std::fmt::Display for NumOrString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumOrString::Num(n) => write!(f, "{n}"),
            NumOrString::Str(s) => write!(f, "{s:?}"),
        }
    }
}

// ---- Millis ------------------------------------------------------------------------------------------------

/// A time value: a number of milliseconds, or a string with units ("350ms", "1.5s", "2m", "1m30s",
/// "01:30.5", "00:01:30.500").
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct Millis(pub NumOrString);

impl Millis {
    /// Construct from a number of milliseconds.
    pub fn ms(ms: f64) -> Self {
        Millis(NumOrString::Num(ms))
    }

    /// Interpret the value as milliseconds. Negative values are returned as-is (callers decide whether
    /// they are allowed). Errors describe the accepted forms.
    pub fn to_ms(&self) -> Result<f64, String> {
        match &self.0 {
            NumOrString::Num(n) if n.is_finite() => Ok(*n),
            NumOrString::Num(n) => Err(format!("`{n}` is not a finite number of milliseconds")),
            NumOrString::Str(s) => parse_duration_str(s),
        }
    }
}

const DURATION_FORMS: &str = "use a number of milliseconds (350) or a string like \"350ms\", \"1.5s\", \"2m\", \"1m30s\" or \"00:01:30.500\"";

/// Why a duration string was rejected ([`parse_duration`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationError {
    /// What is wrong, including the accepted forms (and the corrected spelling, when there is one).
    pub message: String,
    /// The value spelled correctly, when the mistake has one obvious fix: `"350MS"` → `"350ms"`,
    /// `"1:99.5"` → `"159.5s"`.
    pub suggestion: Option<String>,
}

impl std::fmt::Display for DurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Parse a duration string into milliseconds. See [`Millis`] for the accepted forms and [`parse_duration`]
/// for the grammar; the error is [`DurationError::message`].
pub fn parse_duration_str(input: &str) -> Result<f64, String> {
    parse_duration(input).map_err(|e| e.message)
}

/// Whitespace allowed around and inside duration strings: space, tab, CR and LF (the same set as the
/// JSON Schema pattern, see [`millis_string_pattern`]).
fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// Seconds, minutes and hours as written in unit groups (lowercase only).
fn unit_factor(unit: &str) -> Option<f64> {
    Some(match unit {
        "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
        _ => return None,
    })
}

/// Parse a duration string into milliseconds.
///
/// The grammar (also the JSON Schema `pattern` of `Millis` strings, see `millis_string_pattern`):
///
/// - optional whitespace, an optional sign (`+` / `-`), optional whitespace, then one of:
/// - a bare number (`350`, `2.5`, `.5`): milliseconds;
/// - one or more `<number><unit>` groups, optionally separated by whitespace (`1m30s`, `1m 30.5s`), with the
///   lowercase units `ms`/`msec(s)`/`millisecond(s)`, `s`/`sec(s)`/`second(s)`, `m`/`min(s)`/`minute(s)`,
///   `h`/`hr(s)`/`hour(s)`;
/// - a clock time `mm:ss[.fff]` or `hh:mm:ss[.fff]` of decimal digits (no exponents), where seconds (and the
///   minutes of `hh:mm:ss`) are below 60 and take at most two digits; the leading part is unbounded.
///
/// A zero result is always `+0.0` (so `"-0ms"` is a valid non-negative time).
pub fn parse_duration(input: &str) -> Result<f64, DurationError> {
    let s = input.trim_matches(is_ws);
    let plain = |message: String| DurationError {
        message,
        suggestion: None,
    };
    if s.is_empty() {
        return Err(plain(format!("empty duration; {DURATION_FORMS}")));
    }
    let (sign, body) = match s.as_bytes()[0] {
        b'-' => (-1.0, s[1..].trim_start_matches(is_ws)),
        b'+' => (1.0, s[1..].trim_start_matches(is_ws)),
        _ => (1.0, s),
    };
    let bad = || plain(format!("invalid duration {input:?}; {DURATION_FORMS}"));
    if body.is_empty() {
        return Err(bad());
    }
    let ms = if body.contains(':') {
        clock_ms(body, input)?
    } else {
        units_ms(body, input)?
    };
    Ok(if ms == 0.0 { 0.0 } else { sign * ms })
}

/// `mm:ss[.fff]` / `hh:mm:ss[.fff]` (see [`parse_duration`]).
fn clock_ms(body: &str, input: &str) -> Result<f64, DurationError> {
    let bad = |detail: &str, suggestion: Option<String>| DurationError {
        message: match &suggestion {
            Some(s) => format!("invalid duration {input:?}: {detail}; write {s:?}"),
            None if detail.is_empty() => format!("invalid duration {input:?}; {DURATION_FORMS}"),
            None => format!("invalid duration {input:?}: {detail}; {DURATION_FORMS}"),
        },
        suggestion,
    };
    let parts: Vec<&str> = body.split(':').map(|p| p.trim_matches(is_ws)).collect();
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if parts.len() > 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(bad("", None));
    }
    if parts.iter().any(|p| p.contains(['e', 'E'])) {
        return Err(bad(
            "clock times take decimal digits only (no exponents), e.g. \"01:30.5\" or \"00:01:30.500\"",
            None,
        ));
    }
    let (seconds, leading) = parts.split_last().ok_or_else(|| bad("", None))?;
    let (sec_int, sec_frac) = seconds.split_once('.').unwrap_or((seconds, ""));
    if !digits(sec_int) || !sec_frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad("", None));
    }
    if !leading.iter().all(|p| digits(p)) {
        return Err(bad("", None));
    }
    // Total in milliseconds with exact decimal arithmetic where possible.
    let mut total_ms = decimal_times(seconds, 1000.0).ok_or_else(|| bad("", None))?;
    let mut factor = 60_000.0;
    for part in leading.iter().rev() {
        total_ms += decimal_times(part, factor).ok_or_else(|| bad("", None))?;
        factor *= 60.0;
    }
    if !total_ms.is_finite() {
        return Err(bad("", None));
    }
    let as_seconds = || {
        let secs = total_ms / 1000.0;
        Some(format!("{}s", trim_float(secs)))
    };
    if sec_int.len() > 2 || sec_int.parse::<u32>().map_or(true, |v| v >= 60) {
        let detail = if sec_int.len() > 2 && sec_int.parse::<u32>().is_ok_and(|v| v < 60) {
            "the seconds of a clock time take at most two digits"
        } else {
            "the seconds of a clock time must be below 60"
        };
        return Err(bad(detail, as_seconds()));
    }
    if parts.len() == 3 {
        let minutes = parts[1];
        if minutes.len() > 2 || minutes.parse::<u32>().map_or(true, |v| v >= 60) {
            return Err(bad(
                "the minutes of a clock time hh:mm:ss must be below 60 and take at most two digits",
                as_seconds(),
            ));
        }
    }
    Ok(total_ms)
}

/// One or more `<number><unit>` groups, or a bare number (see [`parse_duration`]).
fn units_ms(body: &str, input: &str) -> Result<f64, DurationError> {
    let bad = || DurationError {
        message: format!("invalid duration {input:?}; {DURATION_FORMS}"),
        suggestion: None,
    };
    let bytes = body.as_bytes();
    let ws = |b: u8| is_ws(b as char);
    let mut i = 0;
    let mut total_ms = 0.0;
    let mut groups = 0;
    while i < bytes.len() {
        while i < bytes.len() && ws(bytes[i]) {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if num_start == i {
            return Err(bad());
        }
        let number = &body[num_start..i];
        while i < bytes.len() && ws(bytes[i]) {
            i += 1;
        }
        let unit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        let unit = &body[unit_start..i];
        let factor = if unit.is_empty() {
            // A bare number is only allowed as the whole value.
            if groups > 0 || i < bytes.len() {
                return Err(bad());
            }
            1.0
        } else {
            match unit_factor(unit) {
                Some(f) => f,
                None if unit_factor(&unit.to_ascii_lowercase()).is_some() => {
                    let fixed = input.trim_matches(is_ws).to_ascii_lowercase();
                    let suggestion = parse_duration(&fixed).is_ok().then_some(fixed);
                    return Err(DurationError {
                        message: match &suggestion {
                            Some(s) => {
                                format!(
                                    "invalid duration {input:?}: units are lowercase; write {s:?}"
                                )
                            }
                            None => format!(
                                "invalid duration {input:?}: units are lowercase; {DURATION_FORMS}"
                            ),
                        },
                        suggestion,
                    });
                }
                None => return Err(bad()),
            }
        };
        total_ms += decimal_times(number, factor).ok_or_else(bad)?;
        groups += 1;
    }
    if groups == 0 || !total_ms.is_finite() {
        return Err(bad());
    }
    Ok(total_ms)
}

/// Format a number without a trailing `.0` (`159.5`, `90`).
pub(crate) fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// `number × factor` computed exactly for decimal strings: `"1.001" × 1000` is 1001, not 1000.9999999999999.
/// Falls back to f64 arithmetic for numbers with more than 15 significant digits. `number` is unsigned
/// (digits with an optional `.`); anything else is None.
pub(crate) fn decimal_times(number: &str, factor: f64) -> Option<f64> {
    let (int_part, frac_part) = number.split_once('.').unwrap_or((number, ""));
    let digits_ok = |d: &str| d.bytes().all(|b| b.is_ascii_digit());
    if (int_part.is_empty() && frac_part.is_empty())
        || !digits_ok(int_part)
        || !digits_ok(frac_part)
    {
        return None;
    }
    if int_part.len() + frac_part.len() <= 15 && factor.fract() == 0.0 && factor <= 3_600_000.0 {
        let mantissa: u64 = format!("{int_part}{frac_part}").parse().ok()?;
        let scaled = mantissa.checked_mul(factor as u64)?;
        return Some(scaled as f64 / 10f64.powi(frac_part.len() as i32));
    }
    let value: f64 = number.parse().ok()?;
    Some(value * factor)
}

/// Which values a time field accepts (DESIGN.md §2.1: negative times are only valid for `offset_ms`, and
/// lengths such as `duration_ms` must be positive). Selects the JSON Schema of the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimeSign {
    /// `offset_ms`: any sign.
    Any,
    /// Every other time value: 0 or more.
    NonNegative,
    /// Lengths that must not be zero (`duration_ms`, `subtitles.max_duration_ms`).
    Positive,
}

// Building blocks of the duration grammar (see `parse_duration`), shared by every Millis pattern.
const RX_WS: &str = "[ \\t\\n\\r]";
const RX_NUMBER: &str = "([0-9]+(\\.[0-9]*)?|\\.[0-9]+)";
const RX_ZERO: &str = "(0+(\\.0*)?|\\.0+)";
const RX_UNIT: &str = "(ms|msecs?|milliseconds?|s|secs?|seconds?|m|mins?|minutes?|h|hrs?|hours?)";

/// The `pattern` of `Millis` strings that [`parse_duration`] accepts with the given sign rule.
/// [`TimeSign::Positive`] strings additionally need a non-zero digit (a second `pattern`, see
/// [`millis_schema`]).
pub(crate) fn millis_string_pattern(sign: TimeSign) -> String {
    let w = RX_WS;
    let body = |number: &str, sec_lead: &str, digit: &str| {
        // units | bare number | clock (mm:ss[.fff] or hh:mm:ss[.fff])
        format!(
            "(({number}{w}*{RX_UNIT}{w}*)+|{number}|{digit}+{w}*:{w}*({sec_lead}{digit}{w}*:{w}*)?{sec_lead}{digit}(\\.{digit}*)?)"
        )
    };
    let any = body(RX_NUMBER, "[0-5]?", "[0-9]");
    match sign {
        TimeSign::Any => format!("^{w}*([+-]{w}*)?{any}{w}*$"),
        TimeSign::NonNegative | TimeSign::Positive => {
            let zero = body(RX_ZERO, "0?", "0");
            format!("^{w}*((\\+{w}*)?{any}|-{w}*{zero}){w}*$")
        }
    }
}

/// The JSON Schema of a time field with the given sign rule: a number of milliseconds within
/// ±[`crate::MAX_TIME_MS`] (24 h), or a duration string ([`millis_string_pattern`]).
pub(crate) fn millis_schema(sign: TimeSign) -> serde_json::Value {
    let max = crate::MAX_TIME_MS as u64;
    let (number, what) = match sign {
        TimeSign::Any => (
            serde_json::json!({ "type": "number", "minimum": -(max as i64), "maximum": max }),
            "Signed time value (negative values move the anchor earlier)",
        ),
        TimeSign::NonNegative => (
            serde_json::json!({ "type": "number", "minimum": 0, "maximum": max }),
            "Time value (0 or more)",
        ),
        TimeSign::Positive => (
            serde_json::json!({ "type": "number", "exclusiveMinimum": 0, "maximum": max }),
            "Length (greater than 0)",
        ),
    };
    let string = match sign {
        TimeSign::Positive => serde_json::json!({
            "type": "string",
            "allOf": [{ "pattern": millis_string_pattern(sign) }, { "pattern": "[1-9]" }]
        }),
        _ => serde_json::json!({ "type": "string", "pattern": millis_string_pattern(sign) }),
    };
    serde_json::json!({
        "description": format!(
            "{what}: a number of milliseconds (at most 86400000 = 24 h), or a string with lowercase units such \
             as \"350ms\", \"1.5s\", \"2m\", \"1m30s\", \"01:30.5\" or \"00:01:30.500\"."
        ),
        "anyOf": [number, string]
    })
}

impl JsonSchema for Millis {
    fn schema_name() -> Cow<'static, str> {
        "Millis".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // `millis_schema` always returns an object, so the conversion cannot fail.
        Schema::try_from(millis_schema(TimeSign::Any)).unwrap_or_default()
    }
}

// ---- Bitrate -----------------------------------------------------------------------------------------------

/// A bitrate: "320k", "192kbps", 192000 (bps) or 192 (numbers below 1000 are kbps).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct Bitrate(pub NumOrString);

/// The largest accepted bitrate, in bits per second.
pub const MAX_BITRATE_BPS: f64 = 10_000_000.0;

/// The grammar of bitrate strings (also the first `pattern` of the `Bitrate` schema): a decimal number
/// (no sign or exponent), an optional `k`, an optional `bps`, case-insensitive, with optional whitespace.
const BITRATE_PATTERN: &str =
    "^\\s*([0-9]+(\\.[0-9]*)?|\\.[0-9]+)\\s*([kK]\\s*)?([bB][pP][sS])?\\s*$";

/// Bitrate strings of at most [`MAX_BITRATE_BPS`]: up to 10000 with `k`, up to 10000000 without (numbers
/// below 1000 are kbps, so they are always in range).
const BITRATE_MAGNITUDE_PATTERN: &str = "^\\s*0*(([0-9]{0,4}(\\.[0-9]*)?|10000(\\.0*)?)\\s*[kK]|([0-9]{0,7}(\\.[0-9]*)?|10000000(\\.0*)?))\\s*([bB][pP][sS])?\\s*$";

impl Bitrate {
    /// Interpret as bits per second. Strings follow one grammar (the schema's pattern): a decimal number
    /// without sign or exponent, an optional `k` and an optional `bps`, case-insensitive (`"320k"`,
    /// `"192 kbps"`, `"128000"`). Numbers below 1000 are kbps; the result is at most 10 Mbit/s.
    pub fn to_bps(&self) -> Result<u32, String> {
        let bad = || {
            format!(
                "invalid bitrate {}; use a string like \"192k\" or a number of bits per second",
                self.0
            )
        };
        let value = match &self.0 {
            NumOrString::Num(n) => *n,
            NumOrString::Str(s) => {
                let t = s.trim().to_ascii_lowercase();
                let t = t.strip_suffix("bps").unwrap_or(&t).trim_end();
                let (number, factor) = match t.strip_suffix('k') {
                    Some(k) => (k.trim(), 1000.0),
                    None => (t, 1.0),
                };
                decimal_times(number, factor).ok_or_else(bad)?
            }
        };
        if !value.is_finite() || value <= 0.0 {
            return Err(bad());
        }
        let bps = if value < 1000.0 {
            value * 1000.0
        } else {
            value
        };
        if bps > MAX_BITRATE_BPS {
            return Err(bad());
        }
        Ok(bps.round() as u32)
    }
}

impl JsonSchema for Bitrate {
    fn schema_name() -> Cow<'static, str> {
        "Bitrate".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Bitrate for lossy formats: a string like \"320k\" or \"192kbps\", or a number (bits per second; values below 1000 are kbps). At most 10 Mbit/s.",
            "anyOf": [
                { "type": "number", "exclusiveMinimum": 0, "maximum": MAX_BITRATE_BPS as u64 },
                {
                    "type": "string",
                    "allOf": [
                        { "pattern": BITRATE_PATTERN },
                        { "pattern": BITRATE_MAGNITUDE_PATTERN },
                        { "pattern": "[1-9]" }
                    ]
                }
            ]
        })
    }
}

// ---- Version -----------------------------------------------------------------------------------------------

/// The DSL version, written as a string ("1.0") or a number (1.0).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct Version(pub NumOrString);

impl Version {
    /// `(major, minor)` if the value is a well-formed version.
    ///
    /// Strings are `[v|V]<major>[.<minor>]` with decimal digits only: no whitespace, no sign, and no
    /// leading zero in the major version (`"01.0"` is not a version). Numbers are read as written
    /// (`1`, `1.0`, `1.5`). Very large components saturate (and are then unsupported versions).
    pub fn major_minor(&self) -> Option<(u32, u32)> {
        let text = match &self.0 {
            NumOrString::Num(n) if n.is_finite() && *n >= 0.0 => format!("{n}"),
            NumOrString::Num(_) => return None,
            NumOrString::Str(s) => s.strip_prefix(['v', 'V']).unwrap_or(s).to_string(),
        };
        let (major, minor) = match text.split_once('.') {
            Some((major, minor)) => (major, Some(minor)),
            None => (text.as_str(), None),
        };
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        let value = |p: &str| {
            p.bytes().fold(0u32, |acc, b| {
                acc.saturating_mul(10).saturating_add(u32::from(b - b'0'))
            })
        };
        if !digits(major) || (major.len() > 1 && major.starts_with('0')) {
            return None;
        }
        match minor {
            Some(m) if !digits(m) => None,
            Some(m) => Some((value(major), value(m))),
            None => Some((value(major), 0)),
        }
    }
}

impl JsonSchema for Version {
    fn schema_name() -> Cow<'static, str> {
        "Version".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // The string pattern is the grammar of `Version::major_minor` restricted to major version 1 (other
        // majors are `unsupported_version` errors).
        json_schema!({
            "description": "DSL version. Use \"1.0\".",
            "anyOf": [
                { "type": "string", "pattern": "^[vV]?1(\\.[0-9]+)?$" },
                { "type": "number", "minimum": 1, "exclusiveMaximum": 2 }
            ],
            "examples": ["1.0"]
        })
    }
}

// ---- flag-or-object values ---------------------------------------------------------------------------------

/// Source-level normalization: `true`, `false`, or `{target_lufs: -20}`.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum NormalizeInput {
    /// Enable (target = mix.reference_lufs) or disable.
    Flag(bool),
    /// Enable with an explicit target.
    Config(NormalizeSpec),
}

/// Explicit source normalization target.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct NormalizeSpec {
    /// Integrated loudness target in LUFS for each source on this track.
    pub target_lufs: Option<f64>,
}

/// Silence trimming for synthesized speech: `true`, `false`, or `{threshold_db, padding_ms}`.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TrimSilenceInput {
    /// Enable with defaults (-50 dBFS, 30 ms padding) or disable.
    Flag(bool),
    /// Enable with explicit parameters.
    Config(TrimSilenceSpec),
}

/// Silence trimming parameters.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
pub struct TrimSilenceSpec {
    /// Level below which audio counts as silence, in dBFS. Default -50.
    pub threshold_db: Option<f64>,
    /// Silence kept before the first and after the last audible frame. Default 30 ms.
    pub padding_ms: Option<Millis>,
}

/// Subtitle export switch: `true` (= vtt + srt), `false`, a single format, or a list of formats (vtt, srt, json).
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum SubtitleExport {
    /// true = vtt + srt.
    Flag(bool),
    /// A single format name.
    One(String),
    /// Several format names.
    Many(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(s: &str) -> f64 {
        parse_duration_str(s).unwrap()
    }

    #[test]
    fn duration_forms() {
        assert_eq!(ms("350ms"), 350.0);
        assert_eq!(ms("350"), 350.0);
        assert_eq!(ms(" 1.5s "), 1500.0);
        assert_eq!(ms("2m"), 120_000.0);
        assert_eq!(ms("1m30s"), 90_000.0);
        assert_eq!(ms("1m 30.5s"), 90_500.0);
        assert_eq!(ms("01:30.5"), 90_500.0);
        assert_eq!(ms("00:01:30.500"), 90_500.0);
        assert_eq!(ms("-200ms"), -200.0);
        assert_eq!(ms("1h"), 3_600_000.0);
        assert_eq!(ms("1.001s"), 1001.0);
        assert_eq!(ms("2.675s"), 2675.0);
        assert_eq!(ms("1.5m"), 90_000.0);
        assert_eq!(ms("0.0001s"), 0.1);
    }

    #[test]
    fn duration_rejects_garbage() {
        for bad in [
            "",
            "ms",
            "350 msecx",
            "1.2.3s",
            "abc",
            "1:",
            ":30",
            "5 5",
            "1:2:3:4",
            "10ms 5",
        ] {
            assert!(parse_duration_str(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn bitrate_forms() {
        let b = |v: NumOrString| Bitrate(v).to_bps();
        assert_eq!(b(NumOrString::Str("320k".into())), Ok(320_000));
        assert_eq!(b(NumOrString::Str("192kbps".into())), Ok(192_000));
        assert_eq!(b(NumOrString::Num(128.0)), Ok(128_000));
        assert_eq!(b(NumOrString::Num(96_000.0)), Ok(96_000));
        assert!(b(NumOrString::Str("loud".into())).is_err());
    }

    /// The `Bitrate` schema accepts exactly what `to_bps` accepts.
    #[test]
    fn bitrate_schema_matches_the_parser() {
        let schema: serde_json::Value = <Bitrate as JsonSchema>::json_schema(
            &mut schemars::generate::SchemaSettings::draft2020_12().into_generator(),
        )
        .into();
        let validator = jsonschema::draft202012::new(&schema).expect("Bitrate schema compiles");
        let strings = [
            "320k",
            "320K",
            "320kbps",
            "320 kbps",
            "192000",
            "320KBPS",
            "96k bps",
            " 96 k ",
            "96 bps",
            "0.5k",
            ".5k",
            "5.k",
            "10000k",
            "10000.0k",
            "10000.5k",
            "99999k",
            "10000000",
            "10000001",
            "10000000.0",
            "0k",
            "0",
            "0.0",
            "-5k",
            "+5k",
            "1e3",
            "1e3k",
            "inf",
            "nan",
            "k",
            "bps",
            "",
            "5 5k",
            "5kk",
            "5kb ps",
            "0320k",
            "00010000k",
            "0.0000001k",
            "１k",
        ];
        for s in strings {
            let parsed = Bitrate(NumOrString::Str(s.to_string())).to_bps();
            let value = serde_json::Value::String(s.to_string());
            assert_eq!(
                validator.is_valid(&value),
                parsed.is_ok(),
                "schema vs parser on {s:?}"
            );
        }
        for n in [
            0.0,
            -1.0,
            0.5,
            96.0,
            999.0,
            1000.0,
            9_999_999.0,
            10_000_000.0,
            10_000_001.0,
        ] {
            let parsed = Bitrate(NumOrString::Num(n)).to_bps();
            assert_eq!(
                validator.is_valid(&serde_json::json!(n)),
                parsed.is_ok(),
                "schema vs parser on {n}"
            );
        }
    }

    #[test]
    fn version_forms() {
        let v = |n: NumOrString| Version(n).major_minor();
        assert_eq!(v(NumOrString::Str("1.0".into())), Some((1, 0)));
        assert_eq!(v(NumOrString::Str("1".into())), Some((1, 0)));
        assert_eq!(v(NumOrString::Num(1.0)), Some((1, 0)));
        assert_eq!(v(NumOrString::Num(1.5)), Some((1, 5)));
        assert_eq!(v(NumOrString::Str("v1.2".into())), Some((1, 2)));
        assert_eq!(v(NumOrString::Str("1.00".into())), Some((1, 0)));
        assert_eq!(v(NumOrString::Str("2.0".into())), Some((2, 0)));
        assert_eq!(v(NumOrString::Str("0.9".into())), Some((0, 9)));
        assert_eq!(
            v(NumOrString::Str("1.99999999999".into())),
            Some((1, u32::MAX))
        );
        // The grammar is strict, like the schema pattern `^[vV]?1(\.[0-9]+)?$` for major version 1.
        for bad in [
            "one", "01.0", "01", "+1.0", " 1.0", "1.0 ", "1.", ".5", "1.0.0", "vv1", "1,0", "1e0",
            "",
        ] {
            assert_eq!(v(NumOrString::Str(bad.into())), None, "{bad:?}");
        }
        assert_eq!(v(NumOrString::Num(-1.0)), None);
    }

    #[test]
    fn units_are_lowercase_with_a_suggestion() {
        for (input, fixed) in [
            ("350MS", "350ms"),
            ("1.5S", "1.5s"),
            ("2 Min", "2 min"),
            ("1M30S", "1m30s"),
        ] {
            let e = parse_duration(input).unwrap_err();
            assert_eq!(e.suggestion.as_deref(), Some(fixed), "{input}");
            assert!(e.message.contains("units are lowercase"), "{}", e.message);
            assert!(e.message.contains(&format!("{fixed:?}")), "{}", e.message);
        }
        // Not a unit in any case: no suggestion.
        assert_eq!(parse_duration("350 MSX").unwrap_err().suggestion, None);
    }

    #[test]
    fn clock_times_are_strict() {
        assert_eq!(ms("1:5"), 65_000.0);
        assert_eq!(ms("90:00"), 5_400_000.0);
        assert_eq!(ms("1 : 30"), 90_000.0);
        assert_eq!(ms("-0:01.5"), -1500.0);
        assert_eq!(ms("01:59.999"), 119_999.0);
        assert_eq!(ms("1:30."), 90_000.0);
        // Exponents, infinities and other float syntax are not clock digits.
        for bad in [
            "1:1e2", "1:1E2", "1:inf", "1:NaN", "1:+30", "1:-30", "1:.5", "1:3_0", "1e2:00",
        ] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should fail");
        }
        let e = parse_duration("1:1e2").unwrap_err();
        assert!(e.message.contains("no exponents"), "{}", e.message);
        // Seconds (and the minutes of hh:mm:ss) are below 60, with a mechanical suggestion.
        let e = parse_duration("1:99.5").unwrap_err();
        assert_eq!(e.suggestion.as_deref(), Some("159.5s"));
        assert!(e.message.contains("below 60"), "{}", e.message);
        assert_eq!(
            parse_duration("0:60").unwrap_err().suggestion.as_deref(),
            Some("60s")
        );
        assert_eq!(
            parse_duration("1:75:00").unwrap_err().suggestion.as_deref(),
            Some("8100s")
        );
        assert!(parse_duration("1:005").is_err());
    }

    #[test]
    fn zero_is_never_negative() {
        for zero in ["-0", "-0ms", "-0:00", "- 0.0s", "+0", "0"] {
            let v = parse_duration(zero).unwrap();
            assert_eq!(v, 0.0, "{zero}");
            assert!(v.is_sign_positive(), "{zero}");
        }
    }

    /// The JSON Schema patterns accept exactly the strings the parser accepts (with each sign rule).
    #[test]
    fn schema_patterns_match_the_parser() {
        let compile = |sign: TimeSign| {
            jsonschema::draft202012::new(&millis_schema(sign)).expect("Millis schema compiles")
        };
        let (any, non_negative, positive) = (
            compile(TimeSign::Any),
            compile(TimeSign::NonNegative),
            compile(TimeSign::Positive),
        );
        let samples = [
            "350ms",
            "350",
            " 1.5s ",
            "2m",
            "1m30s",
            "1m 30.5s",
            "01:30.5",
            "00:01:30.500",
            "-200ms",
            "+1s",
            "1h",
            "500 msecs",
            "2 hrs",
            "1.5 seconds",
            "3 minutes",
            "250 milliseconds",
            ".5s",
            "5.s",
            "1 sec",
            "2 mins",
            "1h30m",
            "90:00",
            "1:5",
            "1 : 30",
            "-0",
            "-0ms",
            "-0:00",
            "- 0.0s",
            "+0",
            "0",
            "0.000s",
            "0:00:00.0",
            "-1:30",
            "\t2s\n",
            "1:30.",
            "0.0001s",
            "",
            "ms",
            "350 msecx",
            "1.2.3s",
            "abc",
            "1:",
            ":30",
            "5 5",
            "1:2:3:4",
            "10ms 5",
            "350MS",
            "1.5S",
            "1:1e2",
            "1:99.5",
            "0:60",
            "1:75:00",
            "1:005",
            "1:.5",
            "1:inf",
            "-",
            "+",
            "--1s",
            "1s-",
            "1 s",
            "1s 2",
            "1.5.s",
            ".",
            "1m30",
            "30s1m",
            "\u{a0}1s",
            "1\u{a0}s",
            "1\u{b}s",
            "1e3",
            "1e3ms",
            "+-1s",
            "0x10ms",
            "１s",
        ];
        for s in samples {
            let parsed = parse_duration(s);
            let value = serde_json::Value::String(s.to_string());
            assert_eq!(
                any.is_valid(&value),
                parsed.is_ok(),
                "signed pattern vs parser on {s:?}"
            );
            let non_neg = parsed.as_ref().is_ok_and(|v| *v >= 0.0);
            assert_eq!(
                non_negative.is_valid(&value),
                non_neg,
                "non-negative pattern on {s:?}"
            );
            let pos = parsed.as_ref().is_ok_and(|v| *v > 0.0);
            assert_eq!(positive.is_valid(&value), pos, "positive pattern on {s:?}");
        }
        // Numbers: the 24 h bound and the sign rules.
        let max = crate::MAX_TIME_MS;
        for (n, a, nn, p) in [
            (0.0, true, true, false),
            (-0.0, true, true, false),
            (1.0, true, true, true),
            (-1.0, true, false, false),
            (max, true, true, true),
            (max + 1.0, false, false, false),
            (-max, true, false, false),
            (-max - 1.0, false, false, false),
        ] {
            let v = serde_json::json!(n);
            assert_eq!(any.is_valid(&v), a, "{n}");
            assert_eq!(non_negative.is_valid(&v), nn, "{n}");
            assert_eq!(positive.is_valid(&v), p, "{n}");
        }
    }
}
