//! Small formatting helpers shared by the commands: durations, sizes, plurals, display widths, aligned
//! tables and path display.

use std::path::{Path, PathBuf};

use crate::term::{Style, paint};

/// `1 error`, `2 errors`.
pub(crate) fn plural(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// A duration in milliseconds for humans: `0.85 s`, `24.51 s`, `1:02.35` (minutes from 60 s on).
pub(crate) fn duration_ms(ms: f64) -> String {
    if !ms.is_finite() {
        return "n/a".to_string();
    }
    let ms = ms.max(0.0);
    if ms < 60_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        // Round to centiseconds first so that 59.996 s does not print as 0:60.00.
        let centis = (ms / 10.0).round() as u64;
        let minutes = centis / 6000;
        let rest = centis % 6000;
        format!("{minutes}:{:02}.{:02}", rest / 100, rest % 100)
    }
}

/// A timeline position in milliseconds: `0:04.31` (the layout diagram's notation).
pub(crate) fn timestamp_ms(ms: f64) -> String {
    let centis = (ms.max(0.0) / 10.0).round() as u64;
    format!(
        "{}:{:02}.{:02}",
        centis / 6000,
        (centis % 6000) / 100,
        centis % 100
    )
}

/// A byte count in decimal units: `512 B`, `12.3 kB`, `4.7 MB`.
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["kB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Signed decibel-style number with one decimal: `-16.0`, `+0.5` is printed as `0.5`.
pub(crate) fn db(value: f64) -> String {
    let v = if value.abs() < 0.05 { 0.0 } else { value };
    format!("{v:.1}")
}

/// Terminal columns taken by `c`: 0 for combining marks and other zero-width characters, 2 for wide
/// East Asian characters and emoji, 1 otherwise. (A compact approximation of UAX #11, good enough to keep
/// carets and table columns aligned.)
pub(crate) fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    if matches!(cp,
        0x0300..=0x036F   // combining diacritical marks
        | 0x200B..=0x200F // zero-width space/joiners, direction marks
        | 0x20D0..=0x20FF // combining marks for symbols
        | 0xFE00..=0xFE0F // variation selectors
        | 0xFE20..=0xFE2F // combining half marks
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
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD // CJK extensions B..
    ) {
        return 2;
    }
    1
}

/// Terminal columns taken by `s` (see [`char_width`]); tabs count as 4 and ANSI escape sequences as 0.
pub(crate) fn width(s: &str) -> usize {
    let mut cols = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            in_escape = !c.is_ascii_alphabetic();
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            cols += if c == '\t' { 4 } else { char_width(c) };
        }
    }
    cols
}

/// Pad `s` with spaces to `cols` columns (no truncation).
pub(crate) fn pad(s: &str, cols: usize) -> String {
    let w = width(s);
    let mut out = s.to_string();
    out.extend(std::iter::repeat_n(' ', cols.saturating_sub(w)));
    out
}

/// Column alignment for [`table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Align {
    Left,
    Right,
}

/// An aligned plain-text table: a bold header row, then one row per entry, columns separated by two
/// spaces. The last column is not padded, so long notes simply run on.
pub(crate) fn table(color: bool, headers: &[(&str, Align)], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|(h, _)| width(h)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(width(cell));
        }
    }
    let render_row = |cells: Vec<&str>, header: bool| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let last = i + 1 == cols;
            let w = width(cell);
            let fill = widths[i].saturating_sub(w);
            let text = if header {
                paint(color, Style::Bold, cell)
            } else {
                (*cell).to_string()
            };
            match headers[i].1 {
                Align::Right => {
                    line.extend(std::iter::repeat_n(' ', fill));
                    line.push_str(&text);
                }
                Align::Left => {
                    line.push_str(&text);
                    if !last {
                        line.extend(std::iter::repeat_n(' ', fill));
                    }
                }
            }
            if !last {
                line.push_str("  ");
            }
        }
        let trimmed = line.trim_end().len();
        line.truncate(trimmed);
        line.push('\n');
        line
    };
    let mut out = render_row(headers.iter().map(|(h, _)| *h).collect(), true);
    for row in rows {
        let cells: Vec<&str> = (0..cols)
            .map(|i| row.get(i).map(String::as_str).unwrap_or(""))
            .collect();
        out.push_str(&render_row(cells, false));
    }
    out
}

/// `path` relative to `cwd` when it lies inside it, else as given (absolute paths stay absolute).
pub(crate) fn show_path(path: &Path, cwd: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        Ok(_) => ".".to_string(),
        Err(_) => path.display().to_string(),
    }
}

/// `path` made absolute against `cwd` (no symlink resolution, no filesystem access).
pub(crate) fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Quote `s` for a POSIX shell when it contains anything but safe characters.
pub(crate) fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | '.' | '/' | '+' | ':' | '@' | '%' | ',')
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration_ms(850.0), "0.85 s");
        assert_eq!(duration_ms(24_512.0), "24.51 s");
        assert_eq!(duration_ms(62_345.0), "1:02.35");
        assert_eq!(duration_ms(119_996.0), "2:00.00");
        assert_eq!(duration_ms(f64::NAN), "n/a");
        assert_eq!(timestamp_ms(4_310.0), "0:04.31");
        assert_eq!(timestamp_ms(61_000.0), "1:01.00");
    }

    #[test]
    fn sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(12_345), "12.3 kB");
        assert_eq!(bytes(1_536_044), "1.5 MB");
        assert_eq!(bytes(999_999), "1.0 MB");
        assert_eq!(bytes(4_700_000_000), "4.7 GB");
    }

    #[test]
    fn plurals_and_db() {
        assert_eq!(plural(1, "error", "errors"), "1 error");
        assert_eq!(plural(0, "error", "errors"), "0 errors");
        assert_eq!(db(-16.04), "-16.0");
        assert_eq!(db(-0.01), "0.0");
    }

    #[test]
    fn widths() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("e\u{301}"), 1);
        assert_eq!(width("\t"), 4);
        assert_eq!(width("\x1b[1;32myes\x1b[0m"), 3);
        assert_eq!(pad("ab", 4), "ab  ");
    }

    #[test]
    fn tables_align() {
        let t = table(
            false,
            &[
                ("NAME", Align::Left),
                ("N", Align::Right),
                ("NOTE", Align::Left),
            ],
            &[
                vec!["mock".into(), "8".into(), "ready".into()],
                vec!["elevenlabs".into(), "2".into(), "".into()],
            ],
        );
        assert_eq!(
            t,
            "NAME        N  NOTE\nmock        8  ready\nelevenlabs  2\n"
        );
    }

    #[test]
    fn paths() {
        let cwd = Path::new("/work/proj");
        assert_eq!(
            show_path(Path::new("/work/proj/out/a.wav"), cwd),
            "out/a.wav"
        );
        assert_eq!(
            show_path(Path::new("/elsewhere/a.wav"), cwd),
            "/elsewhere/a.wav"
        );
        assert_eq!(show_path(Path::new("/work/proj"), cwd), ".");
        assert_eq!(
            absolute(Path::new("a.wav"), cwd),
            PathBuf::from("/work/proj/a.wav")
        );
        assert_eq!(shell_quote("demo"), "demo");
        assert_eq!(shell_quote("my demo"), "'my demo'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
