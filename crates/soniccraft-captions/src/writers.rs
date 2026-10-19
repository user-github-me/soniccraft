//! WebVTT and SRT writers (DESIGN §6).
//!
//! Output is byte-stable: UTF-8 without BOM, `\n` line endings, one blank line between cues, a final
//! newline. Timestamps are rounded to the nearest millisecond (half away from zero); negative and
//! non-finite times become 0; hours use at least two digits and grow as needed (`100:00:00.000`). A cue
//! whose rounded end is before its start ends at its start.

use crate::model::Caption;

/// Render captions as a WebVTT file, in slice order.
///
/// Speaker labels ([`Caption::label`]) become a voice span `<v Label>` at the start of the payload.
/// `&`, `<` and `>` are escaped in labels and text (which also keeps `-->` out of payloads).
pub fn to_webvtt(captions: &[Caption]) -> String {
    let mut out = String::from("WEBVTT\n");
    for caption in captions {
        let lines = payload_lines(caption);
        if lines.is_empty() {
            continue;
        }
        let (start, end) = rounded_bounds(caption);
        out.push('\n');
        out.push_str(&format_timestamp(start, '.'));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(end, '.'));
        out.push('\n');
        for (i, line) in lines.iter().enumerate() {
            if i == 0
                && let Some(label) = clean(caption.label.as_deref())
            {
                out.push_str("<v ");
                out.push_str(&escape_vtt(&label));
                out.push('>');
            }
            out.push_str(&escape_vtt(line));
            out.push('\n');
        }
    }
    out
}

/// Render captions as an SRT file, in slice order, numbered from 1.
///
/// Speaker labels ([`Caption::label`]) are prefixed to the first line as `Label: `. Text is written as
/// is (SRT has no escaping).
pub fn to_srt(captions: &[Caption]) -> String {
    let mut out = String::new();
    let mut number = 0usize;
    for caption in captions {
        let lines = payload_lines(caption);
        if lines.is_empty() {
            continue;
        }
        number += 1;
        let (start, end) = rounded_bounds(caption);
        if number > 1 {
            out.push('\n');
        }
        out.push_str(&number.to_string());
        out.push('\n');
        out.push_str(&format_timestamp(start, ','));
        out.push_str(" --> ");
        out.push_str(&format_timestamp(end, ','));
        out.push('\n');
        for (i, line) in lines.iter().enumerate() {
            if i == 0
                && let Some(label) = clean(caption.label.as_deref())
            {
                out.push_str(&label);
                out.push_str(": ");
            }
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A WebVTT timestamp `HH:MM:SS.mmm` for a time in milliseconds.
pub fn vtt_timestamp(ms: f64) -> String {
    format_timestamp(round_ms(ms), '.')
}

/// An SRT timestamp `HH:MM:SS,mmm` for a time in milliseconds.
pub fn srt_timestamp(ms: f64) -> String {
    format_timestamp(round_ms(ms), ',')
}

/// Round to whole milliseconds: nearest, half away from zero; negative or non-finite → 0; saturating.
pub(crate) fn round_ms(ms: f64) -> u64 {
    if ms.is_finite() && ms > 0.0 {
        // Float-to-integer casts saturate, so absurdly large times cannot wrap.
        ms.round() as u64
    } else {
        0
    }
}

fn rounded_bounds(caption: &Caption) -> (u64, u64) {
    let start = round_ms(caption.start_ms);
    (start, round_ms(caption.end_ms).max(start))
}

fn format_timestamp(total_ms: u64, separator: char) -> String {
    let hours = total_ms / 3_600_000;
    let minutes = total_ms / 60_000 % 60;
    let seconds = total_ms / 1000 % 60;
    let millis = total_ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}{separator}{millis:03}")
}

/// The caption's non-empty lines (falling back to its text), split at embedded line breaks so a payload
/// never contains a blank line.
fn payload_lines(caption: &Caption) -> Vec<String> {
    let source: Vec<&str> = if caption.lines.iter().any(|l| !l.trim().is_empty()) {
        caption.lines.iter().map(String::as_str).collect()
    } else {
        vec![caption.text.as_str()]
    };
    source
        .into_iter()
        .flat_map(|l| l.split(['\n', '\r']))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Labels are single-line and trimmed; empty labels are dropped.
fn clean(label: Option<&str>) -> Option<String> {
    let label = label?.split_whitespace().collect::<Vec<_>>().join(" ");
    (!label.is_empty()).then_some(label)
}

fn escape_vtt(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Word;

    fn caption(start: f64, end: f64, lines: &[&str], label: Option<&str>) -> Caption {
        Caption {
            index: 0,
            cue_id: "c".into(),
            track_id: "t".into(),
            speaker: label.map(String::from),
            label: label.map(String::from),
            start_ms: start,
            end_ms: end,
            text: lines.join(" "),
            lines: lines.iter().map(|l| l.to_string()).collect(),
            words: vec![Word::new(lines.join(" "), start, end)],
            cue_start_ms: start,
            cue_order: 0,
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(vtt_timestamp(0.0), "00:00:00.000");
        assert_eq!(vtt_timestamp(2480.0), "00:00:02.480");
        assert_eq!(vtt_timestamp(1234.4999), "00:00:01.234");
        assert_eq!(vtt_timestamp(1234.5), "00:00:01.235");
        assert_eq!(srt_timestamp(3_723_004.0), "01:02:03,004");
        assert_eq!(vtt_timestamp(100.0 * 3_600_000.0 + 1.0), "100:00:00.001");
        assert_eq!(vtt_timestamp(-5.0), "00:00:00.000");
        assert_eq!(vtt_timestamp(f64::NAN), "00:00:00.000");
        assert_eq!(srt_timestamp(59_999.6), "00:01:00,000");
    }

    #[test]
    fn webvtt_layout_and_escaping() {
        let captions = [
            caption(
                0.0,
                2480.0,
                &["Autonomous software", "development"],
                Some("Narrator"),
            ),
            caption(2500.0, 3000.4, &["R&D <beta> --> done"], Some("A<B>&C")),
            caption(3100.0, 3900.0, &["plain"], None),
        ];
        let expected = "WEBVTT\n\n00:00:00.000 --> 00:00:02.480\n<v Narrator>Autonomous software\ndevelopment\n\n00:00:02.500 --> 00:00:03.000\n<v A&lt;B&gt;&amp;C>R&amp;D &lt;beta&gt; --&gt; done\n\n00:00:03.100 --> 00:00:03.900\nplain\n";
        assert_eq!(to_webvtt(&captions), expected);
        assert_eq!(to_webvtt(&[]), "WEBVTT\n");
    }

    #[test]
    fn srt_layout_and_numbering() {
        let captions = [
            caption(
                0.0,
                2480.0,
                &["Autonomous software", "development"],
                Some("Narrator"),
            ),
            caption(2500.0, 3000.0, &["", "  "], None),
            caption(3100.0, 3900.0, &["R&D <ok>"], None),
        ];
        // The second caption has no text and is skipped; numbering stays contiguous.
        let expected = "1\n00:00:00,000 --> 00:00:02,480\nNarrator: Autonomous software\ndevelopment\n\n2\n00:00:03,100 --> 00:00:03,900\nR&D <ok>\n";
        assert_eq!(to_srt(&captions), expected);
        assert_eq!(to_srt(&[]), "");
    }

    #[test]
    fn hand_built_captions_are_sanitized() {
        let mut c = caption(
            1000.0,
            900.0,
            &["two\n\nlines"],
            Some(" Multi\nLine  Label "),
        );
        c.lines = vec!["two\n\nlines".into()];
        assert_eq!(
            to_srt(&[c.clone()]),
            "1\n00:00:01,000 --> 00:00:01,000\nMulti Line Label: two\nlines\n"
        );
        c.lines.clear();
        c.text = "fallback".into();
        assert!(to_webvtt(&[c]).ends_with("<v Multi Line Label>fallback\n"));
    }
}
