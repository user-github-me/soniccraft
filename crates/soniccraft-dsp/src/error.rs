//! DSP error type.

use std::path::PathBuf;

/// Errors from decoding, resampling, analysis, loudness measurement and encoding.
///
/// Every variant carries enough context (a path, the thing being processed, the underlying library
/// message) to be shown to a user or an agent without further decoration.
#[derive(Debug, thiserror::Error)]
pub enum DspError {
    /// A filesystem operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The file or directory the operation was about.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Audio could not be decoded (unsupported format, corrupt data, no audio track, ...).
    #[error("could not decode {what}: {message}")]
    Decode {
        /// What was being decoded (a path, or a description of in-memory data).
        what: String,
        /// Why it failed.
        message: String,
    },
    /// Audio could not be encoded.
    #[error("could not encode {what}: {message}")]
    Encode {
        /// What was being encoded (usually the output path or format).
        what: String,
        /// Why it failed.
        message: String,
    },
    /// The resampler failed or was given parameters it cannot handle.
    #[error("resampling failed: {0}")]
    Resample(String),
    /// Parameters are out of range or inconsistent (ragged channels, zero sample rate, ...).
    #[error("invalid audio parameters: {0}")]
    InvalidInput(String),
    /// An operation needs ffmpeg and none was found.
    #[error("ffmpeg is required for {0} but was not found on PATH")]
    FfmpegNotFound(String),
    /// Loudness measurement (EBU R128 / BS.1770) failed.
    #[error("loudness measurement failed: {0}")]
    Loudness(String),
    /// The caller's cancellation callback asked a long operation (mastering, limiting) to stop. The
    /// buffer being processed is left in an unspecified (but valid) state.
    #[error("the operation was cancelled")]
    Cancelled,
}

impl DspError {
    /// Shorthand for [`DspError::Io`].
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        DspError::Io {
            path: path.into(),
            source,
        }
    }

    /// Shorthand for [`DspError::Decode`].
    pub(crate) fn decode(what: impl Into<String>, message: impl Into<String>) -> Self {
        DspError::Decode {
            what: what.into(),
            message: message.into(),
        }
    }

    /// Shorthand for [`DspError::Encode`].
    pub(crate) fn encode(what: impl Into<String>, message: impl Into<String>) -> Self {
        DspError::Encode {
            what: what.into(),
            message: message.into(),
        }
    }
}
