//! SonicCraft MCP server: the engine's validate / dry-run / render operations as Model Context Protocol
//! tools over stdio (newline-delimited JSON-RPC 2.0). See docs/DESIGN.md §9.2.
//!
//! # Protocol
//!
//! Built on the official Rust SDK (`rmcp` 3.5.1), the server speaks both MCP eras on one stdio
//! connection:
//!
//! * the `initialize` handshake of revisions 2024-11-05, 2025-03-26, 2025-06-18 and 2025-11-25 (a
//!   requested version the server supports is echoed; any other gets 2025-11-25), used by Codex and
//!   Cursor, with `ping`;
//! * the stateless 2026-07-28 revision: `server/discover` plus per-request `_meta`
//!   (`io.modelcontextprotocol/protocolVersion`, `…/clientCapabilities`), which Claude Code probes first.
//!
//! Requests are handled concurrently (a render never blocks `ping` or `tools/list`), responses are
//! single compact lines, unknown methods are `-32601`, lines that are not JSON are `-32700`, malformed
//! messages `-32600` (also a request whose `id` is not a string or an integer), unknown tools and
//! malformed `tools/call` params (including positional `params`) `-32602`. Problems with tool *arguments*
//! (an unknown name, a wrong type, a bad value) are tool results with `isError: true` that the model can
//! act on (SEP-1303). In the 2026-07-28 revision every result carries
//! `_meta["io.modelcontextprotocol/serverInfo"]`. Nothing but protocol messages is written to the output;
//! logs go through `tracing` (the binary sends them to stderr).
//!
//! # Tools
//!
//! | Tool | Arguments | `structuredContent` |
//! |---|---|---|
//! | `soniccraft_validate_timeline` | `yaml_content` \| `yaml_path` (alias `path`), `base_dir`, `check_credentials`, `detail` | `ValidationReport` |
//! | `soniccraft_dry_run` | `yaml_content` \| `yaml_path` (alias `path`), `base_dir`, `detail` | `PlanReport` |
//! | `soniccraft_render` | `yaml_content` \| `yaml_path` (alias `path`), `base_dir`, `output_path`, `format`, `export_subtitles`, `no_cache`, `detail` | `RenderReport` / `RenderFailure` |
//! | `soniccraft_get_schema` | `detail` | `{dsl_version, schema_id, quick_reference}` (+ `schema` with `detail: "full"`) |
//! | `soniccraft_list_providers` | none | `ProvidersReport` |
//! | `soniccraft_list_voices` | `provider` | `VoicesReport` |
//!
//! Every result has the report as `structuredContent`, a compact summary as its first text block and the
//! report JSON as its second (`soniccraft_get_schema`: the quick reference, then the schema JSON or a
//! note); render results add `resource_link` blocks (`file://`, absolute paths) for the audio and side
//! files, or, for sessions on a revision older than 2025-06-18, a text block listing them. An invalid
//! timeline is a successful validate/dry-run call (`valid: false`); a failed render, and a known argument
//! with a bad value, is `isError: true`.
//!
//! Results are sized for an agent's context: with `detail: "auto"` (the default) a report whose JSON
//! exceeds about 12,000 characters comes back compact (counts, errors and warnings, the first and last
//! items and the items diagnostics name, `items_truncated`, `full_report`), and the whole result stays
//! under about 40,000 characters; `detail: "summary"` is always compact and `detail: "full"` returns
//! everything that fits in one message, shortening (never failing) beyond that.
//!
//! Renders send `notifications/progress` when the call carries `_meta.progressToken`, and
//! `notifications/cancelled` stops a render through the engine's [`soniccraft_engine::CancelFlag`]
//! (partial files are removed; no response is sent for the cancelled request). At most
//! [`ServerOptions::max_concurrent_renders`] renders run at once (default 2, env
//! `SONICCRAFT_MCP_MAX_RENDERS`); later ones wait, reporting progress and honouring cancellation.
//!
//! # Safety
//!
//! Tool calls carry timeline documents, which may come from anywhere. The server therefore runs the
//! programs a timeline names (`command` providers, a piper `command`) only with
//! [`ServerOptions::allow_command`] (`soniccraft mcp --allow-command`, env `SONICCRAFT_ALLOW_COMMAND=1`),
//! and keeps a timeline's own output paths inside the call's base directory; an `output_path` argument is
//! the caller's explicit choice and may point anywhere. API keys go only to each provider's official
//! host, servers on this machine (the provider's own key variable only) or hosts listed in
//! `SONICCRAFT_ALLOWED_HOSTS` (enforced by the TTS layer).
//!
//! # Paths
//!
//! Relative `yaml_path`, `output_path` and the paths inside `yaml_content` resolve against the tool
//! argument `base_dir`, else [`ServerOptions::base_dir`] (`soniccraft mcp --base-dir`), else env
//! `CLAUDE_PROJECT_DIR`, else the process's working directory.
//!
//! # Example
//!
//! ```no_run
//! # async fn run() -> std::io::Result<()> {
//! use soniccraft_engine::{Engine, EngineOptions};
//! use soniccraft_mcp::{ServerOptions, serve_stdio};
//!
//! let engine = Engine::new(EngineOptions::default());
//! serve_stdio(engine, ServerOptions::default()).await
//! # }
//! ```

#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod args;
mod compact;
mod paths;
mod progress;
mod results;
mod server;
mod stdio;
mod summary;
mod tools;
mod transport;

use std::path::PathBuf;
use std::time::Duration;

use rmcp::ServiceExt as _;
use rmcp::service::ServerInitializeError;
use soniccraft_engine::Engine;
use tokio::io::{AsyncRead, AsyncWrite};

pub use tools::{
    DRY_RUN_TOOL, PROVIDERS_TOOL, RENDER_TOOL, SCHEMA_TOOL, TOOL_NAMES, VALIDATE_TOOL, VOICES_TOOL,
};

/// `serverInfo.name`.
pub const SERVER_NAME: &str = "soniccraft";

/// `serverInfo.title`.
pub const SERVER_TITLE: &str = "SonicCraft";

/// The DSL quick reference returned by `soniccraft_get_schema` (Markdown).
pub const DSL_QUICK_REFERENCE: &str = include_str!("quickref.md");

/// How long renders still running when the client disconnects get to stop and remove partial files
/// (after rmcp has already waited up to 5 s for in-flight requests to finish).
const DISCONNECT_GRACE: Duration = Duration::from_secs(2);

/// How long renders get on SIGINT/SIGTERM. Clients escalate to SIGKILL quickly (Claude Code after
/// 500 ms), so this is short.
const SIGNAL_GRACE: Duration = Duration::from_millis(400);

/// Environment variable that lets timelines run local programs (`1`, `true`, `yes` or `on`), like
/// [`ServerOptions::allow_command`].
pub const ALLOW_COMMAND_ENV: &str = "SONICCRAFT_ALLOW_COMMAND";

/// Environment variable with the most renders that run at once (a positive integer), used when
/// [`ServerOptions::max_concurrent_renders`] is not set.
pub const MAX_RENDERS_ENV: &str = "SONICCRAFT_MCP_MAX_RENDERS";

/// Renders that run at once by default; further renders wait (and report progress while they do).
pub const DEFAULT_MAX_CONCURRENT_RENDERS: usize = 2;

/// Server configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerOptions {
    /// Default directory that relative tool paths resolve against (`soniccraft mcp --base-dir`). `None`
    /// uses env `CLAUDE_PROJECT_DIR`, else the process's working directory. A relative value resolves
    /// against the working directory. A tool call's own `base_dir` argument takes precedence.
    pub base_dir: Option<PathBuf>,
    /// Let timelines use the `command` provider, which runs local programs (`soniccraft mcp
    /// --allow-command`, or env `SONICCRAFT_ALLOW_COMMAND=1`, which the server also reads). Off by
    /// default: tool calls carry timeline documents, which are data and must not run programs unless the
    /// user opted in.
    pub allow_command: bool,
    /// Most renders that run at once; later renders wait for a slot, reporting progress and honouring
    /// cancellation while they wait. `None` uses env `SONICCRAFT_MCP_MAX_RENDERS`, else
    /// [`DEFAULT_MAX_CONCURRENT_RENDERS`]. Renders hold whole timelines of audio in memory, so a few at a
    /// time keeps the server's footprint bounded.
    pub max_concurrent_renders: Option<usize>,
}

impl ServerOptions {
    /// Options with a default base directory.
    pub fn with_base_dir(base_dir: impl Into<PathBuf>) -> Self {
        ServerOptions {
            base_dir: Some(base_dir.into()),
            ..ServerOptions::default()
        }
    }

    /// Allow or forbid the `command` provider (see [`ServerOptions::allow_command`]).
    #[must_use]
    pub fn with_allow_command(mut self, allow: bool) -> Self {
        self.allow_command = allow;
        self
    }

    /// Set the most renders that run at once (at least 1; see
    /// [`ServerOptions::max_concurrent_renders`]).
    #[must_use]
    pub fn with_max_concurrent_renders(mut self, renders: usize) -> Self {
        self.max_concurrent_renders = Some(renders.max(1));
        self
    }
}

/// Serve MCP on the process's stdin/stdout until stdin closes or the process receives SIGINT/SIGTERM
/// (Ctrl-C on Windows).
///
/// Returns `Ok(())` on every clean shutdown, including a client that closes stdin before (or without)
/// initializing. Renders still running at shutdown are cancelled and given a moment to remove their
/// partial files. Errors are I/O failures of the transport.
///
/// stdin is read on a detached thread rather than Tokio's blocking pool, so returning after a signal
/// never leaves the runtime waiting on a stdin read that cannot be cancelled.
pub async fn serve_stdio(engine: Engine, options: ServerOptions) -> std::io::Result<()> {
    run(
        engine,
        &options,
        stdio::stdin(),
        tokio::io::stdout(),
        shutdown_signal(),
    )
    .await
}

/// Serve MCP over any reader/writer pair (for tests and embedding) until the reader reaches end of
/// file. Same behaviour as [`serve_stdio`] except that it installs no signal handlers.
pub async fn serve<R, W>(
    engine: Engine,
    options: ServerOptions,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run(engine, &options, reader, writer, std::future::pending()).await
}

async fn run<R, W>(
    engine: Engine,
    options: &ServerOptions,
    reader: R,
    writer: W,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let server = server::Server::new(engine, options);
    let renders = server.renders();
    let transport = transport::LineTransport::new(reader, writer);
    let (outcome, grace) = tokio::select! {
        outcome = serve_connection(server, transport) => (outcome, DISCONNECT_GRACE),
        () = shutdown => {
            tracing::info!("shutdown signal received");
            (Ok(()), SIGNAL_GRACE)
        }
    };
    let running = renders.cancel_all();
    if running > 0 {
        tracing::info!(running, "cancelling renders still in progress");
        if !renders.wait_idle(grace).await {
            tracing::warn!("renders did not stop in time; partial output files may remain");
        }
    }
    outcome
}

/// Run one MCP connection to completion.
async fn serve_connection<R, W>(
    server: server::Server,
    transport: transport::LineTransport<R, W>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let running = match server.serve(transport).await {
        Ok(running) => running,
        Err(ServerInitializeError::ConnectionClosed(_)) => {
            tracing::info!("the client closed the connection before initializing");
            return Ok(());
        }
        Err(ServerInitializeError::Cancelled) => return Ok(()),
        Err(ServerInitializeError::TransportError { error, context }) => {
            let io = error.error.downcast_ref::<std::io::Error>();
            if io.is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe) {
                tracing::info!("the client went away during initialization");
                return Ok(());
            }
            return Err(std::io::Error::other(format!(
                "MCP transport error while {context}: {error}"
            )));
        }
        Err(error) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("MCP initialization failed: {error}"),
            ));
        }
    };
    match running.waiting().await {
        Ok(reason) => {
            tracing::info!(?reason, "MCP connection closed");
            Ok(())
        }
        Err(error) => Err(std::io::Error::other(format!(
            "the MCP service loop failed: {error}"
        ))),
    }
}

/// Resolves on SIGINT or SIGTERM (Ctrl-C elsewhere); never resolves if handlers cannot be installed.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut interrupt), Ok(mut terminate)) => {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
            }
            _ => {
                tracing::warn!("cannot install signal handlers; relying on stdin EOF for shutdown");
                std::future::pending::<()>().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_reference_is_compact_and_consistent() {
        let lines = DSL_QUICK_REFERENCE.lines().count();
        assert!(lines <= 250, "{lines} lines");
        // The example timeline in the quick reference must compile without errors.
        let start = DSL_QUICK_REFERENCE
            .find("## Example\n```yaml\n")
            .expect("example section")
            + "## Example\n```yaml\n".len();
        let end = start
            + DSL_QUICK_REFERENCE[start..]
                .find("```")
                .expect("end of example");
        let compiled = soniccraft_core::compile(
            &DSL_QUICK_REFERENCE[start..end],
            soniccraft_core::SourceFormat::Yaml,
            &soniccraft_core::LowerOptions::default(),
        );
        let errors: Vec<String> = compiled
            .diagnostics
            .iter()
            .filter(|d| d.is_error())
            .map(ToString::to_string)
            .collect();
        assert!(errors.is_empty(), "{errors:#?}");
        assert!(compiled.program.is_some());
        // Every tool the workflow names exists.
        for tool in [VALIDATE_TOOL, DRY_RUN_TOOL, RENDER_TOOL] {
            assert!(DSL_QUICK_REFERENCE.contains(tool), "{tool}");
        }
    }

    #[test]
    fn options_constructors() {
        let default = ServerOptions::default();
        assert_eq!(default.base_dir, None);
        assert!(
            !default.allow_command,
            "timelines may not run programs by default"
        );
        assert_eq!(default.max_concurrent_renders, None);
        let options = ServerOptions::with_base_dir("/x")
            .with_allow_command(true)
            .with_max_concurrent_renders(0);
        assert_eq!(options.base_dir, Some(PathBuf::from("/x")));
        assert!(options.allow_command);
        assert_eq!(options.max_concurrent_renders, Some(1));
    }
}
