//! One module per subcommand.

pub(crate) mod cache;
pub(crate) mod init;
pub(crate) mod mcp;
pub(crate) mod plan;
pub(crate) mod providers;
pub(crate) mod render;
pub(crate) mod schema;
pub(crate) mod validate;
pub(crate) mod voices;

use crate::cli::Command;
use crate::context::Ctx;
use crate::exit::CmdResult;
use crate::term::{Style, Term};

/// Run `command`.
pub(crate) fn execute(ctx: &Ctx, command: &Command) -> CmdResult {
    match command {
        Command::Validate(args) => validate::run(ctx, args),
        Command::Plan(args) => plan::run(ctx, args),
        Command::Render(args) => render::run(ctx, args),
        Command::Schema(args) => schema::run(ctx, args),
        Command::Init(args) => init::run(ctx, args),
        Command::Providers(args) => providers::run(ctx, args),
        Command::Voices(args) => voices::run(ctx, args),
        Command::Cache(args) => cache::run(ctx, args),
        Command::Mcp(args) => mcp::run(ctx, args),
    }
}

/// One `  key        value` line of a result block.
pub(crate) fn kv(term: &Term, key: &str, value: &str) -> String {
    format!(
        "  {} {value}\n",
        term.paint(Style::Bold, &format!("{key:<11}"))
    )
}
