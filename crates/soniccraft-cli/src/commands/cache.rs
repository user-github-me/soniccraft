//! `soniccraft cache <info|clear> [TIMELINE] [--cache-dir DIR] [--json]`

use serde::Serialize;

use super::kv;
use crate::cli::{CacheAction, CacheArgs};
use crate::context::Ctx;
use crate::exit::{CliError, CmdResult, Exit};
use crate::format::{bytes, plural};
use crate::term::Style;

/// JSON result of `cache clear`.
#[derive(Debug, Serialize)]
struct Cleared {
    path: String,
    removed: usize,
}

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &CacheArgs) -> CmdResult {
    let (CacheAction::Info(target) | CacheAction::Clear(target)) = &args.action;
    let loaded = target
        .timeline
        .as_deref()
        .map(|p| ctx.load(p))
        .transpose()?;
    let source = loaded.as_ref().map(|l| &l.source);
    let engine = ctx.engine();
    let dir = engine.cache_dir(source);
    let io_error = |e: std::io::Error| {
        CliError::failure(format!(
            "cannot access the cache directory `{}`: {e}",
            dir.display()
        ))
    };
    match &args.action {
        CacheAction::Info(_) => {
            let info = engine.cache_info(source).map_err(io_error)?;
            if ctx.json {
                ctx.print_json(&info)?;
                return Ok(Exit::Success);
            }
            let term = &ctx.out;
            let mut text = kv(term, "cache", &ctx.show_str(&info.path));
            let exists = std::path::Path::new(&info.path).is_dir();
            text.push_str(&kv(
                term,
                "entries",
                &if exists {
                    format!(
                        "{}, {}",
                        plural(info.entries, "entry", "entries"),
                        bytes(info.bytes)
                    )
                } else {
                    "none (the directory does not exist yet)".to_string()
                },
            ));
            term.write(&text);
        }
        CacheAction::Clear(_) => {
            let removed = engine.cache_clear(source).map_err(io_error)?;
            let path = dir.display().to_string();
            if ctx.json {
                ctx.print_json(&Cleared { path, removed })?;
                return Ok(Exit::Success);
            }
            if !ctx.quiet {
                ctx.status(
                    &ctx.out,
                    Style::Success,
                    "cleared",
                    &format!(
                        "removed {} from {}",
                        plural(removed, "cache entry", "cache entries"),
                        ctx.show(&dir)
                    ),
                );
            }
        }
    }
    Ok(Exit::Success)
}
