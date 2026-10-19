//! Diagnostic logging (`-v`, `RUST_LOG`) through `tracing-subscriber`, always on stderr.
//!
//! Logging is off by default. `-v` enables info-level logs of the SonicCraft crates, `-vv` debug, `-vvv`
//! trace for everything; a non-empty `RUST_LOG` replaces that choice entirely. stdout is never written:
//! it carries reports (`--json`) or MCP protocol messages.

use tracing_subscriber::EnvFilter;

/// The SonicCraft crates whose logs `-v`/`-vv` enable.
const CRATES: [&str; 8] = [
    "soniccraft_cli",
    "soniccraft_engine",
    "soniccraft_mcp",
    "soniccraft_tts",
    "soniccraft_dsp",
    "soniccraft_core",
    "soniccraft_layout",
    "soniccraft_captions",
];

/// The filter directive for a `-v` count (`None`: logging off).
fn directive(verbose: u8) -> Option<String> {
    let (ours, others) = match verbose {
        0 => return None,
        1 => ("info", "warn"),
        2 => ("debug", "info"),
        _ => return Some("trace".to_string()),
    };
    let mut d = others.to_string();
    for krate in CRATES {
        d.push_str(&format!(",{krate}={ours}"));
    }
    Some(d)
}

/// Install the global subscriber. `ansi` must be false unless stderr is a terminal (MCP clients and log
/// files must never receive escape codes). Does nothing when neither `-v` nor `RUST_LOG` asks for logs.
pub(crate) fn init(verbose: u8, ansi: bool) {
    let from_env = std::env::var("RUST_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let filter = match from_env {
        Some(spec) => match EnvFilter::try_new(&spec) {
            Ok(filter) => filter,
            Err(e) => {
                use std::io::Write;
                let _ = writeln!(
                    std::io::stderr(),
                    "warning: ignoring invalid RUST_LOG `{spec}`: {e}"
                );
                match directive(verbose) {
                    Some(d) => EnvFilter::new(d),
                    None => return,
                }
            }
        },
        None => match directive(verbose) {
            Some(d) => EnvFilter::new(d),
            None => return,
        },
    };
    // `try_init` only fails when a subscriber is already installed (tests); that is fine.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_env_filter(filter)
        .with_target(verbose >= 2)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_per_level() {
        assert_eq!(directive(0), None);
        let one = directive(1).unwrap_or_default();
        assert!(one.starts_with("warn,"), "{one}");
        assert!(one.contains("soniccraft_engine=info"), "{one}");
        let two = directive(2).unwrap_or_default();
        assert!(two.contains("soniccraft_tts=debug"), "{two}");
        assert_eq!(directive(3).as_deref(), Some("trace"));
        for d in [one, two] {
            assert!(EnvFilter::try_new(&d).is_ok(), "{d}");
        }
    }
}
