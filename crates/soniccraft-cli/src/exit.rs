//! Process exit statuses (DESIGN §9.1) and the error type that ends a command.

use std::process::ExitCode;

use soniccraft_core::Diagnostic;

/// How a command ended. The numeric codes are a stable interface for scripts and agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exit {
    /// `0`: the command did what was asked.
    Success,
    /// `1`: the timeline is invalid (the report has error diagnostics).
    Invalid,
    /// `2`: the command line is wrong (clap errors, bad argument values).
    Usage,
    /// `3`: a runtime or render failure (I/O, provider, encoder, internal error).
    Failure,
    /// `130`: interrupted with Ctrl-C (128 + SIGINT).
    Interrupted,
}

impl Exit {
    /// The process exit status.
    pub(crate) fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Invalid => 1,
            Exit::Usage => 2,
            Exit::Failure => 3,
            Exit::Interrupted => 130,
        }
    }

    /// `Invalid` when the diagnostics contain an error, else `Success`.
    pub(crate) fn from_diagnostics(valid: bool, diagnostics: &[Diagnostic]) -> Exit {
        if valid && !soniccraft_core::diagnostics::has_errors(diagnostics) {
            Exit::Success
        } else {
            Exit::Invalid
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> ExitCode {
        ExitCode::from(exit.code())
    }
}

/// A failure that ends a command before it could produce its report.
///
/// Printed as `error: <message>` (plus `= hint:`) on stderr; with `--json`, stdout additionally carries the
/// failure document `{"message": …, "diagnostics": […], "hint": …}` so that stdout always holds exactly
/// one JSON document.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CliError {
    /// The exit status to use.
    pub(crate) exit: Exit,
    /// What went wrong, as one sentence without a trailing period.
    pub(crate) message: String,
    /// How to fix it, when there is a concrete suggestion.
    pub(crate) hint: Option<String>,
    /// Diagnostics for the `--json` failure document (usually none; e.g. the size error of a document
    /// too large to read).
    pub(crate) diagnostics: Vec<Diagnostic>,
}

impl CliError {
    /// A runtime failure (exit 3).
    pub(crate) fn failure(message: impl Into<String>) -> Self {
        CliError {
            exit: Exit::Failure,
            message: message.into(),
            hint: None,
            diagnostics: Vec::new(),
        }
    }

    /// A bad argument value (exit 2).
    pub(crate) fn usage(message: impl Into<String>) -> Self {
        CliError {
            exit: Exit::Usage,
            message: message.into(),
            hint: None,
            diagnostics: Vec::new(),
        }
    }

    /// Attach a fix suggestion.
    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

/// Result of a command: the exit status of a completed command, or the failure that stopped it.
pub(crate) type CmdResult = Result<Exit, CliError>;

#[cfg(test)]
mod tests {
    use super::*;
    use soniccraft_core::Diagnostic;

    #[test]
    fn codes_are_stable() {
        assert_eq!(Exit::Success.code(), 0);
        assert_eq!(Exit::Invalid.code(), 1);
        assert_eq!(Exit::Usage.code(), 2);
        assert_eq!(Exit::Failure.code(), 3);
        assert_eq!(Exit::Interrupted.code(), 130);
    }

    #[test]
    fn invalid_when_any_error() {
        let warn = Diagnostic::warning("bitrate_ignored", "w");
        let err = Diagnostic::error("unknown_reference", "e");
        assert_eq!(
            Exit::from_diagnostics(true, std::slice::from_ref(&warn)),
            Exit::Success
        );
        assert_eq!(Exit::from_diagnostics(true, &[warn, err]), Exit::Invalid);
        assert_eq!(Exit::from_diagnostics(false, &[]), Exit::Invalid);
    }
}
