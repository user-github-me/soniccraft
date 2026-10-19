//! `soniccraft-mcp`: the SonicCraft MCP server on stdin/stdout with zero arguments (equivalent to
//! `soniccraft mcp`), for MCP clients that cannot pass arguments (e.g. Codex skill MCP dependencies).

fn main() -> std::process::ExitCode {
    soniccraft_cli::run_mcp_launcher()
}
