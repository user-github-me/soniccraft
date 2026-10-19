//! Deterministic text helpers shared by diagnostics and the plan diagram.
//!
//! Times are formatted from integer frames with integer arithmetic (round half away from zero), so the
//! output never depends on floating-point formatting of intermediate values.

/// `round(frames × scale / sample_rate)`, halves away from zero; 0 when the sample rate is 0.
fn scaled_round(frames: i64, scale: i64, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    let num = i128::from(frames) * i128::from(scale);
    let den = i128::from(sample_rate);
    let magnitude = (num.abs() * 2 + den) / (2 * den);
    let signed = if num < 0 { -magnitude } else { magnitude };
    i64::try_from(signed).unwrap_or(if signed < 0 { i64::MIN } else { i64::MAX })
}

/// Seconds with two decimals, e.g. `2.35`, `-0.20`, `0.00`.
pub(crate) fn secs(frames: i64, sample_rate: u32) -> String {
    let centis = scaled_round(frames, 100, sample_rate);
    let sign = if centis < 0 { "-" } else { "" };
    let abs = centis.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// A half-open interval in seconds, e.g. `2.15-4.80 s`.
pub(crate) fn span(start: i64, end: i64, sample_rate: u32) -> String {
    format!("{}-{} s", secs(start, sample_rate), secs(end, sample_rate))
}

/// Clock time `m:ss` with `decimals` fractional digits of seconds (`0:12.4` for 1, `1:05.25` for 2).
/// Minutes are not wrapped into hours.
pub(crate) fn clock(frames: i64, sample_rate: u32, decimals: u32) -> String {
    let scale = 10_i64.pow(decimals.min(6));
    let units = scaled_round(frames, scale, sample_rate);
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    let scale_u = scale.unsigned_abs();
    let per_minute = 60 * scale_u;
    let minutes = abs / per_minute;
    let rest = abs % per_minute;
    let seconds = rest / scale_u;
    let fraction = rest % scale_u;
    if decimals == 0 {
        format!("{sign}{minutes}:{seconds:02}")
    } else {
        let width = decimals.min(6) as usize;
        format!("{sign}{minutes}:{seconds:02}.{fraction:0width$}")
    }
}

/// Smallest whole number of milliseconds that is at least `frames` (ceiling).
pub(crate) fn ceil_ms(frames: i64, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    let num = i128::from(frames) * 1000;
    let den = i128::from(sample_rate);
    let mut q = num.div_euclid(den);
    if num.rem_euclid(den) != 0 {
        q += 1;
    }
    i64::try_from(q).unwrap_or(if q < 0 { i64::MIN } else { i64::MAX })
}

/// Largest whole number of milliseconds that is at most `frames` (floor).
pub(crate) fn floor_ms(frames: i64, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    let q = (i128::from(frames) * 1000).div_euclid(i128::from(sample_rate));
    i64::try_from(q).unwrap_or(if q < 0 { i64::MIN } else { i64::MAX })
}

/// A non-negative length in human units, for messages about very long timelines: `59.50 s`, `25 h 3 min`,
/// `3.5 days`, `31.7 years` (a year is 365.25 days). Integer arithmetic, rounded down for hours and minutes
/// and to the nearest tenth for days and years.
pub(crate) fn human_length(frames: i64, sample_rate: u32) -> String {
    if sample_rate == 0 || frames <= 0 {
        return "0.00 s".to_string();
    }
    let f = i128::from(frames);
    let sr = i128::from(sample_rate);
    let hour = 3600 * sr;
    let day = 24 * hour;
    if f < hour {
        return format!("{} s", secs(frames, sample_rate));
    }
    if f < 2 * day {
        let minutes = f / (60 * sr);
        return format!("{} h {} min", minutes / 60, minutes % 60);
    }
    let (unit, den) = if f < 365 * day {
        ("days", day)
    } else {
        // 365.25 days: tenths = f × 10 × 4 / (1461 days).
        ("years", 1461 * day / 4)
    };
    let tenths = (f * 20 + den) / (2 * den);
    format!("{}.{} {unit}", tenths / 10, tenths % 10)
}

/// A millisecond value as an agent would write it in YAML: `350`, `-200`, `12.5` (at most 3 decimals).
pub(crate) fn ms_num(ms: f64) -> String {
    if !ms.is_finite() {
        return "0".to_string();
    }
    let rounded = (ms * 1000.0).round() / 1000.0;
    if rounded.fract() == 0.0 && rounded.abs() < 1e15 {
        // `as` is exact here: the value is integral and well inside the i64 range. `-0.0` becomes `0`.
        return format!("{}", rounded as i64);
    }
    let text = format!("{rounded:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Replace control characters and whitespace runs with single spaces and trim the ends.
pub(crate) fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Terminal columns taken by `c`: 0 for combining marks and other zero-width characters, 2 for wide East
/// Asian characters and emoji, 1 otherwise. A compact approximation of UAX #11 (the same table as the CLI's,
/// plus a few emoji and zero-width blocks), good enough to keep the diagram's columns aligned.
pub(crate) fn char_width(c: char) -> usize {
    let cp = u32::from(c);
    if matches!(cp,
        0x0000
        | 0x0300..=0x036F   // combining diacritical marks
        | 0x1AB0..=0x1AFF   // combining diacritical marks extended
        | 0x1DC0..=0x1DFF   // combining diacritical marks supplement
        | 0x200B..=0x200F   // zero-width space/joiners, direction marks
        | 0x2060..=0x2064   // word joiner, invisible operators
        | 0x20D0..=0x20FF   // combining marks for symbols
        | 0xFE00..=0xFE0F   // variation selectors
        | 0xFE20..=0xFE2F   // combining half marks
        | 0xFEFF            // zero-width no-break space
        | 0xE0100..=0xE01EF // variation selectors supplement
    ) {
        return 0;
    }
    if matches!(cp,
        0x1100..=0x115F     // Hangul Jamo
        | 0x2E80..=0x303E   // CJK radicals, punctuation
        | 0x3041..=0x33FF   // kana, CJK symbols
        | 0x3400..=0x4DBF   // CJK extension A
        | 0x4E00..=0x9FFF   // CJK unified ideographs
        | 0xA000..=0xA4CF   // Yi
        | 0xAC00..=0xD7A3   // Hangul syllables
        | 0xF900..=0xFAFF   // CJK compatibility ideographs
        | 0xFE30..=0xFE4F   // CJK compatibility forms
        | 0xFF00..=0xFF60   // fullwidth forms
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F // emoji
        | 0x1F680..=0x1F6FF // transport and map symbols
        | 0x1F900..=0x1F9FF // supplemental symbols and pictographs
        | 0x1FA70..=0x1FAFF // symbols and pictographs extended-A
        | 0x20000..=0x3FFFD // CJK extensions B..
    ) {
        return 2;
    }
    1
}

/// Terminal columns taken by `text` (see [`char_width`]).
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// The longest prefix of `text` that fits in `max` columns (zero-width characters that follow the last kept
/// character stay attached to it).
fn head(text: &str, max: usize) -> &str {
    let mut used = 0;
    for (i, c) in text.char_indices() {
        let w = char_width(c);
        if used + w > max {
            return &text[..i];
        }
        used += w;
    }
    text
}

/// The longest suffix of `text` that fits in `max` columns and does not start with a zero-width character
/// (which would belong to the character before it).
fn tail(text: &str, max: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let w = char_width(c);
        if used + w > max {
            break;
        }
        used += w;
        if w > 0 {
            start = i;
        }
    }
    &text[start..]
}

/// Truncate to at most `max` terminal columns, ending in `...` when something was cut.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max <= 3 {
        return head(text, max).to_string();
    }
    let mut out = head(text, max - 3).to_string();
    out.push_str("...");
    out
}

/// Shorten to at most `max` terminal columns by cutting out the middle (`voic..._left`), so that ids that
/// share a long prefix stay distinguishable. Text that fits is returned unchanged.
pub(crate) fn abbreviate(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_string();
    }
    if max <= 3 {
        return head(text, max).to_string();
    }
    let room = max - 3;
    let front = room / 2;
    let start = head(text, front);
    let end = tail(&text[start.len()..], room - display_width(start));
    format!("{start}...{end}")
}

/// Left-align `text` in a field of `width` terminal columns (no truncation).
pub(crate) fn pad(text: &str, width: usize) -> String {
    let used = display_width(text);
    let mut out = String::with_capacity(text.len() + width.saturating_sub(used));
    out.push_str(text);
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

/// Labels of at most `max` terminal columns for `texts` that stay distinct where the texts are distinct:
/// texts that fit are kept; longer ones are cut at the end (`voice_pri...`); when that makes two labels equal,
/// their middle is cut instead (`voic..._left`); labels that still collide get a `~<n>` marker (n = 1-based
/// position among the colliding texts). Deterministic.
pub(crate) fn unique_labels(texts: &[String], max: usize) -> Vec<String> {
    use std::collections::HashMap;
    fn collisions(labels: &[String]) -> HashMap<&str, usize> {
        let mut seen: HashMap<&str, usize> = HashMap::new();
        for label in labels {
            *seen.entry(label.as_str()).or_default() += 1;
        }
        seen
    }
    let fits: Vec<bool> = texts.iter().map(|t| display_width(t) <= max).collect();
    let mut labels: Vec<String> = texts.iter().map(|t| truncate(t, max)).collect();
    for round in 0..2 {
        let counts = collisions(&labels);
        let clash: Vec<bool> = labels
            .iter()
            .zip(&fits)
            .map(|(l, &fit)| !fit && counts.get(l.as_str()).copied().unwrap_or(0) > 1)
            .collect();
        if !clash.contains(&true) {
            return labels;
        }
        for (i, clashes) in clash.into_iter().enumerate() {
            if !clashes {
                continue;
            }
            labels[i] = if round == 0 {
                abbreviate(&texts[i], max)
            } else {
                String::new()
            };
        }
        if round == 1 {
            // Number the texts that still clash, in order, and keep as much of their start as fits.
            let mut next = 0;
            for (i, label) in labels.iter_mut().enumerate() {
                if !label.is_empty() || fits[i] {
                    continue;
                }
                next += 1;
                let marker = format!("~{next}");
                let room = max.saturating_sub(marker.len());
                *label = format!("{}{marker}", head(&texts[i], room));
            }
        }
    }
    labels
}

/// `1 cue`, `2 cues`.
pub(crate) fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_round_half_away_from_zero() {
        assert_eq!(secs(0, 48_000), "0.00");
        assert_eq!(secs(112_800, 48_000), "2.35");
        // 2.345 s exactly → 2.35 (no binary floating-point surprise).
        assert_eq!(secs(2345, 1000), "2.35");
        assert_eq!(secs(-2345, 1000), "-2.35");
        assert_eq!(secs(-200, 1000), "-0.20");
        assert_eq!(secs(5, 1000), "0.01");
        assert_eq!(secs(4, 1000), "0.00");
        assert_eq!(secs(123, 0), "0.00");
        assert_eq!(span(2150, 4800, 1000), "2.15-4.80 s");
    }

    #[test]
    fn clock_forms() {
        assert_eq!(clock(12_350, 1000, 1), "0:12.4");
        assert_eq!(clock(12_340, 1000, 1), "0:12.3");
        assert_eq!(clock(65_000, 1000, 1), "1:05.0");
        assert_eq!(clock(59_960, 1000, 1), "1:00.0");
        assert_eq!(clock(4_525, 1000, 2), "0:04.53");
        assert_eq!(clock(3_600_000 * 2, 1000, 0), "120:00");
        assert_eq!(clock(0, 48_000, 2), "0:00.00");
    }

    #[test]
    fn human_lengths() {
        assert_eq!(human_length(59_500, 1000), "59.50 s");
        assert_eq!(human_length(90_180_000, 1000), "25 h 3 min");
        assert_eq!(human_length(302_400_000, 1000), "3.5 days");
        // 10^12 ms (epoch milliseconds written as a start time).
        assert_eq!(human_length(1_000_000_000_000, 1000), "31.7 years");
        assert_eq!(human_length(-5, 1000), "0.00 s");
        assert_eq!(human_length(5, 0), "0.00 s");
    }

    #[test]
    fn millisecond_rounding() {
        assert_eq!(ceil_ms(1, 48_000), 1);
        assert_eq!(ceil_ms(48, 48_000), 1);
        assert_eq!(ceil_ms(49, 48_000), 2);
        assert_eq!(ceil_ms(-49, 48_000), -1);
        assert_eq!(floor_ms(49, 48_000), 1);
        assert_eq!(floor_ms(-49, 48_000), -2);
        assert_eq!(ms_num(350.0), "350");
        assert_eq!(ms_num(-200.0), "-200");
        assert_eq!(ms_num(12.5), "12.5");
        assert_eq!(ms_num(0.1234), "0.123");
        assert_eq!(ms_num(-0.0), "0");
        assert_eq!(ms_num(f64::NAN), "0");
    }

    #[test]
    fn text_helpers() {
        assert_eq!(one_line("  a\n b\t\tc  "), "a b c");
        assert_eq!(one_line("\u{7}x"), "x");
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdefg", 6), "abc...");
        assert_eq!(truncate("héllo wörld", 8), "héllo...");
        assert_eq!(truncate("abcdef", 2), "ab");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
        assert_eq!(count(1, "cue", "cues"), "1 cue");
        assert_eq!(count(0, "cue", "cues"), "0 cues");
    }

    #[test]
    fn display_widths() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("司会者"), 6);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("🎙\u{fe0f}"), 2);
        assert_eq!(display_width("zero\u{200b}width"), 9);
        // Padding and truncation count columns, not characters.
        assert_eq!(pad("司会者", 8), "司会者  ");
        assert_eq!(display_width(&pad("司会者", 8)), 8);
        assert_eq!(truncate("日本語のテキスト", 9), "日本語...");
        assert_eq!(truncate("日本語のテキスト", 10), "日本語...");
        assert_eq!(display_width(&truncate("日本語のテキスト", 10)), 9);
        // A combining mark stays with its base character.
        assert_eq!(truncate("abe\u{301}fghij", 6), "abe\u{301}...");
    }

    #[test]
    fn abbreviations_keep_both_ends() {
        assert_eq!(abbreviate("voice_primary_left", 12), "voic..._left");
        assert_eq!(abbreviate("voice_primary_right", 12), "voic...right");
        assert_eq!(abbreviate("short", 12), "short");
        assert_eq!(display_width(&abbreviate("日本語のテキストです", 9)), 9);
        assert_eq!(abbreviate("abcdef", 3), "abc");
    }

    #[test]
    fn labels_stay_unique() {
        let ids = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Plain truncation when it is enough.
        assert_eq!(
            unique_labels(&ids(&["narrator_with_a_long_id", "muted"]), 12),
            ["narrator_...", "muted"]
        );
        // A shared prefix: the distinguishing ends are kept.
        assert_eq!(
            unique_labels(
                &ids(&[
                    "voice_primary_left",
                    "voice_primary_right",
                    "voice_primary_center"
                ]),
                12
            ),
            ["voic..._left", "voic...right", "voic...enter"]
        );
        // Texts that differ only in the middle get numbered.
        let labels = unique_labels(
            &ids(&["aaaaaaaa_1_zzzzzzzz", "aaaaaaaa_2_zzzzzzzz", "fits"]),
            12,
        );
        assert_eq!(labels, ["aaaaaaaa_1~1", "aaaaaaaa_2~2", "fits"]);
        // A truncated label never collides with an id that fits.
        let labels = unique_labels(&ids(&["abcdefghi...", "abcdefghijklmno"]), 12);
        assert_eq!(labels[0], "abcdefghi...");
        assert_ne!(labels[1], labels[0]);
        assert!(labels.iter().all(|l| display_width(l) <= 12));
    }
}
