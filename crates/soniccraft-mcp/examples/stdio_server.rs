//! Run the SonicCraft MCP server on stdin/stdout with a default engine, e.g. to try it before the CLI's
//! `soniccraft mcp` exists or to point the MCP Inspector at it:
//!
//! ```sh
//! cargo build -p soniccraft-mcp --example stdio_server
//! npx @modelcontextprotocol/inspector --cli target/debug/examples/stdio_server --method tools/list
//! ```
//!
//! Optional first argument: the default base directory for relative tool paths (like
//! `soniccraft mcp --base-dir`). Logs go to stderr; set `SONICCRAFT_LOG` (an `EnvFilter` directive such as
//! `info` or `soniccraft_mcp=debug`) to see more.

use std::path::PathBuf;
use std::process::ExitCode;

use soniccraft_engine::{Engine, EngineOptions};
use soniccraft_mcp::{ServerOptions, serve_stdio};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let filter =
        EnvFilter::try_from_env("SONICCRAFT_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr) // stdout carries only MCP messages
        .with_ansi(false)
        .with_env_filter(filter)
        .init();

    let options = ServerOptions {
        base_dir: std::env::args_os().nth(1).map(PathBuf::from),
        ..ServerOptions::default()
    };
    match serve_stdio(Engine::new(EngineOptions::default()), options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("stdio_server: {error}");
            ExitCode::FAILURE
        }
    }
}
