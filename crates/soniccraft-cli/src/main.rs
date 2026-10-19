//! The `soniccraft` binary. See `soniccraft --help` and docs/DESIGN.md §9.1.

fn main() -> std::process::ExitCode {
    soniccraft_cli::run()
}
