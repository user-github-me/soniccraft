//! Render progress on stderr: one cargo-style line per pipeline stage, and a synthesis counter
//! (`Synthesized 3/7 (2 cached)`) that is updated in place on a terminal.
//!
//! The engine calls [`Progress::handle`] from its worker threads, so the state lives behind a mutex. The
//! display is plain text plus optional colors (no cursor control beyond `\r`), so it degrades gracefully
//! on any terminal.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use soniccraft_engine::{ProgressCallback, ProgressEvent};

use crate::format::width;
use crate::term::{Style, paint};

/// Width of the right-aligned stage verb column (cargo uses 12).
const VERB_WIDTH: usize = 12;

#[derive(Debug, Default)]
struct State {
    /// A counter line is open (not yet terminated by a newline).
    live: bool,
    /// Characters of the open line, to blank it out when it gets shorter.
    live_len: usize,
    /// Cues served from the cache so far.
    cached: usize,
}

/// The progress printer.
#[derive(Debug)]
pub(crate) struct Progress {
    color: bool,
    /// The current directory: absolute paths below it are shown relative in stage details.
    cwd: String,
    /// Update the counter in place (`\r`); otherwise every update is its own line.
    in_place: bool,
    state: Mutex<State>,
}

impl Progress {
    /// A printer for stderr. `in_place` should be true only for an interactive terminal without
    /// interleaved log output.
    pub(crate) fn new(color: bool, in_place: bool, cwd: &Path) -> Arc<Self> {
        Arc::new(Progress {
            color,
            cwd: cwd.to_string_lossy().into_owned(),
            in_place,
            state: Mutex::new(State::default()),
        })
    }

    /// The engine callback.
    pub(crate) fn callback(self: &Arc<Self>) -> ProgressCallback {
        let this = Arc::clone(self);
        Arc::new(move |event: &ProgressEvent| this.handle(event))
    }

    /// Display one engine event.
    pub(crate) fn handle(&self, event: &ProgressEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            ProgressEvent::Stage { stage, detail } => {
                let detail = relative_detail(detail, &self.cwd);
                let line = format!("{} {detail}", self.verb(&stage_verb(stage)));
                let line = line.trim_end();
                self.finish_line(&mut state);
                if self.in_place && stage == "synthesize" {
                    // The synthesis counter overwrites this line in place.
                    write_err(line);
                    state.live = true;
                    state.live_len = width(line);
                } else {
                    write_err(&format!("{line}\n"));
                }
            }
            ProgressEvent::Synthesized {
                cached,
                done,
                total,
                ..
            } => {
                if *cached {
                    state.cached += 1;
                }
                let complete = done >= total;
                let text = format!(
                    "{} {done}/{total} cues ({} cached)",
                    self.verb(if complete {
                        "Synthesized"
                    } else {
                        "Synthesizing"
                    }),
                    state.cached
                );
                if self.in_place {
                    let visible = width(&text);
                    let blank = state.live_len.saturating_sub(visible);
                    write_err(&format!("\r{text}{}", " ".repeat(blank)));
                    state.live = true;
                    state.live_len = visible;
                    if complete {
                        self.finish_line(&mut state);
                    }
                } else if complete || *done == 1 || done % 10 == 0 {
                    write_err(&format!("{text}\n"));
                }
                if complete {
                    state.cached = 0;
                }
            }
        }
    }

    /// Print a message on its own line (closing an open counter line first).
    pub(crate) fn notice(&self, message: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.finish_line(&mut state);
        write_err(&format!("{message}\n"));
    }

    /// Terminate an open counter line.
    pub(crate) fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.finish_line(&mut state);
    }

    fn finish_line(&self, state: &mut State) {
        if state.live {
            write_err("\n");
            state.live = false;
            state.live_len = 0;
        }
    }

    fn verb(&self, verb: &str) -> String {
        let padded = format!("{verb:>VERB_WIDTH$}");
        paint(self.color, Style::Success, &padded)
    }
}

/// `detail` with absolute paths below `cwd` made relative (`cwd` itself becomes `.`).
fn relative_detail(detail: &str, cwd: &str) -> String {
    if cwd.is_empty() || cwd == "/" {
        return detail.to_string();
    }
    if detail == cwd {
        return ".".to_string();
    }
    let prefix = format!("{cwd}{}", std::path::MAIN_SEPARATOR);
    detail.replace(&prefix, "")
}

/// `parse` → `Parsing`, `synthesize` → `Synthesizing`, …; unknown stages are capitalized.
fn stage_verb(stage: &str) -> String {
    let known = match stage {
        "parse" => "Parsing",
        "validate" => "Validating",
        "synthesize" => "Synthesizing",
        "decode" => "Decoding",
        "process" => "Processing",
        "layout" => "Laying out",
        "mix" => "Mixing",
        "duck" => "Ducking",
        "master" => "Mastering",
        "encode" => "Encoding",
        "export" => "Exporting",
        "subtitles" => "Captioning",
        "stems" => "Writing stems",
        "manifest" => "Writing",
        "preview" => "Previewing",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    let mut chars = stage.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn write_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn details_are_relative() {
        assert_eq!(relative_detail("/work/p", "/work/p"), ".");
        assert_eq!(relative_detail("/work/p/out", "/work/p"), "out");
        assert_eq!(
            relative_detail("/elsewhere/out", "/work/p"),
            "/elsewhere/out"
        );
        assert_eq!(relative_detail("12 cues", "/work/p"), "12 cues");
    }

    #[test]
    fn verbs() {
        assert_eq!(stage_verb("synthesize"), "Synthesizing");
        assert_eq!(stage_verb("layout"), "Laying out");
        assert_eq!(stage_verb("normalize"), "Normalize");
        assert_eq!(stage_verb(""), "");
    }
}
