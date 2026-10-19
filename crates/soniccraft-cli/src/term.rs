//! Terminal output: color decisions, ANSI styles and stdout/stderr writers that never panic.
//!
//! `println!` panics when stdout is a closed pipe (`soniccraft plan x.yaml | head -3`); every byte the CLI
//! prints goes through [`Term`] instead, which stops writing quietly once the stream is gone.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use clap::ValueEnum;

/// `--color` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub(crate) enum ColorChoice {
    // Plain comments, not doc comments: clap would turn those into a long per-value help list.
    // Color when the stream is a terminal, `NO_COLOR` is unset and `TERM` is not `dumb`.
    #[default]
    Auto,
    // Always emit ANSI colors.
    Always,
    // Never emit ANSI colors.
    Never,
}

impl ColorChoice {
    /// Whether to color a stream that is (or is not) a terminal.
    pub(crate) fn enabled(self, is_terminal: bool) -> bool {
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => is_terminal && env_allows_color(),
        }
    }
}

/// `NO_COLOR` (any non-empty value) and `TERM=dumb` switch automatic colors off.
fn env_allows_color() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    let dumb = std::env::var_os("TERM").is_some_and(|t| t == "dumb");
    !no_color && !dumb
}

/// The two output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    /// Whether the stream is attached to a terminal.
    pub(crate) fn is_terminal(self) -> bool {
        match self {
            Stream::Stdout => std::io::stdout().is_terminal(),
            Stream::Stderr => std::io::stderr().is_terminal(),
        }
    }
}

/// Text styles used across the CLI (rustc's palette for diagnostics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    /// Bold red: `error`, error carets.
    Error,
    /// Bold yellow: `warning`, warning carets.
    Warning,
    /// Bold cyan: `info`, info carets.
    Info,
    /// Bold green: success lines, progress verbs.
    Success,
    /// Bold blue: the `-->`/`|`/`=` gutter and line numbers.
    Gutter,
    /// Bold.
    Bold,
    /// Dimmed secondary text.
    Dim,
}

impl Style {
    fn ansi(self) -> &'static str {
        match self {
            Style::Error => "\x1b[1;31m",
            Style::Warning => "\x1b[1;33m",
            Style::Info => "\x1b[1;36m",
            Style::Success => "\x1b[1;32m",
            Style::Gutter => "\x1b[1;34m",
            Style::Bold => "\x1b[1m",
            Style::Dim => "\x1b[2m",
        }
    }
}

const RESET: &str = "\x1b[0m";

/// Wrap `text` in the ANSI codes of `style` when `color` is on.
pub(crate) fn paint(color: bool, style: Style, text: &str) -> String {
    if !color || text.is_empty() {
        text.to_string()
    } else {
        format!("{}{text}{RESET}", style.ansi())
    }
}

/// A writer for stdout or stderr that remembers whether it may color and swallows broken pipes.
#[derive(Debug)]
pub(crate) struct Term {
    stream: Stream,
    color: bool,
    terminal: bool,
    broken: AtomicBool,
}

impl Term {
    /// A writer for `stream`, coloring according to `choice`.
    pub(crate) fn new(stream: Stream, choice: ColorChoice) -> Self {
        let terminal = stream.is_terminal();
        Term {
            stream,
            color: choice.enabled(terminal),
            terminal,
            broken: AtomicBool::new(false),
        }
    }

    /// Whether ANSI colors are enabled for this stream.
    pub(crate) fn color(&self) -> bool {
        self.color
    }

    /// Whether the stream is attached to a terminal.
    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Style `text` for this stream.
    pub(crate) fn paint(&self, style: Style, text: &str) -> String {
        paint(self.color, style, text)
    }

    /// Write `text` as is. After the first failed write (closed pipe, full disk) nothing more is written.
    pub(crate) fn write(&self, text: &str) {
        if text.is_empty() || self.broken.load(Ordering::Relaxed) {
            return;
        }
        let result = match self.stream {
            Stream::Stdout => {
                let mut out = std::io::stdout().lock();
                out.write_all(text.as_bytes()).and_then(|()| out.flush())
            }
            Stream::Stderr => std::io::stderr().lock().write_all(text.as_bytes()),
        };
        if result.is_err() {
            self.broken.store(true, Ordering::Relaxed);
        }
    }

    /// Write `text` followed by a newline.
    pub(crate) fn line(&self, text: &str) {
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(text);
        s.push('\n');
        self.write(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_choices_ignore_the_terminal() {
        assert!(ColorChoice::Always.enabled(false));
        assert!(!ColorChoice::Never.enabled(true));
        assert!(!ColorChoice::Auto.enabled(false));
    }

    #[test]
    fn paint_is_plain_without_color() {
        assert_eq!(paint(false, Style::Error, "error"), "error");
        assert_eq!(paint(true, Style::Error, "error"), "\x1b[1;31merror\x1b[0m");
        assert_eq!(paint(true, Style::Bold, ""), "");
    }
}
