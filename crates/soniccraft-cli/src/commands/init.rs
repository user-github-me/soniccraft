//! `soniccraft init [DIR] [--template NAME] [--force] [--no-assets] [--list]`

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::InitArgs;
use crate::context::Ctx;
use crate::exit::{CliError, CmdResult, Exit};
use crate::format::{pad, shell_quote, width};
use crate::templates::{self, TEMPLATES};
use crate::term::Style;

/// The file name of the timeline `init` writes.
const TIMELINE_FILE: &str = "timeline.yaml";

/// Run the command.
pub(crate) fn run(ctx: &Ctx, args: &InitArgs) -> CmdResult {
    if args.list {
        list(ctx);
        return Ok(Exit::Success);
    }
    let name: &str = match args.template.as_deref() {
        Some(name) => name,
        None => templates::default_name(),
    };
    let template = templates::find(name).ok_or_else(|| {
        CliError::usage(format!("unknown template `{name}`"))
            .with_hint("`soniccraft init --list` shows the templates")
    })?;
    let instance = templates::instantiate(template, TIMELINE_FILE, !args.no_assets)
        .map_err(CliError::failure)?;

    let dir = ctx.absolute(&args.dir);
    if dir.exists() && !dir.is_dir() {
        return Err(CliError::failure(format!(
            "`{}` exists and is not a directory",
            args.dir.display()
        )));
    }
    let timeline_path = dir.join(TIMELINE_FILE);
    let mut planned: Vec<PathBuf> = vec![timeline_path.clone()];
    if !args.no_assets {
        planned.extend(instance.assets.iter().map(|a| dir.join(a.relative_path())));
    }
    let existing: Vec<&PathBuf> = planned.iter().filter(|p| p.exists()).collect();
    if !existing.is_empty() && !args.force {
        let names: Vec<String> = existing.iter().map(|p| ctx.show(p)).collect();
        return Err(CliError::failure(format!(
            "refusing to overwrite existing {}: {}",
            if names.len() == 1 { "file" } else { "files" },
            names.join(", ")
        ))
        .with_hint("pass --force to overwrite them, or choose another directory"));
    }

    std::fs::create_dir_all(&dir).map_err(|e| {
        CliError::failure(format!(
            "cannot create directory `{}`: {e}",
            args.dir.display()
        ))
    })?;
    if !args.no_assets {
        for asset in &instance.assets {
            let path = dir.join(asset.relative_path());
            asset.write(&path).map_err(|e| {
                CliError::failure(format!("cannot write `{}`: {e}", ctx.show(&path)))
            })?;
        }
    }
    write_atomic(&timeline_path, instance.text.as_bytes()).map_err(|e| {
        CliError::failure(format!("cannot write `{}`: {e}", ctx.show(&timeline_path)))
    })?;

    if ctx.quiet {
        return Ok(Exit::Success);
    }
    let term = &ctx.out;
    let mut text = format!(
        "{} {} from template `{}`\n",
        term.paint(Style::Success, "Created"),
        term.paint(Style::Bold, &ctx.show(&timeline_path)),
        template.name
    );
    if !args.no_assets && !instance.assets.is_empty() {
        let col = instance
            .assets
            .iter()
            .map(|a| width(a.relative_path()))
            .max()
            .unwrap_or(0);
        for asset in &instance.assets {
            text.push_str(&format!(
                "  {}  {}\n",
                pad(asset.relative_path(), col),
                asset.describe()
            ));
        }
    } else if args.no_assets && !instance.assets.is_empty() {
        let paths: Vec<&str> = instance.assets.iter().map(|a| a.relative_path()).collect();
        text.push_str(&format!(
            "  no audio written (--no-assets); the timeline expects: {}\n",
            paths.join(", ")
        ));
    }
    if template.needs != "nothing" {
        text.push_str(&format!(
            "  {} {}\n",
            term.paint(Style::Bold, "needs:"),
            template.needs
        ));
    }
    text.push_str(&format!("\n{}\n", term.paint(Style::Bold, "Next steps:")));
    if dir != ctx.cwd {
        text.push_str(&format!("  cd {}\n", shell_quote(&ctx.show(&dir))));
    }
    for step in ["validate", "plan", "render"] {
        text.push_str(&format!("  soniccraft {step} {TIMELINE_FILE}\n"));
    }
    term.write(&text);
    Ok(Exit::Success)
}

/// `--list`: every template with its description and requirements.
fn list(ctx: &Ctx) {
    let term = &ctx.out;
    let default = templates::default_name();
    let label = |name: &str| {
        if name == default {
            format!("{name} (default)")
        } else {
            name.to_string()
        }
    };
    let col = TEMPLATES
        .iter()
        .map(|t| width(&label(t.name)))
        .max()
        .unwrap_or(0);
    let mut text = String::new();
    for t in &TEMPLATES {
        text.push_str(&format!(
            "{}  {}\n{}  {}\n",
            term.paint(Style::Bold, &pad(&label(t.name), col)),
            t.summary,
            " ".repeat(col),
            term.paint(Style::Dim, &format!("needs: {}", t.needs))
        ));
    }
    if !ctx.quiet {
        text.push_str(
            "\nsoniccraft init [DIR] --template NAME writes DIR/timeline.yaml and its assets.\n",
        );
    }
    term.write(&text);
}

/// Write `bytes` to `path` through a temporary sibling and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "timeline".to_string());
    let temp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}
