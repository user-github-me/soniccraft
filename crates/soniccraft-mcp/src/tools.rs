//! The tool catalogue: names, titles, LLM-facing descriptions, annotations and input schemas.
//!
//! Input schemas are generated from the argument structs in [`crate::args`] with schemars (draft 2020-12,
//! every subschema inlined: no `$ref`/`$defs`, no `$schema`, no root `title`), so the schema an agent sees
//! is exactly what the parser accepts. The list is built once and returned in a fixed order.

use std::sync::{Arc, LazyLock};

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::Value;

use crate::args::{DryRunArgs, NoArgs, RenderArgs, SchemaArgs, ValidateArgs, VoicesArgs};

/// Name of the validation tool.
pub const VALIDATE_TOOL: &str = "soniccraft_validate_timeline";
/// Name of the dry-run tool.
pub const DRY_RUN_TOOL: &str = "soniccraft_dry_run";
/// Name of the render tool.
pub const RENDER_TOOL: &str = "soniccraft_render";
/// Name of the schema tool.
pub const SCHEMA_TOOL: &str = "soniccraft_get_schema";
/// Name of the provider listing tool.
pub const PROVIDERS_TOOL: &str = "soniccraft_list_providers";
/// Name of the voice listing tool.
pub const VOICES_TOOL: &str = "soniccraft_list_voices";

/// Every tool name, in the order `tools/list` returns them.
pub const TOOL_NAMES: [&str; 6] = [
    VALIDATE_TOOL,
    DRY_RUN_TOOL,
    RENDER_TOOL,
    SCHEMA_TOOL,
    PROVIDERS_TOOL,
    VOICES_TOOL,
];

/// Which operation a tool performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Validate,
    DryRun,
    Render,
    GetSchema,
    ListProviders,
    ListVoices,
}

/// A tool definition plus the argument names its schema declares.
#[derive(Debug, Clone)]
pub(crate) struct ToolSpec {
    pub(crate) kind: ToolKind,
    pub(crate) tool: Tool,
    /// Property names of the input schema, in schema order: the only argument names accepted.
    pub(crate) arg_names: Vec<String>,
}

impl ToolSpec {
    pub(crate) fn name(&self) -> &str {
        &self.tool.name
    }
}

const VALIDATE_DESCRIPTION: &str = "Validate a SonicCraft audio timeline (YAML or JSON) without rendering. \
Parses it and checks every field, reference, timing anchor and the environment (asset files exist, TTS \
providers available, API keys present, ffmpeg for ogg/opus/m4a), reporting ALL problems in one pass. Call it \
after every edit, before soniccraft_dry_run and soniccraft_render. Pass exactly one of `yaml_content` (the \
document text) or `yaml_path` (a file; `path` also works). Returns a ValidationReport {valid, diagnostics, \
summary}; an invalid timeline is still a successful call, with valid: false. Each diagnostic has a \
severity, a stable `code`, a `path` such as tracks[1].sequence[0].start_relative.after_cue, the line and \
column, a `message` and usually a `hint` you can apply mechanically (rename `fade_in` to `fade_in_ms`, use a \
suggested id): fix every error, then validate again until valid is true. Warnings and infos do not block \
rendering. A report with many diagnostics comes back compact (every error and warning up to 50, plus \
counts); `detail: \"full\"` returns all. Never calls paid APIs; `check_credentials: true` sends one free \
authenticated request per provider to test its key. API keys only ever go to the provider's official API \
host, a server on this machine (the provider's own key variable only), or a host listed in \
SONICCRAFT_ALLOWED_HOSTS.";

const DRY_RUN_DESCRIPTION: &str = "Plan a SonicCraft timeline without synthesizing audio: free, never calls \
paid TTS APIs. Lays out every cue, pause and clip with cached or estimated speech durations (mock voices \
are measured exactly), applies the speech-overlap policy and computes the ducking regions. Returns a PlanReport: valid, diagnostics, \
duration_ms (estimated total length), items (id, kind, track_id, start_ms, end_ms, duration_source, and \
pushed_from_ms for cues the overlap policy moved), ducking regions per ducked track, synthesis (cues, \
cached, characters_to_synthesize, i.e. what a render will bill) and an ASCII timeline diagram, which the \
text result also shows. A long timeline (more than about 40 items) gets a compact report: totals, cost, \
ducking, errors and warnings, the diagram's track lanes, the first and last 10 items and every item the \
overlap policy moved or a diagnostic names (items_total and items_truncated say what was left out); pass \
`detail: \"full\"` for every item. Call it after validation passes and before soniccraft_render to check \
order, timing and cost; show the diagram to the user when timing matters. Estimates are within about \
±15%; a render measures exact durations and lays out again. Takes `yaml_content` or `yaml_path` like \
soniccraft_validate_timeline; an invalid timeline is a successful call with valid: false.";

const RENDER_DESCRIPTION: &str = "Render a SonicCraft timeline to a mastered audio file. Synthesizes the \
speech (cached by content hash, so re-renders only pay for changed cues), places every item \
sample-accurately, ducks music under speech, normalizes to the loudness target (EBU R128) with a \
true-peak limiter, encodes (wav, mp3, flac; ogg, opus and m4a need ffmpeg) and writes the subtitles, stems \
and manifest the timeline or arguments ask for. It writes files (inside the base directory unless \
`output_path` names another place) and may call paid TTS APIs; API keys go only to each provider's \
official host, servers on this machine (own key variable only), or SONICCRAFT_ALLOWED_HOSTS. Programs named \
by a timeline (`command` providers, a piper `command`) run only if the server was started with \
--allow-command. Run soniccraft_validate_timeline and soniccraft_dry_run first. Pass `yaml_content` or \
`yaml_path`; `output_path` chooses the file and `format` the encoding (`format: mp3` for an .mp3 path when \
the timeline sets another format). Returns a RenderReport: output {path (absolute), format, sample_rate, \
channels, duration_ms, bytes}, loudness {integrated_lufs vs target_lufs, true_peak_dbtp vs \
true_peak_target_dbtp, measured on the delivered file; for mp3, ogg, opus and m4a, pre_encode has the \
master's values}, files {subtitles, stems, manifest} with absolute paths, synthesis, diagnostics and \
items with exact timings (a long timeline lists the first and last 10; `detail: \"full\"` lists all), \
plus links to the files (audio is never inlined). A failure returns isError: true with {message, \
diagnostics}: fix what they name and render again. Long renders report progress, may first wait for \
another render to finish, and stop cleanly when the call is cancelled. Files are put in place only when \
the whole render succeeds: a failed or cancelled render removes its partial files and leaves earlier \
outputs untouched.";

const SCHEMA_DESCRIPTION: &str = "Get the SonicCraft timeline DSL (v1.0): a compact quick reference \
(document rules, every field with its default, timing and anchor rules, ducking, loudness presets, \
providers, a complete example and the common diagnostic fixes) as text. structuredContent is \
{dsl_version, schema_id, quick_reference}. Pass `detail: \"full\"` to also get the complete JSON Schema \
(draft 2020-12, about 35,000 characters) as text and as structuredContent.schema; the quick reference is \
enough to write timelines. Call it once before writing your first timeline; afterwards rely on \
soniccraft_validate_timeline, whose diagnostics name the exact field to fix.";

const PROVIDERS_DESCRIPTION: &str = "List the built-in TTS providers (elevenlabs, openai, cartesia, kokoro, \
piper, macos_say, command, mock) and whether each can be used right now: API key present in the \
environment, local binary found, local server answering (a quick probe of at most 300 ms), operating \
system supported. Returns {providers: [{name, aliases, display_name, local, word_timestamps, available, \
status, api_key_env, default_voice, default_model, notes}]}. `mock` (deterministic synthetic voice, any \
OS) and `macos_say` (macOS) need no API key. Call it before choosing `provider:` values. If a provider \
lacks its key, ask the user to export the environment variable named here; never put API keys in a \
timeline.";

const VOICES_DESCRIPTION: &str = "List the voices of one TTS provider, given its built-in name or alias \
(e.g. elevenlabs, openai, cartesia, kokoro, piper, macos_say, mock). Returns {provider, voices: [{id, name, \
language, gender, description}]}; put a voice's `id` into `voice_id` in the timeline. Cloud providers are \
queried over their API (needs the provider's API key in the environment); local providers list what is \
installed.";

/// All tools, in a fixed order.
pub(crate) fn specs() -> &'static [ToolSpec] {
    static SPECS: LazyLock<Vec<ToolSpec>> = LazyLock::new(build_specs);
    &SPECS
}

/// Look a tool up by name.
pub(crate) fn find(name: &str) -> Option<&'static ToolSpec> {
    specs().iter().find(|spec| spec.name() == name)
}

fn build_specs() -> Vec<ToolSpec> {
    vec![
        spec::<ValidateArgs>(
            ToolKind::Validate,
            VALIDATE_TOOL,
            "Validate timeline",
            VALIDATE_DESCRIPTION,
            // Reads files and env; with check_credentials it contacts the provider's API.
            annotations("Validate timeline", true, false, true, true),
        ),
        spec::<DryRunArgs>(
            ToolKind::DryRun,
            DRY_RUN_TOOL,
            "Dry-run timeline",
            DRY_RUN_DESCRIPTION,
            annotations("Dry-run timeline", true, false, true, false),
        ),
        spec::<RenderArgs>(
            ToolKind::Render,
            RENDER_TOOL,
            "Render timeline",
            RENDER_DESCRIPTION,
            // Writes (and overwrites) files; the same arguments produce the same bytes; calls TTS APIs.
            annotations("Render timeline", false, true, true, true),
        ),
        spec::<SchemaArgs>(
            ToolKind::GetSchema,
            SCHEMA_TOOL,
            "Get timeline schema",
            SCHEMA_DESCRIPTION,
            annotations("Get timeline schema", true, false, true, false),
        ),
        spec::<NoArgs>(
            ToolKind::ListProviders,
            PROVIDERS_TOOL,
            "List TTS providers",
            PROVIDERS_DESCRIPTION,
            annotations("List TTS providers", true, false, true, false),
        ),
        spec::<VoicesArgs>(
            ToolKind::ListVoices,
            VOICES_TOOL,
            "List provider voices",
            VOICES_DESCRIPTION,
            annotations("List provider voices", true, false, true, true),
        ),
    ]
}

fn annotations(
    title: &str,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
) -> ToolAnnotations {
    ToolAnnotations::with_title(title)
        .read_only(read_only)
        .destructive(destructive)
        .idempotent(idempotent)
        .open_world(open_world)
}

fn spec<T: JsonSchema>(
    kind: ToolKind,
    name: &'static str,
    title: &str,
    description: &'static str,
    annotations: ToolAnnotations,
) -> ToolSpec {
    let schema = input_schema::<T>();
    let arg_names = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();
    let tool = Tool::new(name, description, Arc::new(schema))
        .with_title(title)
        .with_annotations(annotations);
    ToolSpec {
        kind,
        tool,
        arg_names,
    }
}

/// An inlined draft 2020-12 object schema for `T`, shaped for MCP `inputSchema`.
pub(crate) fn input_schema<T: JsonSchema>() -> JsonObject {
    let generator = SchemaSettings::draft2020_12()
        .with(|settings| settings.inline_subschemas = true)
        .into_generator();
    let mut schema = match generator.into_root_schema_for::<T>().to_value() {
        Value::Object(map) => map,
        // schemars always produces an object for a struct; fall back to an open object schema.
        _ => JsonObject::new(),
    };
    // `$schema` is optional (MCP defaults to 2020-12) and trips some LLM tool-schema validators; the root
    // `title` is the Rust struct name, and the description is the tool's.
    for key in ["$schema", "title", "description"] {
        schema.remove(key);
    }
    schema.insert("type".into(), Value::String("object".into()));
    if !schema.contains_key("properties") {
        schema.insert("properties".into(), Value::Object(JsonObject::new()));
    }
    schema.insert("additionalProperties".into(), Value::Bool(false));
    if let Some(Value::Object(props)) = schema.get_mut("properties") {
        for prop in props.values_mut() {
            if let Value::Object(prop) = prop {
                // `Option<…>` fields documented with an explicit type never advertise `null` defaults.
                if prop.get("default") == Some(&Value::Null) {
                    prop.remove("default");
                }
                if let Some(Value::String(description)) = prop.get_mut("description") {
                    *description = flow(description);
                }
            }
        }
    }
    if schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        schema.remove("required");
    }
    schema
}

/// Doc-comment text as flowing prose: line breaks inside a paragraph become spaces.
fn flow(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|paragraph| !paragraph.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_ordered() {
        let names: Vec<&str> = specs().iter().map(ToolSpec::name).collect();
        assert_eq!(names, TOOL_NAMES);
        for name in TOOL_NAMES {
            assert!(find(name).is_some());
            assert!(name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
        assert!(find("soniccraft_nope").is_none());
    }

    #[test]
    fn schemas_are_inlined_described_objects() {
        for spec in specs() {
            let schema = Value::Object(spec.tool.input_schema.as_ref().clone());
            let text = schema.to_string();
            assert!(!text.contains("$ref") && !text.contains("$defs"), "{text}");
            assert!(!text.contains("\"$schema\""), "{text}");
            assert_eq!(schema["type"], "object", "{}", spec.name());
            assert_eq!(schema["additionalProperties"], false, "{}", spec.name());
            assert!(schema.get("title").is_none(), "{}", spec.name());
            for (name, prop) in schema["properties"].as_object().unwrap() {
                let description = prop["description"].as_str().unwrap_or_default();
                assert!(
                    description.len() > 20,
                    "{}.{name} lacks a description",
                    spec.name()
                );
                assert!(
                    !description.contains('\n'),
                    "{}.{name}: {description:?}",
                    spec.name()
                );
                assert!(
                    prop["type"].is_string(),
                    "{}.{name} must have one plain type: {prop}",
                    spec.name()
                );
            }
        }
    }

    #[test]
    fn argument_names_match_the_documented_contract() {
        let names = |tool| find(tool).unwrap().arg_names.clone();
        assert_eq!(
            names(VALIDATE_TOOL),
            [
                "yaml_content",
                "yaml_path",
                "path",
                "base_dir",
                "check_credentials",
                "detail"
            ]
        );
        assert_eq!(
            names(DRY_RUN_TOOL),
            ["yaml_content", "yaml_path", "path", "base_dir", "detail"]
        );
        assert_eq!(
            names(RENDER_TOOL),
            [
                "yaml_content",
                "yaml_path",
                "path",
                "base_dir",
                "output_path",
                "format",
                "export_subtitles",
                "no_cache",
                "detail"
            ]
        );
        let schema = |tool| Value::Object(find(tool).unwrap().tool.input_schema.as_ref().clone());
        let render = schema(RENDER_TOOL);
        assert_eq!(
            render["properties"]["format"]["enum"],
            serde_json::json!(crate::args::FORMAT_VALUES)
        );
        for tool in [VALIDATE_TOOL, DRY_RUN_TOOL, RENDER_TOOL, SCHEMA_TOOL] {
            let detail = &schema(tool)["properties"]["detail"];
            assert_eq!(
                detail["enum"],
                serde_json::json!(crate::args::DETAIL_VALUES),
                "{tool}"
            );
            assert_eq!(detail["default"], "auto", "{tool}");
            assert_eq!(detail["type"], "string", "{tool}");
        }
        assert_eq!(names(SCHEMA_TOOL), ["detail"]);
        assert!(names(PROVIDERS_TOOL).is_empty());
        assert_eq!(names(VOICES_TOOL), ["provider"]);
        let voices = Value::Object(
            find(VOICES_TOOL)
                .unwrap()
                .tool
                .input_schema
                .as_ref()
                .clone(),
        );
        assert_eq!(voices["required"], serde_json::json!(["provider"]));
    }

    #[test]
    fn voices_description_names_every_builtin_provider() {
        let schema = find(VOICES_TOOL).unwrap().tool.input_schema.clone();
        let description = schema["properties"]["provider"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        for &provider in soniccraft_core::providers::all() {
            let name = provider.to_string();
            assert!(PROVIDERS_DESCRIPTION.contains(&name), "{name}");
        }
        for spelling in soniccraft_core::providers::builtin_names() {
            assert!(
                description.contains(spelling),
                "{spelling} missing from: {description}"
            );
        }
    }

    #[test]
    fn descriptions_state_what_render_may_do() {
        for text in [
            RENDER_DESCRIPTION,
            VALIDATE_DESCRIPTION,
            DRY_RUN_DESCRIPTION,
        ] {
            assert!(text.len() <= 2048, "{} chars: {text}", text.len());
        }
        for fact in [
            "writes files",
            "inside the base directory unless `output_path`",
            "paid TTS APIs",
            "--allow-command",
            "SONICCRAFT_ALLOWED_HOSTS",
            "`format`",
            "detail",
            // The engine's report and commit semantics.
            "measured on the delivered file",
            "pre_encode",
            "leaves earlier outputs untouched",
        ] {
            assert!(RENDER_DESCRIPTION.contains(fact), "{fact}");
        }
        assert!(PROVIDERS_DESCRIPTION.contains("local server answering (a quick probe"));
        assert!(VALIDATE_DESCRIPTION.contains("SONICCRAFT_ALLOWED_HOSTS"));
        assert!(DRY_RUN_DESCRIPTION.contains("detail: \"full\""));
    }

    #[test]
    fn annotations_are_explicit() {
        for spec in specs() {
            let a = spec.tool.annotations.as_ref().unwrap();
            assert!(a.title.is_some());
            assert!(a.read_only_hint.is_some() && a.destructive_hint.is_some());
            assert!(a.idempotent_hint.is_some() && a.open_world_hint.is_some());
        }
        let render = find(RENDER_TOOL).unwrap().tool.annotations.clone().unwrap();
        assert_eq!(render.read_only_hint, Some(false));
        assert_eq!(render.open_world_hint, Some(true));
        let dry = find(DRY_RUN_TOOL)
            .unwrap()
            .tool
            .annotations
            .clone()
            .unwrap();
        assert_eq!(dry.read_only_hint, Some(true));
        assert_eq!(dry.open_world_hint, Some(false));
    }
}
