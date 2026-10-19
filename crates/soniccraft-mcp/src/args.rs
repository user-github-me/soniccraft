//! Tool arguments: one serde + schemars struct per tool, so the advertised `inputSchema` and the parser
//! cannot drift apart, plus the shared timeline-loading logic (`yaml_content` | `yaml_path` | `path`).
//!
//! Every argument problem is a tool result with `isError: true` (MCP 2025-11-25 SEP-1303: input validation
//! errors are tool execution errors, which clients show to the model so it can correct the call): an
//! argument name the tool does not know (with the valid names and a did-you-mean), a wrong type, both or
//! neither of `yaml_content` / `yaml_path`, a missing file, a value outside an enumeration.
//!
//! Timeline files are read with a hard size cap and only when they are regular files, so a wrong
//! `yaml_path` (a media file, `/dev/zero`, a FIFO) fails fast instead of reading without bound.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use rmcp::model::JsonObject;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use soniccraft_core::SourceFormat;
use soniccraft_engine::TimelineSource;

use crate::paths;

/// Largest timeline file the server reads: core's document limit
/// ([`soniccraft_core::parse::MAX_DOCUMENT_BYTES`], 5 MiB). Reading stops one byte past it, so a larger
/// file (or an endless stream) is rejected without reading more than that.
const MAX_TIMELINE_FILE_BYTES: usize = soniccraft_core::parse::MAX_DOCUMENT_BYTES;

/// Values of the `detail` argument, as advertised in the input schema.
pub(crate) const DETAIL_VALUES: [&str; 3] = ["auto", "summary", "full"];

/// Values of the render `format` argument, as advertised in the input schema.
pub(crate) const FORMAT_VALUES: [&str; 6] = ["wav", "mp3", "flac", "ogg", "opus", "m4a"];

/// A tool call that cannot run because of its arguments. The message is shown to the model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArgError(pub(crate) String);

impl std::fmt::Display for ArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where the timeline comes from: shared by `soniccraft_validate_timeline`, `soniccraft_dry_run` and
/// `soniccraft_render`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct TimelineArgs {
    /// The timeline document itself, as YAML or JSON text (the same format as a .yaml/.json timeline
    /// file). Relative paths inside it (clip `source:` files, `output.path`) resolve against `base_dir`.
    /// Pass exactly one of `yaml_content` or `yaml_path`.
    #[serde(default)]
    #[schemars(with = "String")]
    pub(crate) yaml_content: Option<String>,
    /// Path of a timeline file (.yaml, .yml or .json). A relative path resolves against `base_dir`;
    /// absolute paths are safest. Paths inside the file resolve against the file's own directory. Pass
    /// exactly one of `yaml_content` or `yaml_path`.
    #[serde(default)]
    #[schemars(with = "String")]
    pub(crate) yaml_path: Option<String>,
    /// Synonym of `yaml_path`, accepted so either name works. Pass only one of the two.
    #[serde(default)]
    #[schemars(with = "String")]
    pub(crate) path: Option<String>,
    /// Directory that relative `yaml_path`, `output_path` and the paths inside `yaml_content` resolve
    /// against. Default: the server's base directory (`soniccraft mcp --base-dir`, else env
    /// CLAUDE_PROJECT_DIR, else the server's working directory).
    #[serde(default)]
    #[schemars(with = "String")]
    pub(crate) base_dir: Option<String>,
}

/// Arguments of `soniccraft_validate_timeline`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ValidateArgs {
    #[serde(flatten)]
    pub(crate) timeline: TimelineArgs,
    /// Also verify each provider's API key with one cheap authenticated request (network access, never
    /// synthesizes, costs nothing). Default false: keys are only checked for presence in the environment.
    #[serde(default)]
    #[schemars(with = "bool", extend("default" = false))]
    pub(crate) check_credentials: Option<bool>,
    /// How much of the report to return. `auto` (default): the whole report while it is small, else a
    /// compact one (counts and every error and warning, up to 50); `summary`: always the compact report;
    /// `full`: every diagnostic, however many.
    #[serde(default)]
    #[schemars(with = "String", extend("enum" = ["auto", "summary", "full"], "default" = "auto"))]
    pub(crate) detail: Option<String>,
}

/// Arguments of `soniccraft_dry_run`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct DryRunArgs {
    #[serde(flatten)]
    pub(crate) timeline: TimelineArgs,
    /// How much of the plan to return. `auto` (default): the whole PlanReport while it is small (about
    /// 12,000 characters, some 40 items), else a compact report with the totals, synthesis cost, ducking,
    /// errors and warnings, the first and last 10 items and every item the overlap policy moved or a
    /// diagnostic names (`items_total` and `items_truncated` say what was left out); `summary`: always
    /// the compact report; `full`: every item and the whole diagram.
    #[serde(default)]
    #[schemars(with = "String", extend("enum" = ["auto", "summary", "full"], "default" = "auto"))]
    pub(crate) detail: Option<String>,
}

/// Arguments of `soniccraft_render`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct RenderArgs {
    #[serde(flatten)]
    pub(crate) timeline: TimelineArgs,
    /// Output audio file; overrides `output.path` in the timeline. A relative path resolves against
    /// `base_dir`. Its extension (.wav, .mp3, .flac, .ogg, .opus, .m4a) selects the format unless the
    /// timeline sets `output.format` (pass `format` to override that too). Side files (subtitles,
    /// manifest, stems) are written next to it, named after its stem. Existing outputs are replaced.
    /// Without it, the timeline's own `output.path` must stay inside `base_dir`.
    #[serde(default)]
    #[schemars(with = "String")]
    pub(crate) output_path: Option<String>,
    /// Output format, overriding the timeline's `output.format`: wav, mp3 or flac (built in), or ogg,
    /// opus or m4a (these need ffmpeg). Pass the format that matches `output_path` (for example `mp3`
    /// with `out/episode.mp3`): otherwise a timeline that sets `format: wav` writes WAV data into the
    /// .mp3 file. Omit it to use `output.format`, else the output file's extension, else wav.
    #[serde(default)]
    #[schemars(with = "String", extend("enum" = ["wav", "mp3", "flac", "ogg", "opus", "m4a"]))]
    pub(crate) format: Option<String>,
    /// Override the timeline's `output.export_subtitles`: true writes subtitle files next to the output
    /// (WebVTT + SRT unless the timeline lists its own formats), false writes none. Omit to follow the
    /// timeline.
    #[serde(default)]
    #[schemars(with = "bool")]
    pub(crate) export_subtitles: Option<bool>,
    /// Ignore cached TTS audio and synthesize every cue again (results still refresh the cache). Costs
    /// provider credits; only use it when cached audio is stale. Default false.
    #[serde(default)]
    #[schemars(with = "bool", extend("default" = false))]
    pub(crate) no_cache: Option<bool>,
    /// How much of the render report to return. `auto` (default): the whole RenderReport while it is
    /// small, else a compact report with the output file, loudness, side files, synthesis, ducking,
    /// errors and warnings and the first and last 10 items (`items_total` and `items_truncated` say what
    /// was left out; the manifest file, when written, has every item); `summary`: always the compact
    /// report; `full`: every item with exact timings.
    #[serde(default)]
    #[schemars(with = "String", extend("enum" = ["auto", "summary", "full"], "default" = "auto"))]
    pub(crate) detail: Option<String>,
}

/// Arguments of tools that take none (`soniccraft_list_providers`).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct NoArgs {}

/// Arguments of `soniccraft_get_schema`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct SchemaArgs {
    /// How much to return. `auto` (default) and `summary`: the DSL quick reference, which covers every
    /// field, default and rule (the JSON Schema is left out while the whole result would be too large
    /// for an agent's context); `full`: the quick reference and the complete JSON Schema (draft 2020-12,
    /// about 35,000 characters).
    #[serde(default)]
    #[schemars(with = "String", extend("enum" = ["auto", "summary", "full"], "default" = "auto"))]
    pub(crate) detail: Option<String>,
}

/// Arguments of `soniccraft_list_voices`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct VoicesArgs {
    /// Built-in provider name or alias: elevenlabs, openai, cartesia, kokoro, piper, macos_say, command
    /// or mock (aliases: eleven_labs, 11labs, open_ai, local_kokoro, kokoro_fastapi, say, system).
    /// Instance names from a timeline's `providers:` block are not accepted; use the instance's `type`.
    pub(crate) provider: String,
}

/// Check each argument's JSON type, and that required arguments are present, against the tool's input
/// schema. Runs before deserialization because serde cannot name a field inside a flattened struct.
/// `null` counts as absent.
pub(crate) fn check_values(
    tool: &str,
    schema: &JsonObject,
    args: &JsonObject,
) -> Result<(), ArgError> {
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    for (name, value) in args {
        if value.is_null() {
            continue;
        }
        let expected = properties
            .and_then(|props| props.get(name))
            .and_then(|prop| prop.get("type"))
            .and_then(serde_json::Value::as_str);
        let Some(expected) = expected else { continue };
        let matches = match expected {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            _ => true,
        };
        if !matches {
            return Err(ArgError(format!(
                "Invalid argument `{name}` for `{tool}`: expected {}, got {}.",
                with_article(expected),
                describe(value)
            )));
        }
    }
    let required = schema
        .get("required")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str);
    for name in required {
        if args.get(name).is_none_or(serde_json::Value::is_null) {
            return Err(ArgError(format!(
                "Missing required argument `{name}` for `{tool}`."
            )));
        }
    }
    Ok(())
}

fn with_article(json_type: &str) -> String {
    match json_type {
        "integer" | "object" | "array" => format!("an {json_type}"),
        other => format!("a {other}"),
    }
}

fn describe(value: &serde_json::Value) -> String {
    let shown = |text: String| {
        if text.chars().count() > 40 {
            format!("{}…", text.chars().take(40).collect::<String>())
        } else {
            text
        }
    };
    match value {
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Bool(b) => format!("the boolean {b}"),
        serde_json::Value::Number(n) => format!("the number {n}"),
        serde_json::Value::String(s) => format!("the string {}", shown(format!("{s:?}"))),
        serde_json::Value::Array(_) => "an array".into(),
        serde_json::Value::Object(_) => "an object".into(),
    }
}

/// Deserialize a tool's argument object, naming the offending argument in the error.
pub(crate) fn parse<T: DeserializeOwned>(tool: &str, args: &JsonObject) -> Result<T, ArgError> {
    let value = serde_json::Value::Object(args.clone());
    serde_path_to_error::deserialize::<_, T>(value).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        if path.is_empty() || path == "." {
            ArgError(format!("Invalid arguments for `{tool}`: {inner}."))
        } else {
            ArgError(format!("Invalid argument `{path}` for `{tool}`: {inner}."))
        }
    })
}

/// Text argument with surrounding whitespace removed; empty strings count as absent (some clients send
/// `""` for every optional parameter).
fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// A timeline ready for the engine, plus what the summary should call it.
#[derive(Debug, Clone)]
pub(crate) struct LoadedTimeline {
    /// The engine input.
    pub(crate) source: TimelineSource,
    /// The directory relative tool arguments (`output_path`) resolve against.
    pub(crate) base_dir: PathBuf,
}

/// Which of `yaml_content` / `yaml_path` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimelineInput<'a> {
    Content(&'a str),
    Path(&'a str),
}

impl TimelineArgs {
    /// The directory relative arguments resolve against: `base_dir` (which must exist) or the server
    /// default.
    pub(crate) fn effective_base_dir(&self, default_base: &Path) -> Result<PathBuf, ArgError> {
        let Some(dir) = present(&self.base_dir) else {
            return Ok(default_base.to_path_buf());
        };
        let resolved = paths::resolve(default_base, dir);
        if resolved.is_dir() {
            Ok(resolved)
        } else if resolved.exists() {
            Err(ArgError(format!(
                "`base_dir` must be a directory, but {} is a file. Pass the directory that relative paths \
                 should resolve against.",
                resolved.display()
            )))
        } else {
            Err(ArgError(format!(
                "`base_dir` does not exist: {}{}. Pass an existing directory (absolute paths are safest).",
                resolved.display(),
                relative_note(dir, default_base)
            )))
        }
    }

    fn input(&self) -> Result<TimelineInput<'_>, ArgError> {
        let content = self
            .yaml_content
            .as_deref()
            .filter(|c| !c.trim().is_empty());
        let path = match (present(&self.yaml_path), present(&self.path)) {
            (Some(a), Some(b)) if a != b => {
                return Err(ArgError(format!(
                    "`path` is a synonym of `yaml_path`, and they disagree (yaml_path = {a:?}, path = {b:?}). \
                     Pass only one of them."
                )));
            }
            (a, b) => a.or(b),
        };
        match (content, path) {
            (Some(_), Some(_)) => Err(ArgError(
                "Pass exactly one of `yaml_content` (the timeline text) or `yaml_path` (a timeline file), \
                 not both."
                    .into(),
            )),
            (Some(content), None) => Ok(TimelineInput::Content(content)),
            (None, Some(path)) => Ok(TimelineInput::Path(path)),
            (None, None) => {
                let empty = self.yaml_content.is_some() || self.yaml_path.is_some() || self.path.is_some();
                Err(ArgError(format!(
                    "Missing timeline: pass `yaml_content` (the YAML or JSON document as a string) or \
                     `yaml_path` (path of a .yaml/.json timeline file){}.",
                    if empty { "; the value given was empty" } else { "" }
                )))
            }
        }
    }

    /// Resolve the arguments into an engine [`TimelineSource`]: read the file for `yaml_path`, or wrap
    /// `yaml_content` with the effective base directory.
    pub(crate) async fn load(&self, default_base: &Path) -> Result<LoadedTimeline, ArgError> {
        let base_dir = self.effective_base_dir(default_base)?;
        let source = match self.input()? {
            TimelineInput::Content(text) => {
                TimelineSource::from_text(text, base_dir.clone(), SourceFormat::Auto)
            }
            TimelineInput::Path(raw) => {
                let path = paths::resolve(&base_dir, raw);
                read_timeline_file(path, raw, &base_dir).await?
            }
        };
        Ok(LoadedTimeline { source, base_dir })
    }
}

/// Read a timeline file on the blocking pool, with precise errors.
async fn read_timeline_file(
    path: PathBuf,
    raw: &str,
    base_dir: &Path,
) -> Result<TimelineSource, ArgError> {
    let note = relative_note(raw, base_dir);
    let task_path = path.clone();
    let read = tokio::task::spawn_blocking(move || read_bounded(&task_path))
        .await
        .map_err(|error| ArgError(format!("Reading {} failed: {error}.", path.display())))?;
    let shown = path.display();
    let text = read.map_err(|problem| {
        ArgError(match problem {
            ReadProblem::Io(error) => match error.kind() {
                std::io::ErrorKind::NotFound => format!(
                    "Timeline file not found: {shown}{note}. Pass an absolute `yaml_path`, set `base_dir`, \
                     or send the document as `yaml_content`."
                ),
                std::io::ErrorKind::PermissionDenied => {
                    format!("Cannot read timeline file {shown}: permission denied.")
                }
                _ => format!("Cannot read timeline file {shown}: {error}."),
            },
            ReadProblem::Directory => {
                format!("`yaml_path` must be a timeline file, but {shown} is a directory.")
            }
            ReadProblem::NotAFile(kind) => format!(
                "`yaml_path` must be a regular timeline file, but {shown} is {kind}. Pass the path of a \
                 .yaml or .json file, or send the document as `yaml_content`."
            ),
            ReadProblem::TooLarge(size) => format!(
                "Timeline file {shown} is {}, but a timeline document is at most 5 MiB \
                 ({MAX_TIMELINE_FILE_BYTES} bytes). Check that `yaml_path` names the timeline (not an audio \
                 file), or split the composition into several timelines.",
                match size {
                    Some(size) => format!("{size} bytes"),
                    None => format!("more than {MAX_TIMELINE_FILE_BYTES} bytes"),
                },
            ),
            ReadProblem::NotUtf8(offset) => format!(
                "Cannot read timeline file {shown}: it is not UTF-8 text (invalid byte at offset {offset}). \
                 Timelines are UTF-8 YAML or JSON; check that `yaml_path` names the timeline, not an audio \
                 file."
            ),
        })
    })?;
    let base_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let format = SourceFormat::from_extension(path.extension().and_then(|e| e.to_str()));
    Ok(TimelineSource {
        text,
        format,
        path: Some(path),
        base_dir,
    })
}

/// Why a timeline file could not be read.
#[derive(Debug)]
enum ReadProblem {
    Io(std::io::Error),
    Directory,
    /// Not a regular file: "a character device", "a FIFO", ...
    NotAFile(&'static str),
    /// Larger than [`MAX_TIMELINE_FILE_BYTES`]; the size when the file system reports it.
    TooLarge(Option<u64>),
    /// Not UTF-8; the offset of the first invalid byte.
    NotUtf8(usize),
}

/// Read at most [`MAX_TIMELINE_FILE_BYTES`] of a regular file as UTF-8. The file type is checked before
/// opening it, so a FIFO (whose `open` would wait for a writer) or a device is never opened, and the
/// read itself stops one byte past the limit whatever the file system reports.
fn read_bounded(path: &Path) -> Result<String, ReadProblem> {
    let meta = std::fs::metadata(path).map_err(ReadProblem::Io)?;
    check_file_type(&meta)?;
    if meta.len() > MAX_TIMELINE_FILE_BYTES as u64 {
        return Err(ReadProblem::TooLarge(Some(meta.len())));
    }
    let file = std::fs::File::open(path).map_err(ReadProblem::Io)?;
    // The path may have been replaced since the check; trust only the opened file.
    check_file_type(&file.metadata().map_err(ReadProblem::Io)?)?;
    let mut bytes = Vec::with_capacity(meta.len() as usize + 1);
    file.take(MAX_TIMELINE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadProblem::Io)?;
    if bytes.len() > MAX_TIMELINE_FILE_BYTES {
        return Err(ReadProblem::TooLarge(None));
    }
    String::from_utf8(bytes).map_err(|error| ReadProblem::NotUtf8(error.utf8_error().valid_up_to()))
}

fn check_file_type(meta: &std::fs::Metadata) -> Result<(), ReadProblem> {
    let kind = meta.file_type();
    if kind.is_file() {
        return Ok(());
    }
    if kind.is_dir() {
        return Err(ReadProblem::Directory);
    }
    Err(ReadProblem::NotAFile(special_file_kind(&kind)))
}

#[cfg(unix)]
fn special_file_kind(kind: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt as _;
    if kind.is_char_device() {
        "a character device"
    } else if kind.is_block_device() {
        "a block device"
    } else if kind.is_fifo() {
        "a FIFO (named pipe)"
    } else if kind.is_socket() {
        "a socket"
    } else {
        "not a regular file"
    }
}

#[cfg(not(unix))]
fn special_file_kind(_kind: &std::fs::FileType) -> &'static str {
    "not a regular file"
}

/// " (resolved from `x` against base_dir /abs)" for relative inputs, else "".
fn relative_note(raw: &str, base: &Path) -> String {
    if Path::new(raw.trim()).is_absolute() {
        String::new()
    } else {
        format!(
            " (`{}` resolved against base_dir {})",
            raw.trim(),
            base.display()
        )
    }
}

/// Resolve `output_path` against the effective base directory.
pub(crate) fn output_path(raw: &Option<String>, base_dir: &Path) -> Option<PathBuf> {
    present(raw).map(|p| paths::resolve(base_dir, p))
}

/// How much of a report a result carries (the `detail` argument).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Detail {
    /// The whole report while it is small, else the compact report.
    #[default]
    Auto,
    /// Always the compact report.
    Summary,
    /// Everything, shortened only to fit the protocol's message size limit.
    Full,
}

/// The `detail` argument: `auto`, `summary` or `full` (case-insensitive); absent or empty is `auto`.
pub(crate) fn detail(tool: &str, raw: &Option<String>) -> Result<Detail, ArgError> {
    let Some(value) = present(raw) else {
        return Ok(Detail::Auto);
    };
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(Detail::Auto),
        "summary" => Ok(Detail::Summary),
        "full" => Ok(Detail::Full),
        _ => Err(ArgError(format!(
            "Invalid argument `detail` for `{tool}`: {}. Use \"auto\" (the default), \"summary\" or \"full\".",
            unknown_choice(value, &DETAIL_VALUES)
        ))),
    }
}

/// The render `format` argument as the engine's format name. Case, a leading `.` and the DSL's aliases
/// (`wave`, `vorbis`, `aac`, `mp4`) are accepted; absent or empty means no override.
pub(crate) fn output_format(tool: &str, raw: &Option<String>) -> Result<Option<String>, ArgError> {
    let Some(value) = present(raw) else {
        return Ok(None);
    };
    let normalized = value.trim_start_matches('.').to_ascii_lowercase();
    match soniccraft_core::ir::OutputFormat::parse(&normalized) {
        Some(format) => Ok(Some(format.as_str().to_string())),
        None => Err(ArgError(format!(
            "Invalid argument `format` for `{tool}`: {}. Use one of wav, mp3, flac (built in) or ogg, \
             opus, m4a (need ffmpeg).",
            unknown_choice(value, &FORMAT_VALUES)
        ))),
    }
}

/// "`verbose` is not one of …; did you mean `full`?" for an enumerated argument.
fn unknown_choice(value: &str, choices: &[&str]) -> String {
    let shown = describe(&serde_json::Value::String(value.to_string()));
    match soniccraft_core::diagnostics::did_you_mean(value, choices.iter().copied()) {
        Some(suggestion) => format!("got {shown}; did you mean \"{suggestion}\"?"),
        None => format!("got {shown}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(value: serde_json::Value) -> JsonObject {
        value.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn parse_reports_the_offending_argument() {
        let err =
            parse::<ValidateArgs>("t", &obj(json!({"check_credentials": "yes"}))).unwrap_err();
        assert!(err.0.contains("`check_credentials`"), "{err}");
        assert!(err.0.contains("boolean"), "{err}");

        let ok = parse::<RenderArgs>(
            "t",
            &obj(json!({"yaml_path": "a.yaml", "no_cache": null, "export_subtitles": true})),
        )
        .unwrap();
        assert_eq!(ok.timeline.yaml_path.as_deref(), Some("a.yaml"));
        assert_eq!(ok.no_cache, None);
        assert_eq!(ok.export_subtitles, Some(true));

        let err = parse::<VoicesArgs>("t", &obj(json!({}))).unwrap_err();
        assert!(err.0.contains("provider"), "{err}");
    }

    #[test]
    fn values_are_checked_against_the_schema() {
        let schema = crate::tools::input_schema::<RenderArgs>();
        let check = |v| check_values("soniccraft_render", &schema, &obj(v));
        let err = check(json!({"yaml_path": 42})).unwrap_err();
        assert_eq!(
            err.0,
            "Invalid argument `yaml_path` for `soniccraft_render`: expected a string, got the number 42."
        );
        let err = check(json!({"no_cache": "yes"})).unwrap_err();
        assert!(
            err.0.contains("`no_cache`") && err.0.contains("a boolean"),
            "{err}"
        );
        assert!(
            check(json!({"yaml_path": "a", "no_cache": null, "export_subtitles": false})).is_ok()
        );

        let schema = crate::tools::input_schema::<VoicesArgs>();
        let err = check_values("v", &schema, &obj(json!({}))).unwrap_err();
        assert_eq!(err.0, "Missing required argument `provider` for `v`.");
        let err = check_values("v", &schema, &obj(json!({"provider": null}))).unwrap_err();
        assert!(err.0.contains("Missing required"), "{err}");
    }

    #[test]
    fn exactly_one_timeline_input() {
        let args = |v| parse::<TimelineArgs>("t", &obj(v)).unwrap();
        assert_eq!(
            args(json!({"yaml_content": "version: 1"})).input(),
            Ok(TimelineInput::Content("version: 1"))
        );
        assert_eq!(
            args(json!({"path": " a.yaml "})).input(),
            Ok(TimelineInput::Path("a.yaml"))
        );
        assert_eq!(
            args(json!({"yaml_path": "a.yaml", "path": "a.yaml"})).input(),
            Ok(TimelineInput::Path("a.yaml"))
        );
        // Empty strings count as absent.
        assert_eq!(
            args(json!({"yaml_content": "", "yaml_path": "a.yaml", "base_dir": ""})).input(),
            Ok(TimelineInput::Path("a.yaml"))
        );
        let both = args(json!({"yaml_content": "x: 1", "yaml_path": "a.yaml"}))
            .input()
            .unwrap_err();
        assert!(both.0.contains("exactly one"), "{both}");
        let disagree = args(json!({"yaml_path": "a.yaml", "path": "b.yaml"}))
            .input()
            .unwrap_err();
        assert!(disagree.0.contains("synonym"), "{disagree}");
        let none = args(json!({})).input().unwrap_err();
        assert!(none.0.contains("Missing timeline"), "{none}");
        let empty = args(json!({"yaml_content": "  "})).input().unwrap_err();
        assert!(empty.0.contains("empty"), "{empty}");
    }

    #[tokio::test]
    async fn load_errors_are_precise() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();

        let missing = TimelineArgs {
            yaml_path: Some("nope.yaml".into()),
            ..Default::default()
        };
        let err = missing.load(base).await.unwrap_err();
        assert!(err.0.contains("not found"), "{err}");
        assert!(err.0.contains("nope.yaml"), "{err}");
        assert!(err.0.contains("resolved against base_dir"), "{err}");

        let is_dir = TimelineArgs {
            yaml_path: Some(base.display().to_string()),
            ..Default::default()
        };
        assert!(is_dir.load(base).await.unwrap_err().0.contains("directory"));

        let bad_base = TimelineArgs {
            yaml_content: Some("version: 1".into()),
            base_dir: Some("does/not/exist".into()),
            ..Default::default()
        };
        assert!(
            bad_base
                .load(base)
                .await
                .unwrap_err()
                .0
                .contains("does not exist")
        );

        std::fs::write(base.join("t.yaml"), "version: \"1.0\"\n").unwrap();
        let ok = TimelineArgs {
            yaml_path: Some("t.yaml".into()),
            ..Default::default()
        };
        let loaded = ok.load(base).await.unwrap();
        assert_eq!(
            loaded.source.path.as_deref(),
            Some(base.join("t.yaml").as_path())
        );
        assert_eq!(loaded.source.format, SourceFormat::Yaml);
        assert_eq!(loaded.base_dir, base);

        let inline = TimelineArgs {
            yaml_content: Some("{\"version\": \"1.0\"}".into()),
            ..Default::default()
        };
        let loaded = inline.load(base).await.unwrap();
        assert_eq!(loaded.source.path, None);
        assert_eq!(loaded.source.base_dir, base);
    }
}
