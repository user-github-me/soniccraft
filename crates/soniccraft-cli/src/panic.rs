//! Panic reporting.
//!
//! Some dependencies deliberately catch panics (the dsp crate turns a decoder crash on a malformed file
//! into a diagnostic), so the hook must not print a scary message for every panic. It records the last
//! panic instead (and echoes it with `-v` or `RUST_BACKTRACE`); when a panic actually reaches the top of
//! the CLI, [`report`] prints one `error: internal error` block and the process exits with status 3.

use std::io::Write;
use std::sync::Mutex;

static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Install the recording hook. With `echo`, every panic is also written to stderr when it happens.
pub(crate) fn install_hook(echo: bool) {
    let echo = echo || std::env::var_os("RUST_BACKTRACE").is_some_and(|v| v != "0");
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string());
        let location = info
            .location()
            .map(|l| format!(" at {}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let thread = std::thread::current();
        let text = format!(
            "thread '{}' panicked{location}: {message}",
            thread.name().unwrap_or("<unnamed>")
        );
        if echo {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "{text}");
            if std::env::var_os("RUST_BACKTRACE").is_some_and(|v| v != "0") {
                let _ = writeln!(err, "{}", std::backtrace::Backtrace::force_capture());
            }
        }
        let mut slot = LAST_PANIC.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(text);
    }));
}

/// Print the `internal error` report for a panic that reached the top level.
pub(crate) fn report() {
    let detail = LAST_PANIC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_else(|| "unknown panic".to_string());
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "error: internal error: {detail}");
    let _ = writeln!(
        err,
        "  = note: this is a bug in soniccraft {}; please report it together with the command you ran",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        err,
        "  = note: run with RUST_BACKTRACE=1 for a backtrace (release binaries are stripped; a build from source names the functions)"
    );
}
