//! Small value checks shared by the lowering passes. Each check reports its own diagnostic and returns
//! the accepted value (or None), so callers can continue with a default and keep collecting problems.

use crate::diagnostics::{self, Diagnostic, codes};
use crate::values::{Millis, NumOrString};

/// Allowed sign of a time value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sign {
    /// `offset_ms` only.
    Any,
    /// Every other time value.
    NonNegative,
    /// Lengths that must not be zero (`duration_ms`, `max_duration_ms`, …).
    Positive,
}

/// The last segment of a path, for messages (`fade_in_ms`, `trigger_tracks[1]`).
pub(super) fn field_name(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// Format milliseconds compactly (`350`, `1500`, `0.5`).
pub(super) fn fmt_ms(ms: f64) -> String {
    fmt_num(ms)
}

/// Format a number without a trailing `.0` (`1e18` in exponent form rather than with 19 digits).
pub(super) fn fmt_num(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else if v.is_finite() && v.abs() >= 1e15 {
        format!("{v:e}")
    } else {
        format!("{v}")
    }
}

/// `invalid_value` for a time beyond ±[`crate::MAX_TIME_MS`] (24 h).
fn too_long(field: &str, ms: f64, path: &str, sign: Sign) -> Diagnostic {
    let limit = if sign == Sign::Any {
        format!(
            "offsets must be between -24 h and 24 h (±{} ms)",
            fmt_num(crate::MAX_TIME_MS)
        )
    } else {
        format!("the maximum is 24 h ({} ms)", fmt_num(crate::MAX_TIME_MS))
    };
    Diagnostic::error(
        codes::INVALID_VALUE,
        format!(
            "`{field}` is {} ms ({}); {limit}",
            fmt_num(ms),
            diagnostics::human_duration(ms)
        ),
    )
    .with_path(path)
    .with_hint(format!(
        "Times are milliseconds and at most 24 h; a value this large is usually epoch milliseconds (a wall-clock \
         timestamp) or seconds multiplied by 1000 twice. Write the time on the timeline, e.g. `{field}: 1500` or \
         `{field}: \"1.5s\"`, and split a composition longer than 24 h into several timelines"
    ))
}

/// Convert a time value; reports `invalid_value` for unparsable strings, disallowed signs and values beyond
/// ±[`crate::MAX_TIME_MS`] (24 h), so every accepted time is finite and far from frame-count overflow.
pub(super) fn millis(
    diags: &mut Vec<Diagnostic>,
    value: Option<&Millis>,
    path: &str,
    sign: Sign,
) -> Option<f64> {
    let value = value?;
    let field = field_name(path);
    match value.to_ms() {
        Err(reason) => {
            diags.push(
                Diagnostic::error(codes::INVALID_VALUE, format!("`{field}`: {reason}"))
                    .with_path(path)
                    .with_hint(format!(
                        "Write `{field}` as a number of milliseconds (`{field}: 350`) or with a unit, e.g. \
                         `{field}: \"350ms\"`, `\"1.5s\"`, `\"2m\"` or `\"00:01:30.500\"`"
                    )),
            );
            None
        }
        Ok(ms) if sign == Sign::NonNegative && ms < 0.0 => {
            diags.push(
                Diagnostic::error(
                    codes::INVALID_VALUE,
                    format!("`{field}` must not be negative (got {} ms)", fmt_ms(ms)),
                )
                .with_path(path)
                .with_hint(
                    "Negative times are only allowed for `offset_ms`; use a value of 0 or more",
                ),
            );
            None
        }
        Ok(ms) if sign == Sign::Positive && ms <= 0.0 => {
            diags.push(
                Diagnostic::error(
                    codes::INVALID_VALUE,
                    format!("`{field}` must be greater than 0 (got {} ms)", fmt_ms(ms)),
                )
                .with_path(path)
                .with_hint(format!(
                    "Use a positive length such as `{field}: 1000`, or remove it"
                )),
            );
            None
        }
        Ok(ms) if ms.abs() > crate::MAX_TIME_MS => {
            diags.push(too_long(field, ms, path, sign));
            None
        }
        Ok(ms) => Some(ms),
    }
}

/// What a time field is, for [`seconds_as_ms`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeUse {
    /// A clip (or output) length: suspicious below 100 ms.
    Length,
    /// A fade or crossfade: suspicious above 0 and below 10 ms.
    Fade,
    /// A pause: suspicious above 0 and below 10 ms.
    Pause,
    /// A start time or an anchor offset: suspicious when fractional and below 10 ms in magnitude.
    Start,
}

/// True when the value was written as a bare number of milliseconds (`30`, `2.5`, `"30"`), not with a unit
/// or in clock form.
pub(super) fn is_bare_number(value: &Millis) -> bool {
    match &value.0 {
        NumOrString::Num(_) => true,
        NumOrString::Str(s) => {
            let t = s.trim().trim_start_matches(['+', '-']);
            !t.is_empty()
                && t.chars().any(|c| c.is_ascii_digit())
                && t.chars().all(|c| c.is_ascii_digit() || c == '.')
        }
    }
}

/// True when a bare number `ms` used as `usage` is almost certainly seconds written as milliseconds.
pub(super) fn looks_like_seconds(value: &Millis, ms: f64, usage: TimeUse) -> bool {
    if !is_bare_number(value) || !ms.is_finite() {
        return false;
    }
    match usage {
        TimeUse::Length => ms > 0.0 && ms < 100.0,
        TimeUse::Fade | TimeUse::Pause => ms > 0.0 && ms < 10.0,
        TimeUse::Start => ms.fract() != 0.0 && ms.abs() < 10.0,
    }
}

/// `suspicious_value` (warning) when a bare number is almost certainly seconds written as milliseconds
/// (`duration_ms: 30` for a music bed): the message states the unit, the hint gives the seconds spelling.
pub(super) fn seconds_as_ms(
    diags: &mut Vec<Diagnostic>,
    value: Option<&Millis>,
    ms: f64,
    path: &str,
    usage: TimeUse,
) {
    let Some(value) = value else { return };
    if !looks_like_seconds(value, ms, usage) {
        return;
    }
    let field = field_name(path);
    let n = fmt_num(ms);
    let unit = if ms.abs() == 1.0 {
        "millisecond"
    } else {
        "milliseconds"
    };
    let seconds = if ms.abs() == 1.0 { "second" } else { "seconds" };
    diags.push(
        Diagnostic::warning(
            codes::SUSPICIOUS_VALUE,
            format!("`{field}: {n}` is {n} {unit}"),
        )
        .with_path(path)
        .with_hint(format!(
            "Times are milliseconds: write `{field}: \"{n}s\"` if you meant {n} {seconds}"
        )),
    );
}

/// Check that a number lies in `[min, max]`; reports `invalid_value` with `hint` otherwise.
pub(super) fn range(
    diags: &mut Vec<Diagnostic>,
    value: Option<f64>,
    path: &str,
    min: f64,
    max: f64,
    hint: &str,
) -> Option<f64> {
    let v = value?;
    if !v.is_finite() || v < min || v > max {
        let field = field_name(path);
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!(
                    "`{field}` must be between {} and {} (got {})",
                    fmt_num(min),
                    fmt_num(max),
                    fmt_num(v)
                ),
            )
            .with_path(path)
            .with_hint(hint),
        );
        return None;
    }
    Some(v)
}

/// Linear gain (`volume`): ≥ 0; above 4.0 is an error, above 2.0 a warning (DESIGN.md §2.1).
pub(super) fn volume(diags: &mut Vec<Diagnostic>, value: Option<f64>, path: &str) -> Option<f64> {
    let v = value?;
    let field = field_name(path);
    if !v.is_finite() || !(0.0..=4.0).contains(&v) {
        diags.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!("`{field}` must be a linear gain between 0 and 4 (got {})", fmt_num(v)),
            )
            .with_path(path)
            .with_hint("1.0 is unity gain, 0.5 is about -6 dB, 2.0 about +6 dB; use `gain_db` for larger changes"),
        );
        return None;
    }
    if v > 2.0 {
        diags.push(
            Diagnostic::warning(
                codes::INVALID_VALUE,
                format!("`{field}` {} is very loud (more than +6 dB)", fmt_num(v)),
            )
            .with_path(path)
            .with_hint(
                "Values above 2.0 are usually a mistake (volume is linear: 1.0 = unity); the master is \
                 loudness-normalized anyway",
            ),
        );
    }
    Some(v)
}

/// `gain_db` in [-60, +24].
pub(super) fn gain_db(diags: &mut Vec<Diagnostic>, value: Option<f64>, path: &str) -> Option<f64> {
    range(
        diags,
        value,
        path,
        -60.0,
        24.0,
        "Use a gain between -60 dB and +24 dB (0 = unchanged)",
    )
}

/// A LUFS value in `[min, -5]`; a positive value whose negation is in range gets the negated value as hint
/// (`target_lufs: 16` → `target_lufs: -16`).
fn lufs_in(
    diags: &mut Vec<Diagnostic>,
    value: Option<f64>,
    path: &str,
    min: f64,
    hint: &str,
) -> Option<f64> {
    let hint = match value {
        Some(v) if v.is_finite() && (5.0..=-min).contains(&v) => format!(
            "Loudness values are negative: use `{}: -{}`",
            field_name(path),
            fmt_num(v)
        ),
        _ => hint.to_string(),
    };
    range(diags, value, path, min, -5.0, &hint)
}

/// A source-normalization LUFS target (`mix.reference_lufs`, `normalize.target_lufs`) in [-70, -5].
pub(super) fn lufs(diags: &mut Vec<Diagnostic>, value: Option<f64>, path: &str) -> Option<f64> {
    lufs_in(
        diags,
        value,
        path,
        -70.0,
        "Use an integrated loudness between -70 and -5 LUFS, e.g. -16 (podcast) or -23 (EBU R128)",
    )
}

/// The master loudness target (`output.target_lufs`) in [-60, -5]: quieter targets approach the -70 LUFS
/// absolute gate of BS.1770, below which integrated loudness is undefined and cannot be verified.
pub(super) fn master_lufs(
    diags: &mut Vec<Diagnostic>,
    value: Option<f64>,
    path: &str,
) -> Option<f64> {
    lufs_in(
        diags,
        value,
        path,
        -60.0,
        "Use a master loudness between -60 and -5 LUFS, e.g. -16 (podcast), -14 (streaming) or -23 (EBU R128); \
         targets near -70 LUFS fall under the BS.1770 absolute gate, where loudness cannot be measured",
    )
}

/// Did-you-mean for values (enum names, ids, provider and speaker names): [`diagnostics::did_you_mean`],
/// then — for short words where Jaro-Winkler is too strict (`vvt` → `vtt`) — a unique candidate within
/// Damerau-Levenshtein distance 1 (2 for inputs of 6+ characters).
pub(super) fn suggest<'c>(
    input: &str,
    candidates: impl IntoIterator<Item = &'c str>,
) -> Option<&'c str> {
    let candidates: Vec<&'c str> = candidates.into_iter().collect();
    if let Some(s) = diagnostics::did_you_mean(input, candidates.iter().copied()) {
        return Some(s);
    }
    let typed = input.trim().to_ascii_lowercase();
    if typed.chars().count() < 3 {
        return None;
    }
    let limit = if typed.chars().count() >= 6 { 2 } else { 1 };
    let mut best: Option<(&'c str, usize)> = None;
    let mut tie = false;
    for c in candidates {
        let d = strsim::damerau_levenshtein(&typed, &c.to_ascii_lowercase());
        if d > limit {
            continue;
        }
        match best {
            Some((_, bd)) if d > bd => {}
            Some((_, bd)) if d == bd => tie = true,
            _ => {
                best = Some((c, d));
                tie = false;
            }
        }
    }
    if tie { None } else { best.map(|(c, _)| c) }
}

/// Parse an enum name (canonical name or alias); reports `invalid_value` with did-you-mean otherwise.
pub(super) fn enum_name<E>(
    diags: &mut Vec<Diagnostic>,
    value: Option<&str>,
    path: &str,
    parse: fn(&str) -> Option<E>,
    names: &'static [&'static str],
    what: &str,
) -> Option<E> {
    let raw = value?;
    if let Some(v) = parse(raw) {
        return Some(v);
    }
    let field = field_name(path);
    let valid = diagnostics::list_names(names.iter().copied(), names.len());
    let hint = match suggest(raw, names.iter().copied()) {
        Some(s) => format!("Did you mean `{s}`? Valid values: {valid}"),
        None => format!("Use one of {valid}, e.g. `{field}: {}`", names[0]),
    };
    diags.push(
        Diagnostic::error(codes::INVALID_VALUE, format!("unknown {what} `{raw}`"))
            .with_path(path)
            .with_hint(hint),
    );
    None
}

/// Does `id` match `^[A-Za-z0-9_][A-Za-z0-9_.:-]*$`?
pub(super) fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// A valid id derived from an invalid one (for hints): invalid characters become `_`.
pub(super) fn sanitize_id(id: &str) -> String {
    let mut out: String = id
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("item");
    }
    if !out.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::NumOrString;

    #[test]
    fn ids() {
        for ok in ["a", "cue_01", "voice.cue1", "a:b-c", "_x", "9"] {
            assert!(is_valid_id(ok), "{ok}");
        }
        for bad in ["", " a", "my cue", "-a", ".a", "é", "a/b"] {
            assert!(!is_valid_id(bad), "{bad}");
        }
        assert_eq!(sanitize_id("my cue"), "my_cue");
        assert_eq!(sanitize_id("-a"), "_-a");
        assert!(is_valid_id(&sanitize_id("über cool")));
    }

    #[test]
    fn millis_rules() {
        let mut d = Vec::new();
        let m = |v: f64| Millis::ms(v);
        assert_eq!(
            millis(&mut d, Some(&m(-5.0)), "a.offset_ms", Sign::Any),
            Some(-5.0)
        );
        assert_eq!(
            millis(&mut d, Some(&m(-5.0)), "a.fade_in_ms", Sign::NonNegative),
            None
        );
        assert_eq!(
            millis(&mut d, Some(&m(0.0)), "a.duration_ms", Sign::Positive),
            None
        );
        let s = Millis(NumOrString::Str("1.5 parsecs".into()));
        assert_eq!(
            millis(&mut d, Some(&s), "a.tail_ms", Sign::NonNegative),
            None
        );
        assert_eq!(d.len(), 3);
        assert!(
            d.iter()
                .all(|x| x.code == "invalid_value" && x.hint.is_some())
        );
        assert_eq!(fmt_ms(1500.0), "1500");
        assert_eq!(fmt_ms(0.5), "0.5");
        assert_eq!(fmt_num(1e18), "1e18");
        assert_eq!(fmt_num(-1e300), "-1e300");
    }

    #[test]
    fn millis_limit_is_24_hours() {
        let mut d = Vec::new();
        let m = |v: f64| Millis::ms(v);
        let day = crate::MAX_TIME_MS;
        assert_eq!(
            millis(&mut d, Some(&m(day)), "a.duration_ms", Sign::Positive),
            Some(day)
        );
        assert_eq!(
            millis(&mut d, Some(&m(-day)), "a.offset_ms", Sign::Any),
            Some(-day)
        );
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(
            millis(&mut d, Some(&m(1e12)), "a.start_at_ms", Sign::NonNegative),
            None
        );
        assert_eq!(
            d[0].message,
            "`start_at_ms` is 1000000000000 ms (31.7 years); the maximum is 24 h (86400000 ms)"
        );
        let hint = d[0].hint.as_deref().unwrap_or_default();
        assert!(hint.contains("epoch milliseconds") && hint.contains("`start_at_ms: \"1.5s\"`"));
        assert_eq!(
            millis(&mut d, Some(&m(-day - 1.0)), "a.offset_ms", Sign::Any),
            None
        );
        assert!(
            d[1].message
                .contains("offsets must be between -24 h and 24 h")
        );
        let s = Millis(NumOrString::Str("25h".into()));
        assert_eq!(
            millis(&mut d, Some(&s), "a.tail_ms", Sign::NonNegative),
            None
        );
        assert!(d[2].message.contains("(25.0 h)"), "{}", d[2].message);
        // Sign errors come first and are the only diagnostic.
        assert_eq!(
            millis(&mut d, Some(&m(-1e12)), "a.fade_in_ms", Sign::NonNegative),
            None
        );
        assert_eq!(d.len(), 4);
        assert!(d[3].message.contains("must not be negative"));
    }

    #[test]
    fn bare_small_numbers_look_like_seconds() {
        let num = |v: f64| Millis::ms(v);
        let text = |s: &str| Millis(NumOrString::Str(s.into()));
        assert!(looks_like_seconds(&num(30.0), 30.0, TimeUse::Length));
        assert!(!looks_like_seconds(&num(100.0), 100.0, TimeUse::Length));
        assert!(looks_like_seconds(&text("30"), 30.0, TimeUse::Length));
        assert!(!looks_like_seconds(&text("30ms"), 30.0, TimeUse::Length));
        assert!(!looks_like_seconds(
            &text("0:30"),
            30_000.0,
            TimeUse::Length
        ));
        assert!(looks_like_seconds(&num(2.0), 2.0, TimeUse::Fade));
        assert!(!looks_like_seconds(&num(0.0), 0.0, TimeUse::Fade));
        assert!(!looks_like_seconds(&num(10.0), 10.0, TimeUse::Fade));
        assert!(looks_like_seconds(&num(1.5), 1.5, TimeUse::Pause));
        assert!(looks_like_seconds(&num(2.5), 2.5, TimeUse::Start));
        assert!(looks_like_seconds(&num(-1.5), -1.5, TimeUse::Start));
        assert!(!looks_like_seconds(&num(2.0), 2.0, TimeUse::Start));
        assert!(!looks_like_seconds(&num(12.5), 12.5, TimeUse::Start));
        assert!(!looks_like_seconds(&text("2.5s"), 2500.0, TimeUse::Start));

        let mut d = Vec::new();
        seconds_as_ms(
            &mut d,
            Some(&num(30.0)),
            30.0,
            "tracks[0].duration_ms",
            TimeUse::Length,
        );
        assert_eq!(d[0].code, "suspicious_value");
        assert_eq!(d[0].severity, crate::Severity::Warning);
        assert_eq!(d[0].message, "`duration_ms: 30` is 30 milliseconds");
        assert_eq!(
            d[0].hint.as_deref(),
            Some("Times are milliseconds: write `duration_ms: \"30s\"` if you meant 30 seconds")
        );
    }

    #[test]
    fn lufs_ranges() {
        let mut d = Vec::new();
        assert_eq!(
            master_lufs(&mut d, Some(-60.0), "output.target_lufs"),
            Some(-60.0)
        );
        assert_eq!(master_lufs(&mut d, Some(-70.0), "output.target_lufs"), None);
        assert!(d[0].message.contains("between -60 and -5"));
        assert!(
            d[0].hint
                .as_deref()
                .unwrap_or_default()
                .contains("absolute gate")
        );
        assert_eq!(master_lufs(&mut d, Some(16.0), "output.target_lufs"), None);
        assert_eq!(
            d[1].hint.as_deref(),
            Some("Loudness values are negative: use `target_lufs: -16`")
        );
        assert_eq!(lufs(&mut d, Some(-65.0), "mix.reference_lufs"), Some(-65.0));
        assert_eq!(d.len(), 2);
    }
}
