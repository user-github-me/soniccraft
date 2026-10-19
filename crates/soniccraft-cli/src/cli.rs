//! The command line (DESIGN §9.1), defined with clap's derive API.
//!
//! The doc comments of the argument structs are the `--help` text, so placeholders such as
//! `<timeline dir>` stay plain text there; rustdoc would read them as unclosed HTML tags.
#![allow(rustdoc::invalid_html_tags)]

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::PossibleValuesParser;
use clap::{ArgAction, Args, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

use crate::context::FailureDoc;
use crate::exit::Exit;
use crate::term::{ColorChoice, Stream, Term};

const ROOT_AFTER_HELP: &str = "\
Quick start:
  soniccraft init demo                   # starter timeline + royalty-free demo assets
  cd demo
  soniccraft validate timeline.yaml      # errors, warnings and fix hints
  soniccraft plan timeline.yaml          # dry run: layout, ducking, cost (no API calls)
  soniccraft render timeline.yaml        # mastered audio + subtitles

Exit codes: 0 success, 1 timeline invalid (error diagnostics), 2 usage error,
            3 runtime or render failure, 130 interrupted (Ctrl-C).
With --json, stdout carries exactly one JSON document; progress and logs go to stderr.

Environment:
  SONICCRAFT_CACHE_DIR      TTS cache directory (--cache-dir wins)
  ELEVENLABS_API_KEY, OPENAI_API_KEY, CARTESIA_API_KEY, KOKORO_BASE_URL, KOKORO_API_KEY,
  PIPER_VOICES_DIR          provider setup (`soniccraft providers` shows what is usable)
  SONICCRAFT_ALLOWED_HOSTS  more hosts that may receive API keys, besides each provider's own API
                            host and servers on this machine (own key variable only; comma-separated)
  SONICCRAFT_MAX_RENDER_GB  memory budget of one render in GB [default: 8]
  SONICCRAFT_ALLOW_COMMAND  `soniccraft mcp`: 1 lets timelines run `command` providers
  SONICCRAFT_MCP_MAX_RENDERS
                            `soniccraft mcp`: renders that run at once [default: 2]
  NO_COLOR                  disable colors (unless --color always)
  RUST_LOG                  log filter for stderr logs, e.g. RUST_LOG=soniccraft_engine=debug";

const VALIDATE_AFTER_HELP: &str = "\
Checks the document (schema, references, anchors, overlaps) and its environment: assets exist and
decode, provider credentials are set (and would go only to allowed hosts), local TTS servers answer,
voices are installed (macOS voices, piper models), ffmpeg is present when the format needs it, the
render fits the memory budget, cache status. Exit status 1 when there are errors.

Examples:
  soniccraft validate timeline.yaml
  soniccraft validate timeline.yaml --check-credentials
  soniccraft validate timeline.yaml --json | jq '.diagnostics[] | select(.severity == \"error\")'
  cat timeline.yaml | soniccraft validate -";

const PLAN_AFTER_HELP: &str = "\
Validates, then lays the timeline out with cached or estimated durations (mock voices are measured
exactly): item times, cues moved by the overlap policy, ducking regions, characters to synthesize
per provider and an ASCII diagram. Never calls a TTS provider.

Examples:
  soniccraft plan timeline.yaml
  soniccraft dry-run timeline.yaml --json | jq '.items[] | {id, start_ms, end_ms}'";

const RENDER_AFTER_HELP: &str = "\
Runs the full pipeline: TTS (cached by content hash), layout, mixing, ducking, loudness mastering,
encoding and the side files. Press Ctrl-C once to cancel cleanly (partial outputs are removed),
twice to quit immediately.

Examples:
  soniccraft render timeline.yaml
  soniccraft render timeline.yaml -o out/episode.mp3 --subtitles --manifest
  soniccraft render timeline.yaml --preview          # + <stem>.preview.html (open in a browser)
  soniccraft render timeline.yaml --json > report.json";

const SCHEMA_AFTER_HELP: &str = "\
Examples:
  soniccraft schema > timeline-schema.json
  soniccraft schema -o .vscode/soniccraft-timeline.schema.json";

const INIT_AFTER_HELP: &str = "\
Writes DIR/timeline.yaml from a template plus the audio assets it references (generated
procedurally: royalty-free and deterministic). Existing files are never overwritten without --force.
The default template renders offline: `offline-demo` on macOS (system voices), `offline-demo-mock`
elsewhere.

Examples:
  soniccraft init demo && cd demo && soniccraft render timeline.yaml
  soniccraft init my-episode --template two-person-podcast
  soniccraft init --list";

const PROVIDERS_AFTER_HELP: &str = "\
Examples:
  soniccraft providers
  soniccraft providers --json | jq '.providers[] | select(.available) | .name'";

const VOICES_AFTER_HELP: &str = "\
PROVIDER is a built-in provider type or alias (elevenlabs, openai, cartesia, kokoro, piper,
macos_say, command, mock); here case and `-`/`_` do not matter (timelines spell types exactly).
Cloud providers are queried with their API key from the environment.

Examples:
  soniccraft voices macos_say
  soniccraft voices elevenlabs --json | jq -r '.voices[] | \"\\(.id)  \\(.name)\"'";

const CACHE_AFTER_HELP: &str = "\
The cache directory is --cache-dir, else $SONICCRAFT_CACHE_DIR, else <timeline dir>/.soniccraft/cache
(the current directory's .soniccraft/cache without a TIMELINE).

Examples:
  soniccraft cache info timeline.yaml
  soniccraft cache clear --cache-dir ~/.cache/soniccraft";

const MCP_AFTER_HELP: &str = "\
Serves the SonicCraft tools over MCP (newline-delimited JSON-RPC 2.0 on stdin/stdout; logs go to
stderr). Relative paths in tool calls resolve against the call's base_dir, else --base-dir, else
$CLAUDE_PROJECT_DIR, else the current directory. Timelines run no local programs (`command`
providers) unless the server is started with --allow-command. `soniccraft-mcp` is the same server
without arguments (configured by the same environment variables).

Register it:
  claude mcp add soniccraft -- soniccraft mcp
  codex mcp add soniccraft -- soniccraft mcp
  claude mcp add soniccraft -- soniccraft mcp --allow-command   # trusts timelines to run programs";

/// Help heading of the options every subcommand accepts.
const GLOBAL: &str = "Global options";

/// SonicCraft compiles declarative audio timelines (TTS speech, music beds, sound effects) into
/// ducked, loudness-mastered audio with WebVTT/SRT subtitles.
#[derive(Debug, Parser)]
#[command(
    name = "soniccraft",
    bin_name = "soniccraft",
    version,
    about = "Compile declarative audio timelines into mastered audio and subtitles",
    after_help = ROOT_AFTER_HELP,
    arg_required_else_help = true,
    propagate_version = true,
    max_term_width = 100
)]
pub(crate) struct Cli {
    #[command(flatten)]
    pub(crate) global: GlobalArgs,
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Options accepted by every subcommand.
#[derive(Debug, Default, Args)]
pub(crate) struct GlobalArgs {
    /// Print only errors and results: no progress, warnings, notes or next steps
    #[arg(short, long, global = true, conflicts_with = "verbose", help_heading = GLOBAL)]
    pub(crate) quiet: bool,

    /// Show info diagnostics and log to stderr (-v info, -vv debug, -vvv trace)
    #[arg(short, long, global = true, action = ArgAction::Count, help_heading = GLOBAL)]
    pub(crate) verbose: u8,

    /// When to use colors (`auto` honors NO_COLOR and needs a terminal)
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "WHEN",
        default_value_t = ColorChoice::Auto,
        help_heading = GLOBAL
    )]
    pub(crate) color: ColorChoice,

    /// TTS cache directory [default: $SONICCRAFT_CACHE_DIR, else <timeline dir>/.soniccraft/cache]
    #[arg(long, global = true, value_name = "DIR", help_heading = GLOBAL)]
    pub(crate) cache_dir: Option<PathBuf>,

    /// Maximum concurrent TTS requests [default: 4]
    #[arg(
        short = 'j',
        long,
        global = true,
        value_name = "N",
        value_parser = clap::value_parser!(u16).range(1..=64),
        help_heading = GLOBAL
    )]
    pub(crate) jobs: Option<u16>,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Check a timeline and its environment without rendering
    #[command(after_help = VALIDATE_AFTER_HELP)]
    Validate(ValidateArgs),

    /// Dry run: lay the timeline out with estimated durations (never calls a TTS provider)
    #[command(visible_alias = "dry-run", after_help = PLAN_AFTER_HELP)]
    Plan(PlanArgs),

    /// Render a timeline to mastered audio (plus subtitles, stems, manifest, preview)
    #[command(after_help = RENDER_AFTER_HELP)]
    Render(RenderArgs),

    /// Print the timeline JSON Schema
    #[command(after_help = SCHEMA_AFTER_HELP)]
    Schema(SchemaArgs),

    /// Create a starter timeline (and the audio assets it uses) from a template
    #[command(after_help = INIT_AFTER_HELP)]
    Init(InitArgs),

    /// List the built-in TTS providers and whether they are usable here
    #[command(after_help = PROVIDERS_AFTER_HELP)]
    Providers(ProvidersArgs),

    /// List the voices of a TTS provider
    #[command(after_help = VOICES_AFTER_HELP)]
    Voices(VoicesArgs),

    /// Show or clear the TTS cache
    #[command(after_help = CACHE_AFTER_HELP, subcommand_required = true, arg_required_else_help = true)]
    Cache(CacheArgs),

    /// Run the MCP server on stdin/stdout
    #[command(after_help = MCP_AFTER_HELP)]
    Mcp(McpArgs),
}

impl Command {
    /// Whether the command was asked for `--json` output.
    pub(crate) fn json(&self) -> bool {
        match self {
            Command::Validate(a) => a.json,
            Command::Plan(a) => a.json,
            Command::Render(a) => a.json,
            Command::Providers(a) => a.json,
            Command::Voices(a) => a.json,
            Command::Cache(a) => match &a.action {
                CacheAction::Info(t) | CacheAction::Clear(t) => t.json,
            },
            Command::Schema(_) | Command::Init(_) | Command::Mcp(_) => false,
        }
    }
}

/// `validate` options.
#[derive(Debug, Args)]
pub(crate) struct ValidateArgs {
    /// Timeline file (.yaml, .yml or .json); `-` reads it from stdin
    #[arg(value_name = "TIMELINE")]
    pub(crate) timeline: PathBuf,

    /// Also verify API keys with a cheap authenticated request (network)
    #[arg(long)]
    pub(crate) check_credentials: bool,

    /// Print the ValidationReport as JSON on stdout
    #[arg(long)]
    pub(crate) json: bool,
}

/// `plan` options.
#[derive(Debug, Args)]
pub(crate) struct PlanArgs {
    /// Timeline file (.yaml, .yml or .json); `-` reads it from stdin
    #[arg(value_name = "TIMELINE")]
    pub(crate) timeline: PathBuf,

    /// Print the PlanReport as JSON on stdout
    #[arg(long)]
    pub(crate) json: bool,
}

/// Output formats accepted by `render --format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    Wav,
    Mp3,
    Flac,
    /// Ogg Vorbis (needs ffmpeg)
    Ogg,
    /// Opus (needs ffmpeg)
    Opus,
    /// AAC in MP4 (needs ffmpeg)
    M4a,
}

impl Format {
    /// The DSL format name.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Mp3 => "mp3",
            Format::Flac => "flac",
            Format::Ogg => "ogg",
            Format::Opus => "opus",
            Format::M4a => "m4a",
        }
    }
}

/// `render` options.
#[derive(Debug, Args)]
pub(crate) struct RenderArgs {
    /// Timeline file (.yaml, .yml or .json); `-` reads it from stdin
    #[arg(value_name = "TIMELINE")]
    pub(crate) timeline: PathBuf,

    /// Output file, relative to the current directory [default: output.path, else <stem>.<ext>]
    ///
    /// Without -o the document's output.path is used (relative to the timeline), else
    /// <timeline stem>.<ext> next to the timeline. The extension picks the format unless
    /// --format or output.format is set.
    #[arg(short, long, value_name = "PATH", verbatim_doc_comment)]
    pub(crate) output: Option<PathBuf>,

    /// Output format (overrides output.format and the -o extension)
    #[arg(long, value_enum, ignore_case = true, value_name = "FORMAT")]
    pub(crate) format: Option<Format>,

    /// Write subtitles: the formats in output.export_subtitles, else WebVTT + SRT
    #[arg(long)]
    pub(crate) subtitles: bool,

    /// Write per-track stems to <out>.stems/ (32-bit float WAV)
    #[arg(long)]
    pub(crate) stems: bool,

    /// Write <out>.manifest.json (the render report with relative paths)
    #[arg(long)]
    pub(crate) manifest: bool,

    /// Write <out>.preview.html, a self-contained page with waveform, ducking and captions
    #[arg(long)]
    pub(crate) preview: bool,

    /// Ignore cached TTS audio (fresh results are still written to the cache)
    #[arg(long)]
    pub(crate) no_cache: bool,

    /// Print the RenderReport (or the failure) as JSON on stdout
    #[arg(long)]
    pub(crate) json: bool,
}

/// `schema` options.
#[derive(Debug, Args)]
pub(crate) struct SchemaArgs {
    /// Write the schema to FILE instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub(crate) output: Option<PathBuf>,
}

/// `init` options.
#[derive(Debug, Args)]
pub(crate) struct InitArgs {
    /// Project directory (created when missing)
    #[arg(value_name = "DIR", default_value = ".")]
    pub(crate) dir: PathBuf,

    /// Starter template [default: offline-demo on macOS, offline-demo-mock elsewhere]
    #[arg(
        short,
        long,
        value_name = "NAME",
        value_parser = PossibleValuesParser::new(crate::templates::NAMES)
    )]
    pub(crate) template: Option<String>,

    /// Overwrite existing files
    #[arg(long)]
    pub(crate) force: bool,

    /// Write only the timeline, not the audio assets it references
    #[arg(long)]
    pub(crate) no_assets: bool,

    /// List the templates and exit
    #[arg(long, conflicts_with_all = ["template", "force", "no_assets"])]
    pub(crate) list: bool,
}

/// `providers` options.
#[derive(Debug, Args)]
pub(crate) struct ProvidersArgs {
    /// Print the ProvidersReport as JSON on stdout
    #[arg(long)]
    pub(crate) json: bool,
}

/// `voices` options.
#[derive(Debug, Args)]
pub(crate) struct VoicesArgs {
    /// Provider type or alias, e.g. elevenlabs, kokoro, macos_say, mock
    #[arg(value_name = "PROVIDER")]
    pub(crate) provider: String,

    /// Print the VoicesReport as JSON on stdout
    #[arg(long)]
    pub(crate) json: bool,
}

/// `cache` options.
#[derive(Debug, Args)]
pub(crate) struct CacheArgs {
    #[command(subcommand)]
    pub(crate) action: CacheAction,
}

/// `cache` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum CacheAction {
    /// Show the cache location, entry count and size
    Info(CacheTarget),
    /// Delete every cached TTS result
    Clear(CacheTarget),
}

/// Which cache a `cache` subcommand acts on.
#[derive(Debug, Args)]
pub(crate) struct CacheTarget {
    /// Use this timeline's cache: <timeline dir>/.soniccraft/cache (unless --cache-dir is given)
    #[arg(value_name = "TIMELINE")]
    pub(crate) timeline: Option<PathBuf>,

    /// Print the result as JSON on stdout
    #[arg(long)]
    pub(crate) json: bool,
}

/// `mcp` options.
#[derive(Debug, Args)]
pub(crate) struct McpArgs {
    /// Base directory for relative tool paths [default: $CLAUDE_PROJECT_DIR, else the current dir]
    #[arg(long, value_name = "DIR")]
    pub(crate) base_dir: Option<PathBuf>,

    /// Let timelines run local programs through `command` providers
    ///
    /// Off by default: the timelines of tool calls are data written by the MCP client
    /// (an LLM agent, possibly steered by untrusted text), and a `command` provider runs
    /// the program it names with this server's permissions. Without this flag, such
    /// timelines get `provider_unavailable` errors. SONICCRAFT_ALLOW_COMMAND=1 does the same.
    #[arg(
        long,
        verbatim_doc_comment,
        env = crate::commands::mcp::ALLOW_COMMAND_ENV,
        value_parser = crate::commands::mcp::switch_value,
        hide_env_values = true
    )]
    pub(crate) allow_command: bool,
}

/// clap's own color setting for help and usage errors, from a `--color` given anywhere before `--`.
fn prescan_color(args: &[OsString]) -> clap::ColorChoice {
    let mut iter = args.iter().skip(1).map(|a| a.to_string_lossy());
    let mut choice = clap::ColorChoice::Auto;
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        let value = if arg == "--color" {
            iter.next().map(|v| v.into_owned())
        } else {
            arg.strip_prefix("--color=").map(str::to_string)
        };
        match value.as_deref() {
            Some("always") => choice = clap::ColorChoice::Always,
            Some("never") => choice = clap::ColorChoice::Never,
            Some("auto") => choice = clap::ColorChoice::Auto,
            _ => {}
        }
    }
    choice
}

/// Whether `--json` is among the options (before a `--` that ends them).
fn wants_json(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .map(|a| a.to_string_lossy())
        .take_while(|a| a != "--")
        .any(|a| a == "--json" || a.starts_with("--json="))
}

/// The subcommand path the arguments name (`["cache", "info"]`), as far as clap gets with them; empty
/// when there is none. Aliases are resolved (`dry-run` → `plan`).
fn subcommand_path(args: &[OsString]) -> Vec<String> {
    let mut path = Vec::new();
    if let Ok(matches) = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(args)
    {
        let mut current = &matches;
        while let Some((name, sub)) = current.subcommand() {
            path.push(name.to_string());
            current = sub;
        }
    }
    path
}

/// The `--json` failure document of a usage error: clap's message (one line, without `error: `) and a
/// hint with clap's tips and the help command of the subcommand (`path`, from [`subcommand_path`]).
fn usage_failure(error: &clap::Error, path: &[String]) -> (String, String) {
    let rendered = error.render().to_string();
    let mut blocks = rendered
        .split("\n\n")
        .map(str::trim)
        .filter(|b| !b.is_empty());
    let message = blocks
        .next()
        .map(|b| b.strip_prefix("error:").unwrap_or(b))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let tips: Vec<String> = blocks
        .flat_map(str::lines)
        .filter_map(|l| l.trim().strip_prefix("tip:"))
        .map(|tip| tip.trim().to_string())
        .collect();
    let mut command = std::iter::once("soniccraft")
        .chain(path.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let json_rejected = error.kind() == clap::error::ErrorKind::UnknownArgument
        && error
            .get(clap::error::ContextKind::InvalidArg)
            .is_some_and(|arg| arg.to_string() == "--json");
    let mut parts = Vec::new();
    if json_rejected {
        // clap's tips are about passing `--json` as a value; say where it goes instead.
        let after = tips.iter().find_map(|tip| {
            tip.strip_prefix('\'')?
                .strip_suffix(" --json' exists")
                .map(str::to_string)
        });
        parts.push(match after {
            Some(sub) => {
                let fix = format!("put --json after `{sub}`");
                command = format!("{command} {sub}");
                fix
            }
            None if path.first().map(String::as_str) == Some("schema") => {
                "`soniccraft schema` always prints JSON: drop --json".to_string()
            }
            None => format!("`{command}` has no --json option: drop it"),
        });
    } else {
        parts.extend(tips);
    }
    parts.push(format!("see `{command} --help`"));
    let message = if message.is_empty() {
        "invalid command line".to_string()
    } else {
        message
    };
    (message, parts.join("; "))
}

/// Parse the command line. On `--help`/`--version` or a usage error, clap's message is printed and the
/// exit status is returned instead (0 or 2). A usage error with `--json` among the arguments also prints
/// the failure document `{"message", "diagnostics": [], "hint"}` on stdout, so that stdout holds exactly
/// one JSON document as with every other `--json` failure (not for `mcp`, whose stdout is the protocol).
pub(crate) fn parse(args: Vec<OsString>) -> Result<Cli, u8> {
    let json = wants_json(&args);
    let mut command = Cli::command().color(prescan_color(&args));
    let parsed = command
        .try_get_matches_from_mut(args.iter().cloned())
        .and_then(|matches| Cli::from_arg_matches(&matches));
    parsed.map_err(|e| {
        let e = e.format(&mut command);
        // A broken pipe while printing help must not panic (clap returns the I/O error).
        let _ = e.print();
        let code = u8::try_from(e.exit_code()).unwrap_or(2);
        let path = if json && code == Exit::Usage.code() {
            subcommand_path(&args)
        } else {
            Vec::new()
        };
        if json && code == Exit::Usage.code() && path.first().map(String::as_str) != Some("mcp") {
            let (message, hint) = usage_failure(&e, &path);
            let doc = FailureDoc {
                message: &message,
                diagnostics: &[],
                hint: Some(&hint),
            };
            if let Ok(mut text) = serde_json::to_string_pretty(&doc) {
                text.push('\n');
                Term::new(Stream::Stdout, ColorChoice::Never).write(&text);
            }
        }
        code
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn globals_after_the_subcommand() {
        let cli = try_parse(&[
            "soniccraft",
            "render",
            "t.yaml",
            "-o",
            "x.wav",
            "--cache-dir",
            "/tmp/c",
            "--jobs",
            "2",
            "-vv",
            "--color",
            "never",
            "--json",
        ])
        .expect("parse");
        assert_eq!(cli.global.verbose, 2);
        assert_eq!(cli.global.jobs, Some(2));
        assert_eq!(cli.global.color, ColorChoice::Never);
        assert_eq!(cli.global.cache_dir, Some(PathBuf::from("/tmp/c")));
        let Command::Render(r) = cli.command else {
            panic!("expected render");
        };
        assert_eq!(r.output, Some(PathBuf::from("x.wav")));
        assert!(r.json);
    }

    #[test]
    fn aliases_and_values() {
        let cli = try_parse(&["soniccraft", "dry-run", "t.yaml"]).expect("parse");
        assert!(matches!(cli.command, Command::Plan(_)));
        let cli = try_parse(&["soniccraft", "render", "t.yaml", "--format", "MP3"]).expect("parse");
        let Command::Render(r) = cli.command else {
            panic!("expected render");
        };
        assert_eq!(r.format, Some(Format::Mp3));
        assert!(try_parse(&["soniccraft", "render", "t.yaml", "--format", "aiff"]).is_err());
        assert!(try_parse(&["soniccraft", "init", "--template", "nope"]).is_err());
        assert!(try_parse(&["soniccraft", "init", "--list", "--force"]).is_err());
        assert!(try_parse(&["soniccraft", "validate", "t.yaml", "-q", "-v"]).is_err());
        assert!(try_parse(&["soniccraft", "render", "t.yaml", "--jobs", "0"]).is_err());
        assert!(try_parse(&["soniccraft", "cache"]).is_err());
        let cli = try_parse(&["soniccraft", "cache", "clear", "t.yaml", "--json"]).expect("parse");
        assert!(cli.command.json());
    }

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn json_requests_and_subcommand_paths() {
        assert!(wants_json(&os(&["s", "validate", "t.yaml", "--json"])));
        assert!(wants_json(&os(&["s", "--json=yes", "plan"])));
        assert!(!wants_json(&os(&["s", "validate", "--", "--json"])));
        assert!(!wants_json(&os(&["s", "validate", "-j", "2"])));
        let path = |v: &[&str]| subcommand_path(&os(v));
        assert_eq!(path(&["s", "dry-run", "--json"]), ["plan"]);
        assert_eq!(path(&["s", "cache", "info", "--bogus"]), ["cache", "info"]);
        // A global option's value is not a subcommand.
        assert_eq!(
            path(&["s", "--cache-dir", "render", "validate", "--bogus"]),
            ["validate"]
        );
        assert!(path(&["s", "validat", "t.yaml"]).is_empty());
    }

    #[test]
    fn usage_failures_name_the_help_of_the_subcommand() {
        let failure = |v: &[&str]| {
            let args = os(v);
            let error = Cli::try_parse_from(&args).expect_err("a usage error");
            usage_failure(&error, &subcommand_path(&args))
        };
        assert_eq!(
            failure(&["s", "plan", "--json"]),
            (
                "the following required arguments were not provided: <TIMELINE>".to_string(),
                "see `soniccraft plan --help`".to_string()
            )
        );
        let (message, hint) = failure(&["s", "render", "t.yaml", "--format", "aiff", "--json"]);
        assert_eq!(
            message,
            "invalid value 'aiff' for '--format <FORMAT>' [possible values: wav, mp3, flac, ogg, opus, m4a]"
        );
        assert_eq!(hint, "see `soniccraft render --help`");
        let (_, hint) = failure(&["s", "--json", "validate", "t.yaml"]);
        assert_eq!(
            hint,
            "put --json after `validate`; see `soniccraft validate --help`"
        );
        let (_, hint) = failure(&["s", "init", "--json"]);
        assert_eq!(
            hint,
            "`soniccraft init` has no --json option: drop it; see `soniccraft init --help`"
        );
        let (_, hint) = failure(&["s", "validate", "t.yaml", "--jsn"]);
        assert_eq!(
            hint,
            "a similar argument exists: '--json'; see `soniccraft validate --help`"
        );
    }

    #[test]
    fn color_prescan() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            prescan_color(&args(&["s", "--color", "never", "plan"])),
            clap::ColorChoice::Never
        );
        assert_eq!(
            prescan_color(&args(&["s", "plan", "--color=always"])),
            clap::ColorChoice::Always
        );
        assert_eq!(
            prescan_color(&args(&["s", "--", "--color=always"])),
            clap::ColorChoice::Auto
        );
    }
}
