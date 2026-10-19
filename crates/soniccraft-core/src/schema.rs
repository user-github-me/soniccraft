//! JSON Schema for the timeline DSL, generated from [`crate::model`].
//!
//! The raw schemars output (draft 2020-12) is post-processed so the schema is a precise contract for
//! agents and editors that agrees with [`crate::compile`] on the structure of a timeline:
//!
//! - `$id`, title and description on the root;
//! - every object is closed (`additionalProperties: false`) except the free-form `settings` maps, and
//!   accepts annotation keys through `patternProperties: {"^x[-_]": {}}` (also on the `speakers` and
//!   `providers` name maps);
//! - `null` means "absent", as in the parser (`fade_in_ms:` without a value): optional keys accept it,
//!   required keys never do, and every presence rule below counts only non-null values;
//! - required keys: the root (`version`, `tracks`), tracks (`id`, `type`), speech tracks (`sequence`),
//!   music/sfx tracks (`source` or `clips`), cues (`text`), clips (`source`), ducking (`trigger_tracks`),
//!   providers not named after a built-in provider (`type`) and command providers (`command`);
//! - conflicts: `start_at_ms` with `start_relative`; more than one of `duration_ms` / `end_at_ms` /
//!   `end_relative`; a `clips` list next to single-clip fields; a `before_cue` start (which fixes the end)
//!   with an end or an open-ended loop; anchors need exactly one of `after_cue` / `with_cue` /
//!   `before_cue` (`end_relative` uses an `EndAnchor` without `before_cue`); sequence items are either a
//!   pause (only `pause_duration` or `pause`) or a cue;
//! - field applicability: audio fields on speech tracks and speech fields on music/sfx tracks;
//! - time values: numbers within 24 hours, strings in the grammar of [`crate::values::parse_duration`]
//!   (lowercase units), non-negative everywhere except `offset_ms`, greater than 0 for lengths;
//! - enum-valued strings list every accepted spelling (canonical names first, then aliases), exactly as
//!   the compiler accepts them (case and separators matter);
//! - value ranges, id patterns and non-empty strings mirror the checks of [`crate::lower`], including the
//!   ones that only apply to lossy output formats (`bitrate`) or to some provider types (`api_key_env`,
//!   `base_url`, `command`);
//! - schemars' non-standard numeric `format` annotations (`uint32`, `double`, …) are removed.
//!
//! Any document that [`crate::compile`] accepts without errors validates against this schema, and every
//! structural error of the compiler is a schema error too (tested on the fixtures, the examples, the skill
//! templates and the differential corpus of `tests/parse_schema_agreement.rs`). What the schema cannot
//! express is only checked by the compiler: references between ids, duration strings longer than 24 hours,
//! the extension of `output.path`, format-specific sample rates and bit depths, and secrets.

use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

use crate::fields::{self, Kind, Obj};
use crate::model::{
    DuckingModeName, FadeCurveName, LoudnessPresetName, OutputFormatName, OverlapPolicyName,
    ProviderTypeName, SubtitleFormatName, Timeline, TrackKindName,
};
use crate::values::{TimeSign, millis_schema};

/// The canonical `$id` of the published schema.
pub const SCHEMA_ID: &str = "https://raw.githubusercontent.com/user-github-me/soniccraft/main/skills/soniccraft/reference/timeline-schema.json";

/// Title of the published schema.
pub const SCHEMA_TITLE: &str = "SonicCraft timeline";

/// Regular expression every track, cue and clip id must match (DESIGN.md §2.1).
pub const ID_PATTERN: &str = "^[A-Za-z0-9_][A-Za-z0-9_.:-]*$";

/// Pattern of annotation keys (`x-…`, `x_…`), which are accepted and ignored on every object.
pub const ANNOTATION_PATTERN: &str = "^x[-_]";

/// Pattern of strings that are not empty or blank (the compiler trims them).
const NON_BLANK: &str = "\\S";

const ROOT_DESCRIPTION: &str = "A SonicCraft timeline (DSL v1.0): a declarative multi-track audio composition of \
    text-to-speech cues, music beds and sound effects that SonicCraft compiles into a sidechain-ducked, \
    loudness-mastered audio file with optional WebVTT/SRT subtitles. Keys are snake_case; unknown keys are \
    errors unless they start with `x-` or `x_` (annotations); `null` means the key is absent. Times accept \
    milliseconds (350) or strings with lowercase units such as \"350ms\", \"1.5s\" or \"00:01:30.500\".";

/// The timeline JSON Schema (draft 2020-12) as a JSON value.
pub fn timeline_schema() -> serde_json::Value {
    let generator = SchemaSettings::draft2020_12().into_generator();
    let raw = generator.into_root_schema_for::<Timeline>().to_value();
    let Value::Object(mut root) = raw else {
        // schemars always produces an object schema for a struct; keep the raw value otherwise.
        return raw;
    };
    let mut defs = match root.shift_remove("$defs") {
        Some(Value::Object(defs)) => defs,
        _ => Map::new(),
    };

    // ---- objects: close, allow annotations, ranges --------------------------------------------------
    close_object(&mut root);
    for (name, def) in defs.iter_mut() {
        let Value::Object(def) = def else { continue };
        if def.contains_key("properties") {
            close_object(def);
        }
        strip_numeric_formats(def);
        apply_constraints(name, def);
    }
    strip_numeric_formats(&mut root);
    time_values(&mut defs);

    // ---- the root -----------------------------------------------------------------------------------
    if let Some(Value::Object(props)) = root.get_mut("properties") {
        // Name maps: entries may be null (`narrator:` registers a speaker without settings), annotation
        // keys are allowed next to the names, and the whole map may be null.
        if let Some(Value::Object(speakers)) = props.get_mut("speakers") {
            speakers.insert("type".into(), json!(["object", "null"]));
            speakers.insert(
                "additionalProperties".into(),
                json!({ "anyOf": [{ "$ref": "#/$defs/VoiceSpec" }, { "type": "null" }] }),
            );
            speakers.insert(
                "patternProperties".into(),
                json!({ ANNOTATION_PATTERN: {} }),
            );
        }
        if let Some(Value::Object(providers)) = props.get_mut("providers") {
            providers.insert("type".into(), json!(["object", "null"]));
            providers.insert("patternProperties".into(), provider_name_patterns());
            // Any other instance name needs a `type`.
            providers.insert(
                "additionalProperties".into(),
                json!({ "allOf": [{ "$ref": "#/$defs/ProviderSpec" }, present("type")] }),
            );
        }
        if let Some(Value::Object(tracks)) = props.get_mut("tracks") {
            tracks.insert("minItems".into(), json!(1));
        }
        if let Some(version) = props.get_mut("version") {
            strip_null(version);
        }
    }
    root.insert("required".into(), json!(["version", "tracks"]));

    // ---- name enums: every accepted spelling -------------------------------------------------------
    for (def_name, spellings) in [
        ("TrackKindName", TrackKindName::SPELLINGS),
        ("OutputFormatName", OutputFormatName::SPELLINGS),
        ("LoudnessPresetName", LoudnessPresetName::SPELLINGS),
        ("OverlapPolicyName", OverlapPolicyName::SPELLINGS),
        ("FadeCurveName", FadeCurveName::SPELLINGS),
        ("DuckingModeName", DuckingModeName::SPELLINGS),
        ("SubtitleFormatName", SubtitleFormatName::SPELLINGS),
        ("ProviderTypeName", ProviderTypeName::SPELLINGS),
    ] {
        if let Some(Value::Object(def)) = defs.get_mut(def_name) {
            def.insert("enum".into(), json!(spellings));
        }
    }
    // `export_subtitles` names are validated during lowering (with did-you-mean); the schema lists them.
    if let Some(Value::Object(def)) = defs.get_mut("SubtitleExport") {
        let names = json!(SubtitleFormatName::SPELLINGS);
        if let Some(Value::Array(branches)) = def.get_mut("anyOf") {
            for branch in branches {
                match branch.get("type").and_then(Value::as_str) {
                    Some("string") => {
                        branch["enum"] = names.clone();
                    }
                    Some("array") => {
                        branch["items"] = json!({ "type": "string", "enum": names.clone() });
                    }
                    _ => {}
                }
            }
        }
    }

    // ---- anchors -------------------------------------------------------------------------------------
    if let Some(Value::Object(anchor)) = defs.get("Anchor").cloned() {
        let mut end_anchor = anchor.clone();
        if let Some(Value::Object(props)) = end_anchor.get_mut("properties") {
            props.insert(
                "before_cue".into(),
                json!({
                    "type": "null",
                    "description": "Not valid in `end_relative` (`before_cue` is only valid in `start_relative`)."
                }),
            );
        }
        end_anchor.insert(
            "description".into(),
            json!("A relative end time. Exactly one of `after_cue` (end = end(ref) + offset) or `with_cue` (end = start(ref) + offset); `before_cue` is only valid in `start_relative`."),
        );
        end_anchor.insert(
            "oneOf".into(),
            json!([present("after_cue"), present("with_cue")]),
        );
        defs.insert("EndAnchor".into(), Value::Object(end_anchor));
    }
    if let Some(Value::Object(anchor)) = defs.get_mut("Anchor") {
        anchor.insert(
            "oneOf".into(),
            json!([
                present("after_cue"),
                present("with_cue"),
                present("before_cue")
            ]),
        );
    }
    for def_name in ["Track", "Clip"] {
        if let Some(prop) = defs
            .get_mut(def_name)
            .and_then(|d| d.get_mut("properties"))
            .and_then(|p| p.get_mut("end_relative"))
        {
            retarget_ref(prop, "#/$defs/Anchor", "#/$defs/EndAnchor");
        }
    }

    // ---- required keys, conflicts and field applicability ------------------------------------------
    require(&mut defs, "Track", &["id", "type"]);
    require(&mut defs, "Clip", &["source"]);
    require(&mut defs, "DuckingSpec", &["trigger_tracks"]);
    if let Some(Value::Object(track)) = defs.get_mut("Track") {
        track.insert("allOf".into(), track_rules());
    }
    if let Some(Value::Object(clip)) = defs.get_mut("Clip") {
        clip.insert("allOf".into(), Value::Array(clip_rules()));
    }
    if let Some(Value::Object(item)) = defs.get_mut("SequenceItem") {
        item.insert(
            "oneOf".into(),
            json!([
                {
                    "title": "Pause",
                    "description": "A pause: exactly one of `pause_duration` or `pause`, and nothing else.",
                    "oneOf": [present("pause_duration"), present("pause")],
                    "properties": { "pause_duration": true, "pause": true },
                    "patternProperties": { ANNOTATION_PATTERN: true },
                    "additionalProperties": { "type": "null" }
                },
                {
                    "title": "Cue",
                    "description": "A speech cue: requires `text`; pause keys are not allowed; at most one of `start_at_ms` / `start_relative`.",
                    "allOf": [
                        present("text"),
                        absent(&["pause_duration", "pause"]),
                        at_most_one(&["start_at_ms", "start_relative"])
                    ]
                }
            ]),
        );
    }
    output_rules(&mut defs);
    provider_rules(&mut defs);

    // ---- assemble in a stable, readable key order ----------------------------------------------------
    let mut out = Map::new();
    out.insert(
        "$schema".into(),
        root.shift_remove("$schema")
            .unwrap_or_else(|| json!("https://json-schema.org/draft/2020-12/schema")),
    );
    out.insert("$id".into(), json!(SCHEMA_ID));
    out.insert("title".into(), json!(SCHEMA_TITLE));
    out.insert("description".into(), json!(ROOT_DESCRIPTION));
    root.shift_remove("title");
    root.shift_remove("description");
    for (key, value) in root {
        out.insert(key, value);
    }
    out.insert("$defs".into(), Value::Object(defs));
    Value::Object(out)
}

/// The schema pretty-printed with a trailing newline (the exact content of
/// `skills/soniccraft/reference/timeline-schema.json`).
pub fn timeline_schema_pretty() -> String {
    // Serializing a `serde_json::Value` cannot fail (all keys are strings, numbers are finite).
    let mut s = serde_json::to_string_pretty(&timeline_schema()).unwrap_or_default();
    s.push('\n');
    s
}

// ---- presence ---------------------------------------------------------------------------------------

/// `key` is present with a non-null value (the parser reads `null` as an absent key).
fn present(key: &str) -> Value {
    json!({ "required": [key], "properties": { key: { "not": { "type": "null" } } } })
}

/// None of `keys` is present (each is missing or null).
fn absent(keys: &[&str]) -> Value {
    let props: Map<String, Value> = keys
        .iter()
        .map(|k| (k.to_string(), json!({ "type": "null" })))
        .collect();
    json!({ "properties": props })
}

/// At most one of `keys` is present.
fn at_most_one(keys: &[&str]) -> Value {
    let mut options = Vec::new();
    for skip in 0..keys.len() {
        let others: Vec<&str> = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .map(|(_, k)| *k)
            .collect();
        options.push(absent(&others));
    }
    json!({ "anyOf": options })
}

/// The item starts with a `before_cue` anchor (which fixes where it ends).
fn before_cue_start() -> Value {
    json!({
        "required": ["start_relative"],
        "properties": { "start_relative": {
            "not": { "type": "null" },
            "required": ["before_cue"],
            "properties": { "before_cue": { "not": { "type": "null" } } }
        } }
    })
}

// ---- conditional rules ----------------------------------------------------------------------------

/// Rules of clips and of the single-clip shorthand of tracks: one start rule, at most one length rule,
/// and no end rule or open-ended loop after a `before_cue` start.
fn clip_rules() -> Vec<Value> {
    vec![
        at_most_one(&["start_at_ms", "start_relative"]),
        at_most_one(&["duration_ms", "end_at_ms", "end_relative"]),
        json!({ "if": before_cue_start(), "then": absent(&["end_at_ms", "end_relative"]) }),
        json!({
            "if": { "allOf": [
                before_cue_start(),
                { "required": ["loop"], "properties": { "loop": { "const": true } } }
            ] },
            "then": present("duration_ms")
        }),
    ]
}

/// Track rules: content and field applicability by type, `clips` or the shorthand, and [`clip_rules`].
fn track_rules() -> Value {
    let names = |fields: &[fields::Field]| fields.iter().map(|(n, _)| *n).collect::<Vec<_>>();
    let audio = names(fields::TRACK_AUDIO);
    let speech = names(fields::TRACK_SPEECH);
    let shorthand: Vec<&str> = audio.iter().copied().filter(|n| *n != "clips").collect();
    let mut rules = vec![
        json!({
            "if": { "required": ["type"], "properties": { "type": { "const": "speech" } } },
            "then": { "allOf": [present("sequence"), absent(&audio)] }
        }),
        json!({
            "if": { "required": ["type"], "properties": { "type": { "enum": ["music", "sfx"] } } },
            "then": { "allOf": [
                { "anyOf": [present("source"), present("clips")] },
                absent(&speech)
            ] }
        }),
        json!({ "anyOf": [absent(&["clips"]), absent(&shorthand)] }),
    ];
    rules.extend(clip_rules());
    Value::Array(rules)
}

/// `output`: `bitrate` is only read for lossy formats (wav and flac ignore it with a warning).
fn output_rules(defs: &mut Map<String, Value>) {
    let Some(Value::Object(output)) = defs.get_mut("OutputSpec") else {
        return;
    };
    if let Some(Value::Object(bitrate)) = output
        .get_mut("properties")
        .and_then(|p| p.get_mut("bitrate"))
    {
        bitrate.shift_remove("anyOf");
        bitrate.insert("type".into(), json!(["number", "string", "null"]));
    }
    let lossy: Vec<&str> = OutputFormatName::SPELLINGS
        .iter()
        .copied()
        .filter(|s| {
            matches!(
                OutputFormatName::parse(s),
                Some(
                    OutputFormatName::Mp3
                        | OutputFormatName::Ogg
                        | OutputFormatName::Opus
                        | OutputFormatName::M4a
                )
            )
        })
        .collect();
    // The output path's extension selects the format when `format` is not set (case-insensitively).
    let extensions: Vec<String> = lossy.iter().map(|s| case_insensitive(s)).collect();
    let lossy_extension = format!("[^/\\\\]\\.({})$", extensions.join("|"));
    output.insert(
        "allOf".into(),
        json!([{
            "if": { "anyOf": [
                { "required": ["format"], "properties": { "format": { "enum": lossy } } },
                { "allOf": [
                    absent(&["format"]),
                    { "required": ["path"], "properties": { "path": { "type": "string", "pattern": lossy_extension } } }
                ] }
            ] },
            "then": { "properties": { "bitrate": { "anyOf": [{ "$ref": "#/$defs/Bitrate" }, { "type": "null" }] } } }
        }]),
    );
}

/// `mp3` → `[mM][pP]3`.
fn case_insensitive(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphabetic() {
                format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// The built-in provider types, in declaration order.
#[cfg(test)]
fn provider_types() -> Vec<ProviderTypeName> {
    ProviderTypeName::NAMES
        .iter()
        .filter_map(|n| ProviderTypeName::parse(n))
        .collect()
}

/// Every spelling of one provider type (its name and aliases).
fn spellings_of(t: ProviderTypeName) -> Vec<&'static str> {
    ProviderTypeName::SPELLINGS
        .iter()
        .copied()
        .filter(|s| ProviderTypeName::parse(s) == Some(t))
        .collect()
}

/// Provider types grouped by the settings that only they check (other types ignore those settings with a
/// warning): the `$defs` entry with the rules, whether an entry named after the type may be null, and the
/// types.
fn provider_groups() -> [(Option<&'static str>, bool, Vec<ProviderTypeName>); 4] {
    use ProviderTypeName as P;
    [
        (
            Some("HttpProviderSettings"),
            true,
            vec![P::Elevenlabs, P::Openai, P::Cartesia, P::Kokoro],
        ),
        // A command provider needs a `command`, so an entry named `command` cannot be null.
        (Some("CommandProviderSettings"), false, vec![P::Command]),
        (Some("PiperProviderSettings"), true, vec![P::Piper]),
        (None, true, vec![P::MacosSay, P::Mock]),
    ]
}

/// The rules of the provider groups as `$defs`, and `ProviderSpec` applying them by `type`.
fn provider_rules(defs: &mut Map<String, Value>) {
    let argv = json!({ "minItems": 1, "prefixItems": [{ "pattern": NON_BLANK }] });
    defs.insert(
        "HttpProviderSettings".into(),
        json!({
            "description": "Checks of the HTTP providers (elevenlabs, openai, cartesia, kokoro).",
            "properties": {
                "api_key_env": { "pattern": "^\\s*[A-Za-z_][A-Za-z0-9_]*\\s*$" },
                "base_url": { "allOf": [
                    { "pattern": "^\\s*https?://[^/]" },
                    { "pattern": "^\\s*https?://.*\\S" }
                ] }
            }
        }),
    );
    defs.insert(
        "CommandProviderSettings".into(),
        json!({
            "description": "Checks of `command` providers: an argv template that starts with the program.",
            "allOf": [present("command")],
            "properties": { "command": argv.clone() }
        }),
    );
    defs.insert(
        "PiperProviderSettings".into(),
        json!({
            "description": "Checks of `piper` providers: a custom argv template starts with the program.",
            "properties": { "command": argv }
        }),
    );
    let rules: Vec<Value> = provider_groups()
        .into_iter()
        .filter_map(|(def, _, types)| {
            let spellings: Vec<&str> = types.into_iter().flat_map(spellings_of).collect();
            Some(json!({
                "if": { "required": ["type"], "properties": { "type": { "enum": spellings } } },
                "then": { "$ref": format!("#/$defs/{}", def?) }
            }))
        })
        .collect();
    if let Some(Value::Object(provider)) = defs.get_mut("ProviderSpec") {
        provider.insert("allOf".into(), Value::Array(rules));
    }
}

/// `patternProperties` of the `providers` map: annotations, and the built-in provider names, which may omit
/// `type` (the name is the type, and its group's rules apply).
fn provider_name_patterns() -> Value {
    let mut patterns = Map::new();
    patterns.insert(ANNOTATION_PATTERN.into(), json!({}));
    for (def, nullable, types) in provider_groups() {
        let names: Vec<&str> = types.into_iter().flat_map(spellings_of).collect();
        let entry = match def {
            Some(def) => json!({ "allOf": [
                { "$ref": "#/$defs/ProviderSpec" },
                { "if": absent(&["type"]), "then": { "$ref": format!("#/$defs/{def}") } }
            ] }),
            None => json!({ "$ref": "#/$defs/ProviderSpec" }),
        };
        let entry = if nullable {
            json!({ "anyOf": [entry, { "type": "null" }] })
        } else {
            entry
        };
        patterns.insert(format!("^({})$", names.join("|")), entry);
    }
    Value::Object(patterns)
}

// ---- time values ----------------------------------------------------------------------------------

/// The sign rule of a time field, as lowering checks it (DESIGN.md §2.1): only `offset_ms` may be negative;
/// lengths must be greater than 0.
fn time_sign(obj: Obj, key: &str) -> TimeSign {
    match (obj, key) {
        (Obj::Anchor, "offset_ms") => TimeSign::Any,
        (Obj::Track | Obj::Clip | Obj::Output, "duration_ms")
        | (Obj::Subtitles, "max_duration_ms") => TimeSign::Positive,
        _ => TimeSign::NonNegative,
    }
}

/// `Millis` (signed, for `offset_ms`), `NonNegativeMillis` and `PositiveMillis` definitions, each time
/// field pointing at the one with its sign rule.
fn time_values(defs: &mut Map<String, Value>) {
    for (name, sign) in [
        ("Millis", TimeSign::Any),
        ("NonNegativeMillis", TimeSign::NonNegative),
        ("PositiveMillis", TimeSign::Positive),
    ] {
        defs.insert(name.into(), millis_schema(sign));
    }
    for &obj in fields::ALL_OBJS {
        let Some(def_name) = fields::schema_def(obj) else {
            continue;
        };
        let Some(Value::Object(props)) =
            defs.get_mut(def_name).and_then(|d| d.get_mut("properties"))
        else {
            continue;
        };
        for (key, kind) in fields::fields(obj) {
            let target = match (kind, time_sign(obj, key)) {
                (Kind::Millis, TimeSign::NonNegative) => "#/$defs/NonNegativeMillis",
                (Kind::Millis, TimeSign::Positive) => "#/$defs/PositiveMillis",
                _ => continue,
            };
            if let Some(prop) = props.get_mut(key) {
                retarget_ref(prop, "#/$defs/Millis", target);
            }
        }
    }
}

// ---- helpers ------------------------------------------------------------------------------------------

/// `additionalProperties: false` plus the annotation escape hatch.
fn close_object(obj: &mut Map<String, Value>) {
    if !obj.contains_key("additionalProperties") {
        obj.insert("additionalProperties".into(), Value::Bool(false));
    }
    obj.insert(
        "patternProperties".into(),
        json!({ ANNOTATION_PATTERN: {} }),
    );
}

/// Make `keys` required in `def_name`, and not nullable.
fn require(defs: &mut Map<String, Value>, def_name: &str, keys: &[&str]) {
    let Some(Value::Object(def)) = defs.get_mut(def_name) else {
        return;
    };
    def.insert("required".into(), json!(keys));
    if let Some(Value::Object(props)) = def.get_mut("properties") {
        for key in keys {
            if let Some(prop) = props.get_mut(*key) {
                strip_null(prop);
            }
        }
    }
}

/// Remove `null` from the accepted types of a property (`type: [string, null]`, or schemars'
/// `anyOf: [{$ref}, {type: null}]` for optional structs).
fn strip_null(prop: &mut Value) {
    let Value::Object(map) = prop else { return };
    if let Some(Value::Array(types)) = map.get_mut("type") {
        types.retain(|t| t != "null");
        if let [single] = types.as_slice() {
            let single = single.clone();
            map.insert("type".into(), single);
        }
    }
    let null = json!({ "type": "null" });
    let single = match map.get_mut("anyOf") {
        Some(Value::Array(branches)) => {
            branches.retain(|b| *b != null);
            (branches.len() == 1).then(|| branches.remove(0))
        }
        _ => None,
    };
    if let Some(Value::Object(only)) = single {
        map.shift_remove("anyOf");
        for (k, v) in only {
            map.entry(k).or_insert(v);
        }
    }
}

/// Replace every `$ref: from` inside `value` with `$ref: to`.
fn retarget_ref(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if key == "$ref" && child.as_str() == Some(from) {
                    *child = json!(to);
                } else {
                    retarget_ref(child, from, to);
                }
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| retarget_ref(item, from, to)),
        _ => {}
    }
}

/// Remove schemars' Rust-specific `format` annotations (`uint32`, `double`, …) and their implied
/// integer bounds; [`apply_constraints`] adds the DSL's real ranges.
fn strip_numeric_formats(value: &mut Map<String, Value>) {
    let is_rust_format =
        |f: &str| matches!(f, "double" | "float") || f.starts_with("uint") || f.starts_with("int");
    if value
        .get("format")
        .and_then(Value::as_str)
        .is_some_and(is_rust_format)
    {
        value.shift_remove("format");
        if value.get("minimum") == Some(&json!(0)) {
            value.shift_remove("minimum");
        }
        if value.get("maximum") == Some(&json!(65535)) {
            value.shift_remove("maximum");
        }
    }
    for child in value.values_mut() {
        match child {
            Value::Object(map) => strip_numeric_formats(map),
            Value::Array(items) => {
                for item in items {
                    if let Value::Object(map) = item {
                        strip_numeric_formats(map);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Value ranges, patterns and list lengths that mirror the lowering checks.
fn apply_constraints(def_name: &str, def: &mut Map<String, Value>) {
    let non_blank = json!({ "pattern": NON_BLANK });
    let voice_fields = [
        ("speaker", non_blank.clone()),
        ("provider", non_blank.clone()),
        ("voice_id", non_blank.clone()),
        ("model", non_blank.clone()),
        ("language", non_blank.clone()),
    ];
    let constraints: Vec<(&str, Value)> = match def_name {
        "OutputSpec" => vec![
            ("path", non_blank.clone()),
            ("sample_rate", json!({ "minimum": 8000, "maximum": 192000 })),
            ("channels", json!({ "enum": [1, 2, null] })),
            ("bit_depth", json!({ "enum": [16, 24, 32, null] })),
            ("target_lufs", json!({ "minimum": -60, "maximum": -5 })),
            ("true_peak_db", json!({ "minimum": -9, "maximum": 0 })),
        ],
        "SubtitleSpec" => vec![
            ("max_chars_per_line", json!({ "minimum": 1 })),
            ("max_lines", json!({ "minimum": 1 })),
        ],
        "MixSpec" => vec![("reference_lufs", json!({ "minimum": -70, "maximum": -5 }))],
        "VoiceSpec" => {
            let mut c = voice_fields[1..].to_vec();
            c.push(("gain_db", json!({ "minimum": -60, "maximum": 24 })));
            c
        }
        "ProviderSpec" => vec![
            ("max_concurrency", json!({ "minimum": 1 })),
            ("timeout_ms", json!({ "minimum": 1 })),
            ("chars_per_second", json!({ "exclusiveMinimum": 0 })),
            ("words_per_minute", json!({ "exclusiveMinimum": 0 })),
        ],
        "Track" => {
            let mut c = vec![
                ("id", json!({ "pattern": ID_PATTERN })),
                ("source", non_blank.clone()),
                ("volume", json!({ "minimum": 0, "maximum": 4 })),
                ("gain_db", json!({ "minimum": -60, "maximum": 24 })),
                ("pan", json!({ "minimum": -1, "maximum": 1 })),
                ("sequence", json!({ "minItems": 1 })),
                ("clips", json!({ "minItems": 1 })),
            ];
            c.extend(voice_fields.iter().cloned());
            c
        }
        "SequenceItem" => {
            let mut c = vec![
                ("id", json!({ "pattern": ID_PATTERN })),
                ("text", non_blank.clone()),
                ("caption", non_blank.clone()),
                ("volume", json!({ "minimum": 0, "maximum": 4 })),
                ("gain_db", json!({ "minimum": -60, "maximum": 24 })),
            ];
            c.extend(voice_fields.iter().cloned());
            c
        }
        "Clip" => vec![
            ("id", json!({ "pattern": ID_PATTERN })),
            ("source", non_blank.clone()),
            ("volume", json!({ "minimum": 0, "maximum": 4 })),
            ("gain_db", json!({ "minimum": -60, "maximum": 24 })),
        ],
        "Anchor" => vec![
            ("after_cue", non_blank.clone()),
            ("with_cue", non_blank.clone()),
            ("before_cue", non_blank.clone()),
        ],
        "DuckingSpec" => vec![
            (
                "trigger_tracks",
                json!({ "minItems": 1, "items": { "type": "string", "pattern": NON_BLANK } }),
            ),
            ("reduction_db", json!({ "minimum": -60, "maximum": 60 })),
            ("threshold_db", json!({ "minimum": -100, "maximum": 0 })),
        ],
        "NormalizeSpec" => vec![("target_lufs", json!({ "minimum": -70, "maximum": -5 }))],
        "TrimSilenceSpec" => vec![("threshold_db", json!({ "minimum": -100, "maximum": 0 }))],
        _ => Vec::new(),
    };
    let Some(Value::Object(props)) = def.get_mut("properties") else {
        return;
    };
    for (prop_name, extra) in constraints {
        if let (Some(Value::Object(prop)), Value::Object(extra)) = (props.get_mut(prop_name), extra)
        {
            for (k, v) in extra {
                prop.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fields::ALL_OBJS;

    fn def<'a>(schema: &'a Value, name: Option<&str>) -> &'a Value {
        match name {
            Some(name) => &schema["$defs"][name],
            None => schema,
        }
    }

    #[test]
    fn field_tables_match_schema_properties() {
        let schema = timeline_schema();
        for &obj in ALL_OBJS {
            let def = def(&schema, fields::schema_def(obj));
            let props: Vec<&str> = def["properties"]
                .as_object()
                .expect("object def has properties")
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                props,
                fields::names(obj),
                "field table for {obj:?} differs from the schema"
            );
        }
        // `EndAnchor` keeps `before_cue` only as an always-absent (null) key.
        let end_anchor = &schema["$defs"]["EndAnchor"]["properties"];
        let keys: Vec<&str> = end_anchor
            .as_object()
            .expect("EndAnchor")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["after_cue", "with_cue", "before_cue", "offset_ms"]);
        assert_eq!(end_anchor["before_cue"]["type"], "null");
    }

    #[test]
    fn root_metadata() {
        let schema = timeline_schema();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(schema["$id"], SCHEMA_ID);
        assert_eq!(schema["title"], SCHEMA_TITLE);
        assert_eq!(schema["required"], json!(["version", "tracks"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        let keys: Vec<&String> = schema.as_object().expect("object").keys().collect();
        assert_eq!(keys[..4], ["$schema", "$id", "title", "description"]);
    }

    #[test]
    fn every_object_is_closed_except_settings() {
        fn walk(value: &Value, path: &str, open: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    let is_object_schema = map.get("type").is_some_and(|t| {
                        t == "object" || t.as_array().is_some_and(|a| a.contains(&json!("object")))
                    });
                    if is_object_schema && map.get("additionalProperties") == Some(&json!(true)) {
                        open.push(path.to_string());
                    }
                    if is_object_schema && map.contains_key("properties") {
                        assert_eq!(
                            map.get("additionalProperties"),
                            Some(&json!(false)),
                            "{path} is not closed"
                        );
                        assert!(
                            map.contains_key("patternProperties"),
                            "{path} lacks annotation patternProperties"
                        );
                    }
                    for (k, v) in map {
                        walk(v, &format!("{path}/{k}"), open);
                    }
                }
                Value::Array(items) => {
                    for (i, v) in items.iter().enumerate() {
                        walk(v, &format!("{path}/{i}"), open);
                    }
                }
                _ => {}
            }
        }
        let schema = timeline_schema();
        let mut open = Vec::new();
        walk(&schema, "", &mut open);
        assert!(!open.is_empty());
        assert!(
            open.iter().all(|p| p.ends_with("/settings")),
            "unexpected open objects: {open:?}"
        );
    }

    #[test]
    fn no_rust_formats_left() {
        let text = timeline_schema_pretty();
        for f in ["uint16", "uint32", "uint64", "\"double\""] {
            assert!(!text.contains(f), "schema still contains {f}");
        }
    }

    #[test]
    fn enums_list_aliases() {
        let schema = timeline_schema();
        let kinds = &schema["$defs"]["ProviderTypeName"]["enum"];
        assert_eq!(kinds[0], "elevenlabs");
        assert!(
            kinds
                .as_array()
                .expect("array")
                .contains(&json!("local_kokoro"))
        );
    }

    #[test]
    fn time_fields_use_their_sign_rule() {
        let schema = timeline_schema();
        let reference = |def: &str, prop: &str| {
            schema["$defs"][def]["properties"][prop]["anyOf"][0]["$ref"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        assert_eq!(reference("Anchor", "offset_ms"), "#/$defs/Millis");
        assert_eq!(reference("EndAnchor", "offset_ms"), "#/$defs/Millis");
        assert_eq!(reference("Clip", "duration_ms"), "#/$defs/PositiveMillis");
        assert_eq!(reference("Track", "duration_ms"), "#/$defs/PositiveMillis");
        assert_eq!(
            reference("OutputSpec", "duration_ms"),
            "#/$defs/PositiveMillis"
        );
        assert_eq!(
            reference("SubtitleSpec", "max_duration_ms"),
            "#/$defs/PositiveMillis"
        );
        for (def, prop) in [
            ("Track", "start_at_ms"),
            ("SequenceItem", "pause"),
            ("SequenceItem", "pause_duration"),
            ("Clip", "fade_in_ms"),
            ("Clip", "crossfade_ms"),
            ("OutputSpec", "tail_ms"),
            ("SubtitleSpec", "min_duration_ms"),
            ("TimingSpec", "min_gap_ms"),
            ("DuckingSpec", "attack_ms"),
            ("TrimSilenceSpec", "padding_ms"),
        ] {
            assert_eq!(
                reference(def, prop),
                "#/$defs/NonNegativeMillis",
                "{def}.{prop}"
            );
        }
    }

    #[test]
    fn required_keys_are_not_nullable() {
        let schema = timeline_schema();
        assert_eq!(schema["properties"]["version"]["$ref"], "#/$defs/Version");
        assert!(schema["properties"]["version"].get("anyOf").is_none());
        assert_eq!(
            schema["$defs"]["Track"]["properties"]["id"]["type"],
            "string"
        );
        assert_eq!(
            schema["$defs"]["Track"]["properties"]["type"]["$ref"],
            "#/$defs/TrackKindName"
        );
        assert_eq!(
            schema["$defs"]["Clip"]["properties"]["source"]["type"],
            "string"
        );
        assert_eq!(
            schema["$defs"]["DuckingSpec"]["properties"]["trigger_tracks"]["type"],
            "array"
        );
        // Optional keys stay nullable.
        assert_eq!(
            schema["$defs"]["Track"]["properties"]["volume"]["type"],
            json!(["number", "null"])
        );
    }

    #[test]
    fn case_insensitive_patterns() {
        assert_eq!(case_insensitive("mp3"), "[mM][pP]3");
        assert_eq!(
            spellings_of(ProviderTypeName::Kokoro),
            ["kokoro", "local_kokoro", "kokoro_fastapi"]
        );
    }

    #[test]
    fn every_provider_type_is_in_one_group() {
        let mut grouped: Vec<ProviderTypeName> = provider_groups()
            .into_iter()
            .flat_map(|(_, _, types)| types)
            .collect();
        assert_eq!(grouped.len(), provider_types().len());
        grouped.dedup();
        for t in provider_types() {
            assert!(grouped.contains(&t), "{t:?} has no provider group");
        }
        // Every built-in name gets exactly one pattern (plus the annotation pattern).
        let patterns = provider_name_patterns();
        assert_eq!(patterns.as_object().map(Map::len), Some(5));
        let schema = timeline_schema();
        for name in ProviderTypeName::SPELLINGS {
            let matching = schema["properties"]["providers"]["patternProperties"]
                .as_object()
                .expect("patterns")
                .keys()
                .filter(|p| {
                    p.trim_start_matches("^(")
                        .trim_end_matches(")$")
                        .split('|')
                        .any(|n| n == *name)
                })
                .count();
            assert_eq!(matching, 1, "{name}");
        }
    }
}
