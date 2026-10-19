//! Element-by-element deserialization of the generic document value into the raw AST.
//!
//! Every top-level field, every `speakers` / `providers` entry, every track, every sequence item and
//! every clip is deserialized on its own, so one bad element never hides problems elsewhere. Inside one
//! element, a field whose value has the wrong type is reported (`invalid_type`, exact path, accepted
//! forms, and the converted value when the intent is clear: `sample_rate: 44.1k` → `44100`, `volume: 50%`
//! → `0.5`), removed, and the element is deserialized again; elements that are not mappings at all are
//! replaced by an empty placeholder so list indices (and therefore diagnostic paths) stay stable. Every
//! removed value is recorded in `dropped` so later stages can ignore their consequences.
//!
//! Unknown keys are collected with `serde_ignored` (plus a manual check of the flag-or-object values that
//! `serde_ignored` cannot see through) and reported as `unknown_field`. The hint tries, in order: a semantic
//! synonym at that location (`file:` on a music track → `source`, `start_after: a2` → `start_relative:
//! {after_cue: a2}`, see [`fields::synonym`]), the key verbatim at another location, did-you-mean
//! candidates among the keys valid there ([`crate::fields`]), a close match elsewhere, and the list of
//! valid keys. A hint that renames a key to a millisecond field whose value is a bare number says how to
//! write seconds. Keys starting with `x-` / `x_` are ignored, and never copied.
//!
//! Values that have the right type but one obvious mechanical fix are reported as `invalid_value` with the
//! fix, and replaced by it, so later stages see what the hint suggests (no follow-up errors): enum names
//! with the wrong case, separators or surrounding spaces (`Speech`, `MP3`, `equal-power`), duration strings
//! with uppercase units or clock seconds of 60 or more (`350MS`, `1:99.5`), positive loudness targets and
//! ceilings (`target_lufs: 16`), and platform names used as loudness presets (`youtube` → `streaming`). A
//! start or end time written as an id (`start_at_ms: a2`) is reported with the anchor that does what was
//! meant (`start_relative: {after_cue: a2}`) and dropped.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde_json::{Map, Number, Value};

use super::secrets;
use crate::diagnostics::{self, Diagnostic, codes};
use crate::fields::{self, Kind, Obj, Synonym};
use crate::model::{
    Clip, DuckingModeName, FadeCurveName, LoudnessPresetName, OutputFormatName, OverlapPolicyName,
    ProviderSpec, ProviderTypeName, SequenceItem, SubtitleFormatName, Timeline, Track,
    TrackKindName, VoiceSpec,
};
use crate::values;

/// Output of [`decode`].
pub(super) struct Decoded {
    pub timeline: Timeline,
    pub diagnostics: Vec<Diagnostic>,
    pub dropped: Vec<String>,
}

/// Decode a document whose root is a mapping. `suppress` lists paths whose `unknown_field` diagnostics are
/// redundant (secret keys, already reported as `secret_in_timeline`).
pub(super) fn decode(root: &Map<String, Value>, suppress: &HashSet<String>) -> Decoded {
    let mut d = Decoder {
        diagnostics: Vec::new(),
        dropped: Vec::new(),
        reported: HashSet::new(),
        suppress,
        ids: declared_ids(root),
    };
    // The collections are decoded element by element below; annotations are never copied.
    let (mut speakers, mut providers, mut tracks) = (None, None, None);
    let mut rest = Map::new();
    for (key, value) in root {
        match key.as_str() {
            "speakers" => speakers = Some(value),
            "providers" => providers = Some(value),
            "tracks" => tracks = Some(value),
            _ if fields::is_annotation(key) => {}
            _ => {
                rest.insert(
                    key.clone(),
                    clone_dsl(value, fields::kind_of(Obj::Timeline, key)),
                );
            }
        }
    }
    let mut timeline: Timeline = d
        .decode_owned(
            Value::Object(rest),
            "",
            Obj::Timeline,
            &Ctx::plain("the top level"),
        )
        .unwrap_or_default();
    timeline.speakers = d.name_map::<VoiceSpec>(speakers, "speakers", Obj::Voice);
    timeline.providers = d.name_map::<ProviderSpec>(providers, "providers", Obj::Provider);
    timeline.tracks = d.tracks(tracks);
    Decoded {
        timeline,
        diagnostics: d.diagnostics,
        dropped: d.dropped,
    }
}

/// Maximum number of remove-and-retry rounds for one element (bounded by its number of leaves anyway).
const MAX_ROUNDS: usize = 256;

/// One step of a document path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

fn render(base: &str, segs: &[Seg]) -> String {
    let mut out = base.to_string();
    for seg in segs {
        match seg {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Index(i) => {
                out.push_str(&format!("[{i}]"));
            }
        }
    }
    out
}

fn join(base: &str, key: &str) -> String {
    diagnostics::path::field(base, key)
}

/// A copy of `value` (a value of `kind`) without the annotation entries (`x-…`, `x_…`) of DSL objects:
/// they are ignored anyway and may be large. Free-form maps (`settings`) and values of unknown keys are
/// copied as they are.
fn clone_dsl(value: &Value, kind: Option<Kind>) -> Value {
    let obj = match kind {
        Some(Kind::Obj(o)) => o,
        Some(Kind::Normalize) => Obj::Normalize,
        Some(Kind::TrimSilence) => Obj::TrimSilence,
        Some(Kind::ObjList(o)) => {
            return match value {
                Value::Array(items) => Value::Array(
                    items
                        .iter()
                        .map(|item| clone_dsl(item, Some(Kind::Obj(o))))
                        .collect(),
                ),
                other => other.clone(),
            };
        }
        Some(Kind::NameMap(o)) => {
            return match value {
                Value::Object(map) => Value::Object(
                    map.iter()
                        .filter(|(name, _)| !fields::is_annotation(name))
                        .map(|(name, entry)| (name.clone(), clone_dsl(entry, Some(Kind::Obj(o)))))
                        .collect(),
                ),
                other => other.clone(),
            };
        }
        _ => return value.clone(),
    };
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| !fields::is_annotation(key))
                .map(|(key, v)| (key.clone(), clone_dsl(v, fields::kind_of(obj, key))))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Where unknown keys / type errors are, for messages ("in music track `bed`").
struct Ctx {
    /// "music track `bed`", "cue `cue_01`", "`output`", …
    label: String,
    /// The track type, when the element is a track (selects did-you-mean candidates and synonyms).
    track_kind: Option<TrackKindName>,
    /// True for the entries of `speakers` (where `label` is valid), false for `defaults`.
    speaker_entry: bool,
}

impl Ctx {
    fn plain(label: impl Into<String>) -> Self {
        Ctx {
            label: label.into(),
            track_kind: None,
            speaker_entry: false,
        }
    }
}

struct Decoder<'s> {
    diagnostics: Vec<Diagnostic>,
    dropped: Vec<String>,
    /// Paths that already have an `invalid_type` diagnostic.
    reported: HashSet<String>,
    suppress: &'s HashSet<String>,
    /// The ids of the document and what they name (for hints).
    ids: HashMap<String, &'static str>,
}

/// The track, cue and clip ids of a document, explicit and automatic (`<track>.cue<n>`, `<track>.clip<n>`,
/// counted the way lowering counts them), with what each one names. Only used for hints.
fn declared_ids(root: &Map<String, Value>) -> HashMap<String, &'static str> {
    let mut ids = HashMap::new();
    let Some(Value::Array(tracks)) = root.get("tracks") else {
        return ids;
    };
    let id_of = |v: &Value| {
        v.get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let set = |v: &Value, key: &str| v.get(key).is_some_and(|x| !x.is_null());
    for track in tracks {
        let track_id = id_of(track);
        if let Some(id) = &track_id {
            ids.insert(id.clone(), "track");
        }
        if let Some(Value::Array(items)) = track.get("sequence") {
            let mut cues = 0;
            for item in items {
                let pause =
                    (set(item, "pause") || set(item, "pause_duration")) && !set(item, "text");
                if pause {
                    continue;
                }
                cues += 1;
                match (id_of(item), &track_id) {
                    (Some(id), _) => ids.insert(id, "cue"),
                    (None, Some(t)) => ids.insert(format!("{t}.cue{cues}"), "cue"),
                    (None, None) => None,
                };
            }
        }
        if let Some(Value::Array(clips)) = track.get("clips") {
            for (j, clip) in clips.iter().enumerate() {
                match (id_of(clip), &track_id) {
                    (Some(id), _) => ids.insert(id, "clip"),
                    (None, Some(t)) => ids.insert(format!("{t}.clip{}", j + 1), "clip"),
                    (None, None) => None,
                };
            }
        }
    }
    ids
}

impl Decoder<'_> {
    // ---- generic struct decoding with recovery ------------------------------------------------------

    /// Deserialize `value` (an object of kind `obj` at `base`) into `T`; see [`Self::decode_owned`].
    fn decode_struct<T: DeserializeOwned>(
        &mut self,
        value: &Value,
        base: &str,
        obj: Obj,
        ctx: &Ctx,
    ) -> Option<T> {
        self.decode_owned(clone_dsl(value, Some(Kind::Obj(obj))), base, obj, ctx)
    }

    /// Deserialize `work` (an object of kind `obj` at `base`, without annotations) into `T`, after the
    /// mechanical value fixes, reporting and removing badly typed fields until it succeeds. Returns None only
    /// if the value itself has the wrong shape.
    fn decode_owned<T: DeserializeOwned>(
        &mut self,
        mut work: Value,
        base: &str,
        obj: Obj,
        ctx: &Ctx,
    ) -> Option<T> {
        coerce_integers(&mut work, obj);
        self.fix_values(&mut work, base, obj);
        let mut previous: Option<Vec<Seg>> = None;
        for _ in 0..MAX_ROUNDS {
            let (result, unknown) = deserialize::<T>(&work);
            match result {
                Ok(t) => {
                    for rel in unknown {
                        self.unknown_key(&work, base, &rel, obj, ctx);
                    }
                    self.check_flag_object_keys(&work, base, obj, ctx);
                    return Some(t);
                }
                Err(segs) => {
                    if segs.is_empty() {
                        self.type_error(base, &work, Some(Kind::Obj(obj)));
                        return None;
                    }
                    // Fields are removed only after they are reported, so `work` still holds the bad value.
                    let repeated = previous.as_ref() == Some(&segs);
                    if !repeated {
                        self.report_field_error(&work, base, &segs, obj, ctx);
                    }
                    let removed = neutralize(&mut work, &segs, repeated)?;
                    self.dropped.push(render(base, &removed));
                    previous = Some(segs);
                }
            }
        }
        None
    }

    /// Report a type error at `base + segs` (an element that failed to deserialize).
    fn report_field_error(&mut self, work: &Value, base: &str, segs: &[Seg], obj: Obj, ctx: &Ctx) {
        let path = render(base, segs);
        let actual = lookup(work, segs);
        let kind = kind_at(obj, segs);
        match (kind, actual) {
            (Some(k @ (Kind::Normalize | Kind::TrimSilence)), Some(Value::Object(map))) => {
                let inner_obj = if k == Kind::Normalize {
                    Obj::Normalize
                } else {
                    Obj::TrimSilence
                };
                if !self.check_inner_fields(map, &path, inner_obj, ctx) {
                    self.type_error_at(&path, actual, kind);
                }
            }
            (Some(Kind::SubtitleExport), Some(Value::Array(items))) => {
                let mut any = false;
                for (i, item) in items.iter().enumerate() {
                    if !item.is_string() {
                        self.type_error_at(&format!("{path}[{i}]"), Some(item), Some(Kind::Str));
                        any = true;
                    }
                }
                if !any {
                    self.type_error_at(&path, actual, kind);
                }
            }
            _ => self.type_error_at(&path, actual, kind),
        }
    }

    /// Check the fields of a flag-or-object mapping by hand (`serde_ignored` cannot see inside untagged
    /// values). Returns true if at least one problem was reported.
    fn check_inner_fields(
        &mut self,
        map: &Map<String, Value>,
        path: &str,
        obj: Obj,
        ctx: &Ctx,
    ) -> bool {
        let mut any = false;
        for (key, value) in map {
            if fields::is_annotation(key) {
                continue;
            }
            match fields::kind_of(obj, key) {
                Some(kind) => {
                    if !value_fits(kind, value) {
                        self.type_error_at(&join(path, key), Some(value), Some(kind));
                        any = true;
                    }
                }
                None => {
                    let inner = Ctx::plain(format!("`{}` of {}", display_field(path), ctx.label));
                    self.report_unknown(path, key, Some(value), Some(map), Some(obj), &inner);
                    any = true;
                }
            }
        }
        any
    }

    /// After a successful decode: unknown keys inside surviving `normalize: {...}` / `trim_silence: {...}`.
    fn check_flag_object_keys(&mut self, work: &Value, base: &str, obj: Obj, ctx: &Ctx) {
        let Value::Object(map) = work else { return };
        for (key, kind) in fields::fields(obj) {
            let inner_obj = match kind {
                Kind::Normalize => Obj::Normalize,
                Kind::TrimSilence => Obj::TrimSilence,
                _ => continue,
            };
            if let Some(Value::Object(inner)) = map.get(key) {
                let path = join(base, key);
                let inner_ctx = Ctx::plain(format!("`{key}` of {}", ctx.label));
                for (k, v) in inner {
                    if !fields::is_annotation(k) && fields::kind_of(inner_obj, k).is_none() {
                        self.report_unknown(
                            &path,
                            k,
                            Some(v),
                            Some(inner),
                            Some(inner_obj),
                            &inner_ctx,
                        );
                    }
                }
            }
        }
    }

    /// `invalid_type` for a whole element (wrong shape) at `path`.
    fn type_error(&mut self, path: &str, actual: &Value, kind: Option<Kind>) {
        self.type_error_at(path, Some(actual), kind);
        self.dropped.push(path.to_string());
    }

    fn type_error_at(&mut self, path: &str, actual: Option<&Value>, kind: Option<Kind>) {
        if !self.reported.insert(path.to_string()) {
            return;
        }
        let field = display_field(path);
        let expected = kind
            .map(fields::expectation)
            .unwrap_or("a different kind of value");
        let message = format!("`{field}` must be {expected}, got {}", describe(actual));
        let hint = type_hint(kind, actual, &field);
        self.diagnostics.push(
            Diagnostic::error(codes::INVALID_TYPE, message)
                .with_path(path)
                .with_hint(hint),
        );
    }

    // ---- values with one mechanical fix -------------------------------------------------------------

    /// Report and replace the values of `value` (an object of kind `obj` at `base`) and of the objects
    /// nested in it that have the right type but one obvious fix (see the module docs). A time field set to
    /// an id (`start_at_ms: a2`) is reported with the anchor to use instead and removed.
    fn fix_values(&mut self, value: &mut Value, base: &str, obj: Obj) {
        let Value::Object(map) = value else { return };
        let mut remove = Vec::new();
        for (key, kind) in fields::fields(obj) {
            let Some(v) = map.get_mut(key) else { continue };
            let path = join(base, key);
            match kind {
                Kind::Obj(inner) => self.fix_values(v, &path, inner),
                Kind::Normalize => self.fix_values(v, &path, Obj::Normalize),
                Kind::TrimSilence => self.fix_values(v, &path, Obj::TrimSilence),
                Kind::SubtitleExport => match v {
                    Value::Array(items) => {
                        for (i, item) in items.iter_mut().enumerate() {
                            self.fix_scalar(obj, key, kind, item, &format!("{path}[{i}]"), true);
                        }
                    }
                    other => self.fix_scalar(obj, key, kind, other, &path, false),
                },
                Kind::Millis if self.time_is_an_id(key, v, &path) => remove.push((key, path)),
                _ => self.fix_scalar(obj, key, kind, v, &path, false),
            }
        }
        for (key, path) in remove {
            map.shift_remove(key);
            self.dropped.push(path);
        }
    }

    fn fix_scalar(
        &mut self,
        obj: Obj,
        key: &str,
        kind: Kind,
        value: &mut Value,
        path: &str,
        in_list: bool,
    ) {
        let fix = match (kind, &*value) {
            (Kind::Millis, Value::String(s)) => duration_fix(key, s),
            (Kind::Str | Kind::SubtitleExport, Value::String(s)) => enum_fix(obj, key, s, in_list),
            (Kind::Num, Value::Number(n)) => sign_fix(obj, key, n),
            _ => None,
        };
        if let Some(fix) = fix {
            self.diagnostics.push(
                Diagnostic::error(codes::INVALID_VALUE, fix.message)
                    .with_path(path)
                    .with_hint(fix.hint),
            );
            *value = fix.value;
        }
    }

    /// `start_at_ms: a2` / `end_at_ms: a2` where `a2` is an id: report the anchor that does what was meant.
    fn time_is_an_id(&mut self, key: &str, value: &Value, path: &str) -> bool {
        let Some(s) = value.as_str() else {
            return false;
        };
        let id = s.trim();
        let (Some(what), true) = (
            self.ids.get(id).copied(),
            values::parse_duration(s).is_err(),
        ) else {
            return false;
        };
        let shown = render_str(id).unwrap_or_else(|| id.to_string());
        let hint = match key {
            "start_at_ms" => format!(
                "To start after `{id}` ends, write `start_relative: {{after_cue: {shown}}}` instead of `start_at_ms` \
                 (or `with_cue: {shown}` to start together with it)"
            ),
            "end_at_ms" => format!(
                "To end when `{id}` ends, write `end_relative: {{after_cue: {shown}}}` instead of `end_at_ms` (or \
                 `with_cue: {shown}` to end when it starts)"
            ),
            _ => return false,
        };
        self.diagnostics.push(
            Diagnostic::error(
                codes::INVALID_VALUE,
                format!("`{key}` is a time, but `{id}` is the id of a {what}"),
            )
            .with_path(path)
            .with_hint(hint),
        );
        true
    }

    // ---- unknown keys --------------------------------------------------------------------------------

    fn unknown_key(&mut self, work: &Value, base: &str, rel: &[Seg], root: Obj, ctx: &Ctx) {
        let Some((Seg::Key(key), parent)) = rel.split_last() else {
            return;
        };
        let parent_obj = obj_at(root, parent);
        let parent_path = render(base, parent);
        let siblings = lookup(work, parent).and_then(Value::as_object);
        let value = siblings.and_then(|m| m.get(key));
        let ctx = if parent.is_empty() {
            Ctx {
                label: ctx.label.clone(),
                track_kind: ctx.track_kind,
                speaker_entry: ctx.speaker_entry,
            }
        } else if base.is_empty() && parent.len() == 1 {
            Ctx::plain(format!("`{}`", display_field(&parent_path)))
        } else {
            Ctx::plain(format!(
                "`{}` of {}",
                display_field(&parent_path),
                ctx.label
            ))
        };
        self.report_unknown(&parent_path, key, value, siblings, parent_obj, &ctx);
    }

    fn report_unknown(
        &mut self,
        parent_path: &str,
        key: &str,
        value: Option<&Value>,
        siblings: Option<&Map<String, Value>>,
        parent_obj: Option<Obj>,
        ctx: &Ctx,
    ) {
        if fields::is_annotation(key) {
            return;
        }
        let path = join(parent_path, key);
        if self.suppress.contains(&path) {
            return;
        }
        let message = format!("unknown field `{key}` in {}", ctx.label);
        let hint = unknown_hint(&UnknownKey {
            key,
            normalized: diagnostics::normalize_key(key.trim()),
            value,
            siblings,
            parent: parent_obj,
            track_kind: ctx.track_kind,
            speaker_entry: ctx.speaker_entry,
        });
        self.diagnostics.push(
            Diagnostic::error(codes::UNKNOWN_FIELD, message)
                .with_path(path)
                .with_hint(hint),
        );
    }

    // ---- collections -------------------------------------------------------------------------------

    fn name_map<T: DeserializeOwned + Default>(
        &mut self,
        value: Option<&Value>,
        key: &str,
        obj: Obj,
    ) -> IndexMap<String, T> {
        let mut out = IndexMap::new();
        match value {
            None | Some(Value::Null) => {}
            Some(Value::Object(map)) => {
                for (name, entry) in map {
                    if fields::is_annotation(name) {
                        continue;
                    }
                    let path = join(key, name);
                    let ctx = match obj {
                        Obj::Voice => Ctx {
                            speaker_entry: true,
                            ..Ctx::plain(format!("speaker `{name}`"))
                        },
                        _ => Ctx::plain(format!("provider `{name}`")),
                    };
                    match entry {
                        Value::Null => {
                            out.insert(name.clone(), T::default());
                        }
                        Value::Object(_) => {
                            if let Some(t) = self.decode_struct::<T>(entry, &path, obj, &ctx) {
                                out.insert(name.clone(), t);
                            }
                        }
                        other => self.type_error(&path, other, Some(Kind::Obj(obj))),
                    }
                }
            }
            Some(other) => self.type_error(key, other, Some(Kind::NameMap(obj))),
        }
        out
    }

    fn tracks(&mut self, value: Option<&Value>) -> Vec<Track> {
        match value {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| self.track(item, i))
                .collect(),
            Some(other) => {
                self.type_error("tracks", other, Some(Kind::ObjList(Obj::Track)));
                Vec::new()
            }
        }
    }

    fn track(&mut self, value: &Value, index: usize) -> Track {
        let path = format!("tracks[{index}]");
        let Value::Object(map) = value else {
            self.type_error(&path, value, Some(Kind::Obj(Obj::Track)));
            return Track::default();
        };
        // For hints, a misspelled type (`Speech`) still selects the keys of that track type.
        let kind = map.get("type").and_then(Value::as_str).and_then(|s| {
            TrackKindName::parse(s)
                .or_else(|| TrackKindName::spelling_of(s).and_then(TrackKindName::parse))
        });
        let label = match (map.get("id").and_then(Value::as_str), kind) {
            (Some(id), Some(k)) => format!("{k} track `{id}`"),
            (Some(id), None) => format!("track `{id}`"),
            (None, _) => format!("track `{path}`"),
        };
        let ctx = Ctx {
            track_kind: kind,
            ..Ctx::plain(label)
        };
        // `sequence` and `clips` are decoded element by element below; nothing is copied twice.
        let rest: Map<String, Value> = map
            .iter()
            .filter(|(key, _)| {
                !matches!(key.as_str(), "sequence" | "clips") && !fields::is_annotation(key)
            })
            .map(|(key, v)| (key.clone(), clone_dsl(v, fields::kind_of(Obj::Track, key))))
            .collect();
        let mut track: Track = self
            .decode_owned(Value::Object(rest), &path, Obj::Track, &ctx)
            .unwrap_or_default();
        track.sequence = self.list(
            map.get("sequence"),
            &path,
            "sequence",
            Obj::SequenceItem,
            |d, item, item_path| d.sequence_item(item, item_path),
        );
        track.clips = self.list(
            map.get("clips"),
            &path,
            "clips",
            Obj::Clip,
            |d, item, item_path| d.clip(item, item_path),
        );
        track
    }

    /// Decode a list field element by element. `None` when the key is absent, null or not a list.
    fn list<T>(
        &mut self,
        value: Option<&Value>,
        base: &str,
        key: &str,
        obj: Obj,
        mut element: impl FnMut(&mut Self, &Value, &str) -> T,
    ) -> Option<Vec<T>> {
        let path = join(base, key);
        match value? {
            Value::Null => None,
            Value::Array(items) => Some(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| element(self, item, &format!("{path}[{i}]")))
                    .collect(),
            ),
            other => {
                self.type_error(&path, other, Some(Kind::ObjList(obj)));
                None
            }
        }
    }

    fn sequence_item(&mut self, value: &Value, path: &str) -> SequenceItem {
        let Value::Object(map) = value else {
            self.type_error(path, value, Some(Kind::Obj(Obj::SequenceItem)));
            return SequenceItem::default();
        };
        let ctx = match map.get("id").and_then(Value::as_str) {
            Some(id) => Ctx::plain(format!("cue `{id}`")),
            None if fields::PAUSE_KEYS.iter().any(|k| map.contains_key(*k)) => {
                Ctx::plain(format!("pause `{path}`"))
            }
            None => Ctx::plain(format!("cue `{path}`")),
        };
        self.decode_struct(value, path, Obj::SequenceItem, &ctx)
            .unwrap_or_default()
    }

    fn clip(&mut self, value: &Value, path: &str) -> Clip {
        let Value::Object(map) = value else {
            self.type_error(path, value, Some(Kind::Obj(Obj::Clip)));
            return Clip::default();
        };
        let ctx = match map.get("id").and_then(Value::as_str) {
            Some(id) => Ctx::plain(format!("clip `{id}`")),
            None => Ctx::plain(format!("clip `{path}`")),
        };
        self.decode_struct(value, path, Obj::Clip, &ctx)
            .unwrap_or_default()
    }
}

// ---- serde plumbing -----------------------------------------------------------------------------------

/// Deserialize with path tracking and unknown-key collection. Errors carry the path of the failing value.
fn deserialize<T: DeserializeOwned>(value: &Value) -> (Result<T, Vec<Seg>>, Vec<Vec<Seg>>) {
    let mut unknown = Vec::new();
    let mut track = serde_path_to_error::Track::new();
    let result = {
        let de = serde_path_to_error::Deserializer::new(value, &mut track);
        serde_ignored::deserialize(de, |p| unknown.push(ignored_segs(&p)))
    };
    match result {
        Ok(t) => (Ok(t), unknown),
        Err(_) => {
            let path = track.path();
            let segs = path
                .iter()
                .filter_map(|s| match s {
                    serde_path_to_error::Segment::Seq { index } => Some(Seg::Index(*index)),
                    serde_path_to_error::Segment::Map { key } => Some(Seg::Key(key.clone())),
                    _ => None,
                })
                .collect();
            (Err(segs), unknown)
        }
    }
}

fn ignored_segs(p: &serde_ignored::Path<'_>) -> Vec<Seg> {
    use serde_ignored::Path;
    match p {
        Path::Root => Vec::new(),
        Path::Seq { parent, index } => {
            let mut v = ignored_segs(parent);
            v.push(Seg::Index(*index));
            v
        }
        Path::Map { parent, key } => {
            let mut v = ignored_segs(parent);
            v.push(Seg::Key(key.clone()));
            v
        }
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => ignored_segs(parent),
    }
}

/// Follow `segs` from `value`.
fn lookup<'v>(value: &'v Value, segs: &[Seg]) -> Option<&'v Value> {
    segs.iter().try_fold(value, |v, seg| match (v, seg) {
        (Value::Object(m), Seg::Key(k)) => m.get(k),
        (Value::Array(a), Seg::Index(i)) => a.get(*i),
        _ => None,
    })
}

fn lookup_mut<'v>(value: &'v mut Value, segs: &[Seg]) -> Option<&'v mut Value> {
    segs.iter().try_fold(value, |v, seg| match (v, seg) {
        (Value::Object(m), Seg::Key(k)) => m.get_mut(k),
        (Value::Array(a), Seg::Index(i)) => a.get_mut(*i),
        _ => None,
    })
}

/// Make the value at `segs` stop failing: remove an object member, or blank a list element (keeping the
/// indices of its siblings). When the same element fails twice (`whole_list`), the enclosing list field is
/// removed instead. If the path cannot be followed, the top-level field is removed. Returns the removed
/// path, or None when nothing can be removed.
fn neutralize(work: &mut Value, segs: &[Seg], whole_list: bool) -> Option<Vec<Seg>> {
    let (last, parent) = segs.split_last()?;
    let removed = match (lookup_mut(work, parent), last) {
        (Some(Value::Object(m)), Seg::Key(k)) => m.shift_remove(k).map(|_| segs.to_vec()),
        (Some(Value::Array(_)), Seg::Index(_)) if whole_list => {
            return neutralize(work, parent, false);
        }
        (Some(Value::Array(items)), Seg::Index(i)) => items.get_mut(*i).map(|item| {
            *item = Value::String(String::new());
            segs.to_vec()
        }),
        _ => None,
    };
    removed.or_else(|| {
        let first = segs.first()?;
        match (work, first) {
            (Value::Object(m), Seg::Key(k)) => m.shift_remove(k).map(|_| vec![first.clone()]),
            _ => None,
        }
    })
}

/// The kind of value expected at `segs` below an object of kind `root`.
fn kind_at(root: Obj, segs: &[Seg]) -> Option<Kind> {
    let mut cur = Kind::Obj(root);
    for seg in segs {
        cur = match (cur, seg) {
            (Kind::Obj(o), Seg::Key(k)) => fields::kind_of(o, k)?,
            (Kind::Normalize, Seg::Key(k)) => fields::kind_of(Obj::Normalize, k)?,
            (Kind::TrimSilence, Seg::Key(k)) => fields::kind_of(Obj::TrimSilence, k)?,
            (Kind::NameMap(o), Seg::Key(_)) => Kind::Obj(o),
            (Kind::StrList | Kind::SubtitleExport, Seg::Index(_)) => Kind::Str,
            (Kind::ObjList(o), Seg::Index(_)) => Kind::Obj(o),
            _ => return None,
        };
    }
    Some(cur)
}

/// The object kind at `segs` below `root`, if that location is an object of the DSL.
fn obj_at(root: Obj, segs: &[Seg]) -> Option<Obj> {
    match kind_at(root, segs)? {
        Kind::Obj(o) => Some(o),
        Kind::Normalize => Some(Obj::Normalize),
        Kind::TrimSilence => Some(Obj::TrimSilence),
        _ => None,
    }
}

/// Integer fields accept integral floats (`sample_rate: 48000.0`, or YAML's `007` which reads as 7.0).
fn coerce_integers(value: &mut Value, obj: Obj) {
    let Value::Object(map) = value else { return };
    for (key, kind) in fields::fields(obj) {
        let Some(v) = map.get_mut(key) else { continue };
        match kind {
            Kind::UInt => {
                if let Some(f) = v
                    .as_f64()
                    .filter(|f| v.is_f64() && f.fract() == 0.0 && *f >= 0.0 && *f <= 9.0e15)
                {
                    *v = Value::from(f as u64);
                }
            }
            Kind::Obj(inner) => coerce_integers(v, inner),
            _ => {}
        }
    }
}

/// Does `value` have the right JSON type for `kind`? (Used for hand-checked flag-or-object mappings.)
fn value_fits(kind: Kind, value: &Value) -> bool {
    match kind {
        _ if value.is_null() => true,
        Kind::Str => value.is_string(),
        Kind::Num => value.is_number(),
        Kind::UInt => value.is_u64(),
        Kind::Bool => value.is_boolean(),
        Kind::Millis | Kind::Bitrate | Kind::Version => value.is_number() || value.is_string(),
        _ => true,
    }
}

// ---- mechanical fixes -------------------------------------------------------------------------------

/// A replacement value with the diagnostic that explains it.
struct Fix {
    value: Value,
    message: String,
    hint: String,
}

/// A duration string with one corrected spelling (`"350MS"` → `"350ms"`, `"1:99.5"` → `"159.5s"`).
fn duration_fix(key: &str, s: &str) -> Option<Fix> {
    let error = values::parse_duration(s).err()?;
    let fixed = error.suggestion?;
    Some(Fix {
        hint: format!("Write `{key}: \"{fixed}\"`"),
        message: format!("`{key}`: {}", error.message),
        value: Value::String(fixed),
    })
}

/// The vocabulary of an enum-valued string field: a label for messages and the spelling corrector.
type SpellingOf = fn(&str) -> Option<&'static str>;

fn enum_field(obj: Obj, key: &str) -> Option<(&'static str, SpellingOf)> {
    Some(match (obj, key) {
        (Obj::Output, "format") => ("output format", OutputFormatName::spelling_of),
        (Obj::Output, "loudness_preset") => ("loudness preset", LoudnessPresetName::spelling_of),
        (Obj::Output, "export_subtitles") => ("subtitle format", SubtitleFormatName::spelling_of),
        (Obj::Timing, "speech_overlap") => {
            ("speech overlap policy", OverlapPolicyName::spelling_of)
        }
        (Obj::Track, "type") => ("track type", TrackKindName::spelling_of),
        (Obj::Track | Obj::Clip, "fade_curve") => ("fade curve", FadeCurveName::spelling_of),
        (Obj::Ducking, "mode") => ("ducking mode", DuckingModeName::spelling_of),
        (Obj::Provider, "type") => ("provider type", ProviderTypeName::spelling_of),
        _ => return None,
    })
}

/// An enum name written with the wrong case, separators or surrounding spaces (`Speech`, `equal-power`),
/// or a platform name where a loudness preset belongs (`youtube`).
fn enum_fix(obj: Obj, key: &str, s: &str, in_list: bool) -> Option<Fix> {
    let (what, spelling_of) = enum_field(obj, key)?;
    if enum_spelling_is_valid(obj, key, s) {
        return None;
    }
    let shown = s.trim();
    if let Some(fixed) = spelling_of(s) {
        let why = if shown == fixed {
            "remove the spaces around it"
        } else if shown.to_lowercase() == fixed {
            "names are lowercase"
        } else {
            "names are lowercase, with `_` between words"
        };
        let hint = if in_list {
            format!("Did you mean `{fixed}`? Write `{fixed}` in the `{key}` list")
        } else {
            format!("Did you mean `{fixed}`? Write `{key}: {fixed}`")
        };
        return Some(Fix {
            value: Value::String(fixed.to_string()),
            message: format!("unknown {what} `{shown}` ({why})"),
            hint,
        });
    }
    if obj == Obj::Output && key == "loudness_preset" {
        let (preset, why) = platform_preset(s)?;
        return Some(Fix {
            value: Value::String(preset.to_string()),
            message: format!("`{shown}` is a platform, not a loudness preset"),
            hint: format!("Use `loudness_preset: {preset}` ({why})"),
        });
    }
    None
}

/// The documented loudness preset for a delivery platform or standard (`YouTube` → `streaming`).
fn platform_preset(s: &str) -> Option<(&'static str, &'static str)> {
    let squashed: String = s
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    Some(match squashed.as_str() {
        "youtube" | "yt" | "youtubemusic" | "youtubeshorts" | "shorts" | "spotify" | "tiktok"
        | "instagram" | "reels" | "facebook" | "twitch" | "soundcloud" | "tidal" | "deezer"
        | "amazonmusic" | "social" | "socialvideo" | "socialmedia" | "musicstreaming" => (
            "streaming",
            "-14 LUFS, the level YouTube, Spotify and social video play at",
        ),
        "podcasts" | "applepodcasts" | "googlepodcasts" | "podcastapp" | "podcastapps"
        | "overcast" | "pocketcasts" | "voiceapps" | "web" | "webaudio" => (
            "podcast",
            "-16 LUFS, the level podcast and voice apps expect",
        ),
        "r128" | "ebur128" | "tv" | "television" | "broadcasttv" | "radio" => {
            ("broadcast", "EBU R128, -23 LUFS, for TV and radio")
        }
        "ustv" | "usbroadcast" | "atsca85" => ("atsc", "ATSC A/85, -24 LUFS, for US broadcast"),
        _ => return None,
    })
}

/// A positive number where the DSL only takes negative ones (loudness targets in LUFS, true-peak
/// ceilings in dBTP), when its negation is valid.
fn sign_fix(obj: Obj, key: &str, n: &Number) -> Option<Fix> {
    let v = n.as_f64().filter(|v| *v > 0.0)?;
    let (min, max, what, example) = match (obj, key) {
        (Obj::Output, "target_lufs") => (
            -60.0,
            -5.0,
            "Loudness values are negative",
            "podcast -16, streaming -14, broadcast -23",
        ),
        (Obj::Mix, "reference_lufs") | (Obj::Normalize, "target_lufs") => (
            -70.0,
            -5.0,
            "Loudness values are negative",
            "the default reference is -20",
        ),
        (Obj::Output, "true_peak_db") => (
            -9.0,
            0.0,
            "True-peak ceilings are negative",
            "-1.0 is typical",
        ),
        _ => return None,
    };
    if !(min..=max).contains(&-v) {
        return None;
    }
    let shown = values::trim_float(v);
    Some(Fix {
        value: Value::from(-v),
        message: format!(
            "`{key}` is {shown}, but {} values are negative",
            if key == "true_peak_db" {
                "dBTP"
            } else {
                "LUFS"
            }
        ),
        hint: format!("{what}: use `{key}: -{shown}` ({example})"),
    })
}

// ---- messages -----------------------------------------------------------------------------------------

/// The last path element for messages: `volume`, `trigger_tracks[1]`, `tracks[2]`.
fn display_field(path: &str) -> String {
    let last_dot = path.rfind('.').map(|i| i + 1).unwrap_or(0);
    path[last_dot..].to_string()
}

const MAX_ECHO_CHARS: usize = 40;

fn describe(value: Option<&Value>) -> String {
    match value {
        None => "nothing".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(b)) => format!("`{b}`"),
        Some(Value::Number(n)) => format!("the number {n}"),
        Some(Value::String(s)) if secrets::looks_like_secret_value(s) => {
            "a string that looks like a secret".to_string()
        }
        Some(Value::String(s)) => {
            let shown: String = s.chars().take(MAX_ECHO_CHARS).collect();
            if shown.len() < s.len() {
                format!("string {:?}…", shown)
            } else {
                format!("string {s:?}")
            }
        }
        Some(Value::Array(items)) if items.is_empty() => "an empty list".to_string(),
        Some(Value::Array(_)) => "a list".to_string(),
        Some(Value::Object(_)) => "a mapping".to_string(),
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::String(s) if !secrets::looks_like_secret_value(s) => {
            Some(s.chars().take(MAX_ECHO_CHARS).collect())
        }
        _ => None,
    }
}

/// A number as a hint shows it (`30`, `2.5`).
fn num_text(n: &Number) -> String {
    n.as_f64()
        .map(values::trim_float)
        .unwrap_or_else(|| n.to_string())
}

/// `n` seconds in milliseconds, as text (`2.5` → `2500`).
fn seconds_as_ms(n: &Number) -> Option<String> {
    let v = n.as_f64()?;
    let text = values::trim_float(v.abs());
    let ms = values::decimal_times(&text, 1000.0)?;
    let sign = if v < 0.0 { "-" } else { "" };
    Some(format!("{sign}{}", values::trim_float(ms)))
}

/// A string for a hint: bare when it is a plain word (`push`, `voice.cue1`), JSON-quoted otherwise (which
/// is also valid YAML). None for secrets and long or multi-line strings.
fn render_str(s: &str) -> Option<String> {
    if secrets::looks_like_secret_value(s)
        || s.chars().count() > 2 * MAX_ECHO_CHARS
        || s.contains(['\n', '\r'])
    {
        return None;
    }
    let reserved = matches!(
        s.to_ascii_lowercase().as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "y" | "n" | "~"
    );
    let bare = s
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        && !reserved;
    if bare {
        Some(s.to_string())
    } else {
        serde_json::to_string(s).ok()
    }
}

/// A short value for a hint: numbers and booleans as written, strings via [`render_str`], short lists of
/// scalars as `[a, b]`. None for mappings, long lists and strings that must not be echoed.
fn render_value(value: &Value) -> Option<String> {
    match value {
        Value::Number(n) => Some(num_text(n)),
        Value::Bool(b) => Some(b.to_string()),
        Value::String(s) => render_str(s),
        Value::Array(items) if !items.is_empty() && items.len() <= 6 => {
            let parts: Option<Vec<String>> = items
                .iter()
                .map(|item| match item {
                    Value::Array(_) | Value::Object(_) | Value::Null => None,
                    other => render_value(other),
                })
                .collect();
            Some(format!("[{}]", parts?.join(", ")))
        }
        _ => None,
    }
}

/// `value` rendered for a field of `kind`, when it has the right shape for it.
fn render_for(kind: Option<Kind>, value: &Value) -> Option<String> {
    match (kind?, value) {
        (Kind::Str, Value::String(_))
        | (Kind::Num | Kind::UInt, Value::Number(_))
        | (Kind::Bool, Value::Bool(_))
        | (Kind::Millis | Kind::Bitrate | Kind::Version, Value::Number(_) | Value::String(_))
        | (Kind::StrList, Value::Array(_))
        | (Kind::SubtitleExport, Value::Bool(_) | Value::String(_) | Value::Array(_))
        | (Kind::Normalize | Kind::TrimSilence, Value::Bool(_)) => render_value(value),
        _ => None,
    }
}

/// An example value for numeric fields, for hints.
fn numeric_example(field: &str) -> &'static str {
    match field {
        "sample_rate" => "48000",
        "channels" => "2",
        "bit_depth" => "24",
        "max_concurrency" => "2",
        "timeout_ms" => "120000",
        "max_chars_per_line" => "42",
        "max_lines" => "2",
        "volume" => "0.8",
        "pan" => "-0.2",
        "target_lufs" | "reference_lufs" => "-16",
        "true_peak_db" => "-1.0",
        "reduction_db" => "-15",
        "threshold_db" => "-40",
        "chars_per_second" => "15",
        "words_per_minute" => "150",
        f if f.ends_with("_db") => "-6",
        _ => "1.0",
    }
}

/// The number a whole-number field most likely means, for a value of the wrong type: `"44.1k"` / `44.1`
/// for `sample_rate` (44100), `"stereo"` for `channels` (2), `"24-bit"` for `bit_depth`, a duration string
/// for `timeout_ms`, or a quoted integer.
fn uint_conversion(field: &str, actual: &Value) -> Option<(u64, &'static str)> {
    let whole =
        |v: f64| (v.is_finite() && v >= 0.0 && v.fract() == 0.0 && v <= 9.0e15).then_some(v as u64);
    match actual {
        Value::Number(n) if field == "sample_rate" => {
            // 44.1 or 48 (kHz).
            let khz = n.as_f64().filter(|v| *v > 0.0 && *v < 1000.0)?;
            let hz = values::decimal_times(&values::trim_float(khz), 1000.0)?;
            whole(hz).map(|hz| (hz, "the sample rate is in Hz"))
        }
        Value::String(s) => {
            let t = s.trim().to_ascii_lowercase();
            match field {
                "sample_rate" => {
                    let (number, factor) =
                        if let Some(n) = t.strip_suffix("khz").or_else(|| t.strip_suffix('k')) {
                            (n, 1000.0)
                        } else {
                            (t.strip_suffix("hz").unwrap_or(&t), 1.0)
                        };
                    whole(values::decimal_times(number.trim(), factor)?)
                        .filter(|hz| *hz > 0)
                        .map(|hz| (hz, "the sample rate is a plain number of Hz"))
                }
                "channels" => match t.as_str() {
                    "mono" | "1" => Some((1, "channels is a number")),
                    "stereo" | "2" => Some((2, "channels is a number")),
                    _ => None,
                },
                "bit_depth" => {
                    let digits = t
                        .strip_suffix("bits")
                        .or_else(|| t.strip_suffix("bit"))
                        .unwrap_or(&t)
                        .trim_end_matches(['-', ' ']);
                    digits
                        .parse()
                        .ok()
                        .map(|b| (b, "the bit depth is a plain number"))
                }
                "timeout_ms" => {
                    let ms = values::parse_duration(&t).ok()?;
                    whole(ms).map(|ms| (ms, "the timeout is a whole number of milliseconds"))
                }
                _ => t.parse().ok().map(|n| (n, "remove the quotes")),
            }
        }
        _ => None,
    }
}

/// A number field written as a string with a unit or a percentage: `"50%"` (volume, pan), `"-6dB"`,
/// `"-16 LUFS"`, `"2x"` (volume).
fn num_conversion(field: &str, actual: &Value) -> Option<String> {
    let s = actual.as_str()?.trim();
    if let Some(percent) = s.strip_suffix('%') {
        let p: f64 = percent
            .trim()
            .parse()
            .ok()
            .filter(|p: &f64| p.is_finite())?;
        let fraction = values::trim_float(p / 100.0);
        return match field {
            "volume" => Some(format!(
                "`volume` is a linear factor: write `volume: {fraction}` ({} % as a linear gain; 1.0 = unchanged)",
                values::trim_float(p)
            )),
            "pan" => Some(format!(
                "Write `pan: {fraction}` (-1 = hard left, 0 = center, 1 = hard right)"
            )),
            _ => None,
        };
    }
    let lower = s.to_ascii_lowercase();
    let units: &[&str] = match field {
        "volume" => &["x"],
        "target_lufs" | "reference_lufs" => &["lufs", "lkfs", "lu", "db"],
        "true_peak_db" => &["dbtp", "dbfs", "db"],
        f if f.ends_with("_db") => &["dbfs", "db"],
        _ => &[],
    };
    let number = units.iter().find_map(|u| lower.strip_suffix(u))?.trim();
    let v: f64 = number.parse().ok().filter(|v: &f64| v.is_finite())?;
    Some(format!(
        "Write the number without the unit and without quotes: `{field}: {}`",
        values::trim_float(v)
    ))
}

fn type_hint(kind: Option<Kind>, actual: Option<&Value>, field: &str) -> String {
    let field = field.split('[').next().unwrap_or(field);
    let actual = actual.unwrap_or(&Value::Null);
    match kind {
        Some(Kind::Str) if field == "export_subtitles" => {
            "List subtitle format names, e.g. `export_subtitles: [vtt, srt, json]`".to_string()
        }
        Some(Kind::Str) => match scalar_text(actual) {
            Some(text) if !actual.is_string() => format!("Quote the value: `{field}: \"{text}\"`"),
            _ => format!("Write `{field}` as text, e.g. `{field}: \"...\"`"),
        },
        Some(Kind::Bool) => match actual.as_str() {
            Some(s) if matches!(s.to_ascii_lowercase().as_str(), "true" | "false") => {
                format!("Remove the quotes: `{field}: {}`", s.to_ascii_lowercase())
            }
            Some(s) if matches!(s.to_ascii_lowercase().as_str(), "yes" | "no" | "on" | "off" | "y" | "n") => {
                format!(
                    "Use `{field}: true` or `{field}: false`; YAML 1.1 words like `{s}` are read as plain strings"
                )
            }
            _ => format!("Use `{field}: true` or `{field}: false`"),
        },
        Some(Kind::Num) => {
            if let Some(hint) = num_conversion(field, actual) {
                return hint;
            }
            match actual.as_str().map(str::trim).filter(|s| s.parse::<f64>().is_ok()) {
                Some(num) => format!("Remove the quotes: `{field}: {num}`"),
                None => format!("Use a plain number, e.g. `{field}: {}`", numeric_example(field)),
            }
        }
        Some(Kind::UInt) => match uint_conversion(field, actual) {
            Some((n, why)) => format!("Write a whole number: `{field}: {n}` ({why})"),
            None => format!(
                "Use a whole number without quotes or decimals, e.g. `{field}: {}`",
                numeric_example(field)
            ),
        },
        Some(Kind::Millis) => {
            format!("Write `{field}` as a number of milliseconds (`{field}: 350`) or a string like `{field}: \"1.5s\"`")
        }
        Some(Kind::Bitrate) => format!("Use a string like `{field}: \"192k\"` or a number like `{field}: 192000`"),
        Some(Kind::Version) => "Use `version: \"1.0\"`".to_string(),
        Some(Kind::StrList) => match scalar_text(actual) {
            Some(text) if field == "command" && actual.is_string() => format!(
                "Write the command as a list of arguments, one per program word, e.g. `command: [{}, ...]`",
                text.split_whitespace().next().unwrap_or("program")
            ),
            Some(text) => format!("Use a list: `{field}: [{text}]`"),
            None => format!("Use a list of strings, e.g. `{field}: [a, b]`"),
        },
        Some(Kind::ObjList(Obj::Track)) => {
            "Write `tracks:` as a list of mappings, each starting with `- id: ...` and `type: speech|music|sfx`"
                .to_string()
        }
        Some(Kind::ObjList(Obj::SequenceItem)) => {
            "Write `sequence:` as a list of cues (`- text: \"...\"`) and pauses (`- pause_duration: 350ms`)".to_string()
        }
        Some(Kind::ObjList(_)) => "Write `clips:` as a list of mappings like `- source: audio/bed.wav`".to_string(),
        Some(Kind::Obj(Obj::Track)) => {
            "Write each track as a mapping, e.g. `- id: narration` / `type: speech` / `sequence: [...]`".to_string()
        }
        Some(Kind::Obj(Obj::SequenceItem)) => match actual {
            Value::String(s) if !secrets::looks_like_secret_value(s) => {
                let shown: String = s.chars().take(MAX_ECHO_CHARS).collect();
                format!("Write a cue as a mapping: `- text: {shown:?}`")
            }
            Value::Number(n) => format!("Write a pause as a mapping: `- pause_duration: {n}`"),
            _ => "Write each item as a cue (`- text: \"...\"`) or a pause (`- pause_duration: 350ms`)".to_string(),
        },
        Some(Kind::Obj(Obj::Clip)) => match scalar_text(actual) {
            Some(text) if actual.is_string() => format!("Write the clip as a mapping: `- source: {text:?}`"),
            _ => "Write each clip as a mapping like `- source: audio/bed.wav`".to_string(),
        },
        Some(Kind::Obj(Obj::Voice)) if actual.is_string() => match scalar_text(actual) {
            Some(text) => format!(
                "Write it as a mapping, e.g. `{field}: {{label: {text:?}}}` or `{field}: {{voice_id: {text:?}}}`"
            ),
            None => format!("Write it as a mapping, e.g. `{field}: {{provider: mock}}`"),
        },
        Some(Kind::Obj(Obj::Subtitles)) if matches!(actual, Value::Bool(_) | Value::String(_) | Value::Array(_)) => {
            let value = render_for(Some(Kind::SubtitleExport), actual).unwrap_or_else(|| "true".to_string());
            format!(
                "Subtitle files are switched on in `output`: write `output: {{export_subtitles: {value}}}`; the \
                 top-level `subtitles` mapping only sets their style (e.g. `subtitles: {{max_lines: 2}}`)"
            )
        }
        Some(Kind::Obj(obj)) => {
            let keys = fields::names(obj);
            let shown: Vec<&str> = keys.iter().take(3).copied().collect();
            format!("Use a mapping with keys such as {}", diagnostics::list_names(shown, 3))
        }
        Some(Kind::NameMap(Obj::Voice)) => {
            "Write `speakers:` as a mapping from speaker names to voice settings, e.g. `speakers: {narrator: \
             {provider: mock}}`"
                .to_string()
        }
        Some(Kind::NameMap(_)) => {
            "Write `providers:` as a mapping from instance names to settings, e.g. `providers: {eleven: {type: \
             elevenlabs}}`"
                .to_string()
        }
        Some(Kind::FreeMap) => format!("Use a mapping of provider-specific settings, e.g. `{field}: {{speed: 1.1}}`"),
        Some(Kind::Normalize) => {
            format!("Use `{field}: true`, `{field}: false` or `{field}: {{target_lufs: -20}}`")
        }
        Some(Kind::TrimSilence) => {
            format!("Use `{field}: true`, `{field}: false` or `{field}: {{threshold_db: -50, padding_ms: 30}}`")
        }
        Some(Kind::SubtitleExport) => format!("Use `{field}: true` or a list such as `{field}: [vtt, srt, json]`"),
        None => "Check the value's type against the timeline schema (`soniccraft schema`)".to_string(),
    }
}

// ---- unknown-key hints ----------------------------------------------------------------------------------

/// Everything the hint for an unknown key looks at.
struct UnknownKey<'a> {
    /// The key as written.
    key: &'a str,
    /// The key in snake_case ([`diagnostics::normalize_key`]).
    normalized: String,
    /// Its value.
    value: Option<&'a Value>,
    /// The mapping it is in (its siblings).
    siblings: Option<&'a Map<String, Value>>,
    /// The kind of that mapping, if it is an object of the DSL.
    parent: Option<Obj>,
    /// The track type, when the mapping is a track (None: not a track, or unknown type).
    track_kind: Option<TrackKindName>,
    /// True when the mapping is an entry of `speakers`.
    speaker_entry: bool,
}

impl UnknownKey<'_> {
    fn number(&self) -> Option<&Number> {
        match self.value {
            Some(Value::Number(n)) => Some(n),
            _ => None,
        }
    }

    /// The key names a millisecond unit (`pause_ms`, `delay_msec`).
    fn says_ms(&self) -> bool {
        let k = self.normalized.as_str();
        ["_ms", "_msec", "_msecs", "_millis", "_milliseconds"]
            .iter()
            .any(|s| k.ends_with(s))
            || k == "ms"
    }

    /// The key names seconds (`duration_s`, `timeout_seconds`).
    fn says_seconds(&self) -> bool {
        let k = self.normalized.as_str();
        ["_s", "_sec", "_secs", "_second", "_seconds"]
            .iter()
            .any(|s| k.ends_with(s))
            || matches!(k, "s" | "sec" | "secs" | "seconds")
    }

    fn sibling_str(&self, keys: &[&str]) -> Option<&str> {
        let siblings = self.siblings?;
        keys.iter()
            .find_map(|k| siblings.get(*k).and_then(Value::as_str))
    }
}

/// The keys valid in `parent` (track keys depend on the track type).
fn local_keys(parent: Obj, track_kind: Option<TrackKindName>) -> Vec<&'static str> {
    match (parent, track_kind) {
        (Obj::Track, Some(TrackKindName::Speech)) => fields::TRACK_COMMON
            .iter()
            .chain(fields::TRACK_SPEECH)
            .map(|(n, _)| *n)
            .collect(),
        (Obj::Track, Some(_)) => fields::TRACK_COMMON
            .iter()
            .chain(fields::TRACK_AUDIO)
            .map(|(n, _)| *n)
            .collect(),
        _ => fields::names(parent),
    }
}

/// "`a`, `b` or `c`".
fn or_list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("`{n}`")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        Some((last, _)) => last.clone(),
        None => String::new(),
    }
}

/// Every word of `typed` (3+ characters in total) is a word of `candidate` (`overlap` → `speech_overlap`).
fn shares_words(typed: &str, candidate: &str) -> bool {
    let typed_words: Vec<&str> = typed.split('_').filter(|w| !w.is_empty()).collect();
    let words: Vec<&str> = candidate.split('_').collect();
    typed.len() >= 3 && !typed_words.is_empty() && typed_words.iter().all(|w| words.contains(w))
}

/// For a rename to the millisecond field `target` (of `kind`: a duration, or a whole number named `…_ms`)
/// of a key whose value is a bare number and whose name does not say `ms`: how to write the number as
/// seconds. Empty otherwise.
fn unit_note(u: &UnknownKey<'_>, kind: Option<Kind>, target: &str) -> String {
    let Some(n) = u.number() else {
        return String::new();
    };
    let ms_field = match kind {
        Some(Kind::Millis) => true,
        Some(Kind::UInt) => target.ends_with("_ms"),
        _ => false,
    };
    if u.says_ms() || !ms_field {
        return String::new();
    }
    let (text, Some(ms)) = (num_text(n), seconds_as_ms(n)) else {
        return String::new();
    };
    if n.as_f64() == Some(0.0) {
        // 0 ms is 0 s.
        return String::new();
    }
    match kind {
        Some(Kind::Millis) if u.says_seconds() => {
            format!(" Write `{target}: \"{text}s\"` ({ms} ms)")
        }
        Some(Kind::Millis) => format!(
            " Times are milliseconds: write `{target}: \"{text}s\"` ({ms} ms) if you meant seconds"
        ),
        // Whole-number millisecond fields (`timeout_ms`) take no unit strings.
        Some(Kind::UInt) if u.says_seconds() => format!(" Write `{target}: {ms}` ({text} s)"),
        Some(Kind::UInt) => {
            format!(" Times are milliseconds: write `{target}: {ms}` if you meant {text} seconds")
        }
        _ => String::new(),
    }
}

/// Hint for an unknown key (see the module docs for the order of the steps).
fn unknown_hint(u: &UnknownKey<'_>) -> String {
    let annotation = "Prefix custom keys with `x-` to keep them as ignored annotations";
    let Some(parent) = u.parent else {
        return annotation.to_string();
    };
    let local = local_keys(parent, u.track_kind);
    let normalized = u.normalized.as_str();

    let case_variant = local.iter().any(|n| n.eq_ignore_ascii_case(normalized));

    // 1. A key that means something else in the DSL, at this location (not a case variant of a valid key).
    if !case_variant
        && let Some(synonym) = fields::synonym(parent, u.track_kind, u.speaker_entry, normalized)
    {
        return synonym_hint(u, parent, synonym);
    }

    // 2. The key exists verbatim at another location: it was put in the wrong place.
    let elsewhere: Vec<&'static str> = fields::ALL_OBJS
        .iter()
        .filter(|&&o| o != parent)
        .filter(|&&o| {
            fields::names(o)
                .iter()
                .any(|n| n.eq_ignore_ascii_case(normalized))
        })
        .map(|&o| fields::key_location(o, normalized))
        .collect();
    if !elsewhere.is_empty() && !case_variant {
        let mut hint = format!(
            "`{normalized}` is not valid here; it belongs in {}",
            elsewhere.join(" or ")
        );
        if parent == Obj::Track {
            hint.push_str(match normalized {
                "trigger_tracks" | "reduction_db" | "attack_ms" | "release_ms" | "hold_ms"
                | "lookahead_ms" | "mode" => {
                    ", e.g. `ducking: {trigger_tracks: [voice], reduction_db: -15}`"
                }
                "text" | "caption" | "pause_duration" | "pause" => {
                    ", e.g. `sequence: [{text: \"...\"}]`"
                }
                "crossfade_ms" => ": use a `clips:` list to crossfade between clips",
                "target_lufs" => ": write `normalize: {target_lufs: -20}`",
                "after_cue" | "with_cue" | "before_cue" | "offset_ms" => {
                    ", e.g. `start_relative: {after_cue: cue_01}`"
                }
                _ => "",
            });
        }
        return hint;
    }

    // 3. A close match among the keys valid here.
    if let Some(candidate) = diagnostics::did_you_mean(u.key, local.iter().copied()) {
        let note = if candidate.eq_ignore_ascii_case(normalized) && candidate != u.key {
            " (keys are snake_case)"
        } else {
            ""
        };
        let units = unit_note(u, fields::kind_of(parent, candidate), candidate);
        return format!("Did you mean `{candidate}`?{note}{units}");
    }
    // 4. Keys valid here that contain the typed words: list them all, rather than a key of another object.
    let sharing: Vec<&str> = local
        .iter()
        .copied()
        .filter(|c| shares_words(normalized, c))
        .collect();
    match sharing.as_slice() {
        [single] => {
            let units = unit_note(u, fields::kind_of(parent, single), single);
            return format!("Did you mean `{single}`?{units}");
        }
        several if (2..=4).contains(&several.len()) => {
            return format!("Did you mean {}?", or_list(several));
        }
        _ => {}
    }
    // 5. A track key that does not apply to this track type.
    if parent == Obj::Track
        && let Some(candidate) = diagnostics::did_you_mean(u.key, fields::names(Obj::Track))
    {
        let kind = u.track_kind.map(|k| k.as_str()).unwrap_or("this");
        return format!(
            "Did you mean `{candidate}`? (note: `{candidate}` does not apply to {kind} tracks)"
        );
    }
    // 6. A close match elsewhere.
    for &o in fields::ALL_OBJS.iter().filter(|&&o| o != parent) {
        if let Some(candidate) = diagnostics::did_you_mean(u.key, fields::names(o))
            && candidate.len() > 3
        {
            let units = unit_note(u, fields::kind_of(o, candidate), candidate);
            return format!(
                "Did you mean `{candidate}`? It belongs in {}{units}",
                fields::key_location(o, candidate)
            );
        }
    }
    // 7. The list of valid keys.
    format!(
        "Valid keys here: {}. {annotation}",
        diagnostics::list_names(local.iter().copied(), 40)
    )
}

/// The hint for a semantic synonym (`file` → `source`, `start_after` → `start_relative`, …).
fn synonym_hint(u: &UnknownKey<'_>, parent: Obj, synonym: Synonym) -> String {
    let key = u.key;
    match synonym {
        Synonym::Key(target) => rename_hint(u, parent, target),
        Synonym::Anchor(field, anchor) => {
            let target = u
                .value
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .and_then(render_str);
            let action = |t: &str| match (field, anchor) {
                ("start_relative", "after_cue") => format!("start after `{t}` ends"),
                ("start_relative", "with_cue") => format!("start together with `{t}`"),
                ("start_relative", _) => format!("end just as `{t}` starts"),
                (_, "after_cue") => format!("end when `{t}` ends"),
                _ => format!("end when `{t}` starts"),
            };
            match target {
                Some(t) => format!("To {}, write `{field}: {{{anchor}: {t}}}`", action(&t)),
                None => format!(
                    "Write `{field}: {{{anchor}: <id>}}` with the id of the cue, clip or track to follow (`{key}` is \
                     not a DSL key)"
                ),
            }
        }
        Synonym::Setting => {
            let value = u
                .value
                .and_then(render_value)
                .unwrap_or_else(|| "...".to_string());
            format!(
                "`{key}` is a provider voice setting: put it inside `settings`, e.g. `settings: {{{key}: {value}}}`"
            )
        }
        Synonym::Pause => pause_hint(u),
        Synonym::Gain => match u.number().and_then(Number::as_f64) {
            Some(g) if !(0.0..=4.0).contains(&g) => format!(
                "Did you mean `gain_db: {}`? (`gain_db` is in decibels; `volume` is a linear factor, 1.0 = \
                 unchanged)",
                values::trim_float(g)
            ),
            Some(g) => {
                let g = values::trim_float(g);
                format!(
                    "`gain` is ambiguous: write `volume: {g}` for a linear factor (1.0 = unchanged) or `gain_db: \
                     {g}` for decibels (0 = unchanged)"
                )
            }
            None => "`gain` is ambiguous: use `volume` for a linear factor (1.0 = unchanged) or `gain_db` for \
                     decibels (0 = unchanged)"
                .to_string(),
        },
        Synonym::Offset => {
            let value = u
                .value
                .and_then(|v| render_for(Some(Kind::Millis), v))
                .unwrap_or_else(|| "500".to_string());
            let units = unit_note(u, Some(Kind::Millis), "trim_start_ms");
            format!(
                "To skip into the audio file write `trim_start_ms: {value}`; to start it later on the timeline \
                 write `start_at_ms: {value}`.{units}"
            )
        }
        Synonym::AnchorTarget => anchor_target_hint(u),
        Synonym::DefaultsField(field) => {
            let value = u
                .value
                .and_then(|v| render_for(Some(Kind::Str), v))
                .unwrap_or_else(|| "...".to_string());
            format!("`{key}` is not a top-level key: set it for every cue with `defaults: {{{field}: {value}}}`")
        }
        Synonym::ExportSubtitles => {
            let value = u
                .value
                .and_then(|v| render_for(Some(Kind::SubtitleExport), v))
                .unwrap_or_else(|| "[vtt, srt]".to_string());
            format!("Subtitle export is switched on in `output`: write `output: {{export_subtitles: {value}}}`")
        }
        Synonym::AudioOnSpeech => {
            let file = u
                .value
                .and_then(Value::as_str)
                .and_then(render_str)
                .unwrap_or_else(|| "\"audio/file.wav\"".to_string());
            format!(
                "Speech tracks only speak their cues; to play {file}, add a separate track \
                 `{{id: sound, type: sfx, source: {file}}}` (or `type: music` for a bed)"
            )
        }
    }
}

/// "Did you mean `target`?" plus the value written for `target` (converted where the units differ).
fn rename_hint(u: &UnknownKey<'_>, parent: Obj, target: &str) -> String {
    let kind = fields::kind_of(parent, target);
    let mut hint = format!("Did you mean `{target}`?");
    let units = unit_note(u, kind, target);
    if !units.is_empty() {
        hint.push_str(&units);
        return hint;
    }
    let Some(value) = u.value else { return hint };
    let written = match (target, value) {
        // A linear factor where a speaker's gain in dB belongs.
        ("gain_db", Value::Number(n))
            if matches!(
                u.normalized.as_str(),
                "volume" | "vol" | "level" | "loudness"
            ) =>
        {
            let linear = n.as_f64().filter(|v| *v > 0.0 && *v <= 4.0);
            return match linear {
                Some(v) => format!(
                    "Did you mean `gain_db`? Speaker gain is in decibels: `{key}: {}` is about `gain_db: {}`",
                    values::trim_float(v),
                    values::trim_float((20.0 * v.log10() * 10.0).round() / 10.0),
                    key = u.key
                ),
                None => {
                    format!("{hint} Speaker gain is in decibels (0 = unchanged, -6 ≈ half as loud)")
                }
            };
        }
        ("channels", _) if u.normalized == "mono" => Some("1".to_string()),
        ("channels", _) if u.normalized == "stereo" => Some("2".to_string()),
        ("ducking", Value::String(_) | Value::Array(_)) => {
            let triggers = match value {
                Value::String(s) => render_str(s).map(|s| format!("[{s}]")),
                other => render_value(other),
            };
            triggers.map(|t| format!("{{trigger_tracks: {t}}}"))
        }
        ("target_lufs" | "reference_lufs" | "true_peak_db", Value::Number(n))
            if n.as_f64().is_some_and(|v| v > 0.0) =>
        {
            Some(format!("-{}", num_text(n)))
        }
        (_, Value::String(s)) => match enum_field(parent, target) {
            // Enum values: the accepted spelling, or nothing (the value is checked after the rename).
            Some(_) if enum_spelling_is_valid(parent, target, s) => render_str(s),
            Some((_, spelling_of)) => spelling_of(s).map(str::to_string).or_else(|| {
                (parent == Obj::Output && target == "loudness_preset")
                    .then(|| platform_preset(s).map(|(preset, _)| preset.to_string()))
                    .flatten()
            }),
            None => render_for(kind, value),
        },
        _ => render_for(kind, value),
    };
    if let Some(written) = written {
        hint.push_str(&format!(" Write `{target}: {written}`"));
    }
    hint
}

/// True when `s` is an accepted spelling of the enum field `target` of `parent`.
fn enum_spelling_is_valid(parent: Obj, target: &str, s: &str) -> bool {
    match (parent, target) {
        (Obj::Output, "format") => OutputFormatName::parse(s).is_some(),
        (Obj::Output, "loudness_preset") => LoudnessPresetName::parse(s).is_some(),
        (Obj::Output, "export_subtitles") => SubtitleFormatName::parse(s).is_some(),
        (Obj::Timing, "speech_overlap") => OverlapPolicyName::parse(s).is_some(),
        (Obj::Track, "type") => TrackKindName::parse(s).is_some(),
        (Obj::Track | Obj::Clip, "fade_curve") => FadeCurveName::parse(s).is_some(),
        (Obj::Ducking, "mode") => DuckingModeName::parse(s).is_some(),
        (Obj::Provider, "type") => ProviderTypeName::parse(s).is_some(),
        _ => false,
    }
}

/// A pause written with another name (`pause_ms`, `wait`, `silence`), or next to a cue's `text`.
fn pause_hint(u: &UnknownKey<'_>) -> String {
    let value = match u.value {
        Some(Value::Number(n)) if u.says_seconds() => Some(format!("\"{}s\"", num_text(n))),
        Some(v @ (Value::Number(_) | Value::String(_))) => render_for(Some(Kind::Millis), v),
        _ => None,
    };
    let units = if u.says_seconds() {
        String::new()
    } else {
        unit_note(u, Some(Kind::Millis), "pause")
    };
    let has_text = u.siblings.is_some_and(|m| m.contains_key("text"));
    let example = value.unwrap_or_else(|| "300".to_string());
    if has_text {
        format!(
            "A sequence item is either a cue or a pause: move the pause into its own item before or after this \
             cue, e.g. `- pause: {example}`.{units}"
        )
    } else {
        // Starts with "Did you mean `pause`": lowering then treats the item as the pause it is meant to be.
        format!(
            "Did you mean `pause`? Write `pause: {example}`; a pause item has only `pause` (or \
             `pause_duration`).{units}"
        )
    }
}

/// An anchor target or edge written with another key (`{cue: a1}`, `{cue: a1, edge: end}`).
fn anchor_target_hint(u: &UnknownKey<'_>) -> String {
    const TARGET_KEYS: &[&str] = &["cue", "target", "ref", "id", "item", "anchor", "reference"];
    const EDGE_KEYS: &[&str] = &["edge", "point", "side", "at", "mode"];
    let (target, edge) = if EDGE_KEYS.contains(&u.normalized.as_str()) {
        (u.sibling_str(TARGET_KEYS), u.value.and_then(Value::as_str))
    } else {
        (u.value.and_then(Value::as_str), u.sibling_str(EDGE_KEYS))
    };
    let anchor_key = edge.and_then(|e| match e.trim().to_ascii_lowercase().as_str() {
        "end" | "after" | "finish" | "stop" | "ends" => Some("after_cue"),
        "start" | "begin" | "with" | "same" | "starts" => Some("with_cue"),
        "before" => Some("before_cue"),
        _ => None,
    });
    let target = target.filter(|t| !t.trim().is_empty()).and_then(render_str);
    // Keep an offset written next to it.
    let offset = u
        .siblings
        .and_then(|m| m.get("offset_ms"))
        .and_then(|v| render_for(Some(Kind::Millis), v))
        .map(|v| format!(", offset_ms: {v}"))
        .unwrap_or_default();
    match (anchor_key, target) {
        (Some(anchor), Some(t)) => {
            format!("Did you mean `{anchor}`? Write the anchor as `{{{anchor}: {t}{offset}}}`")
        }
        (Some(anchor), None) => {
            format!("Did you mean `{anchor}`? Write the anchor as `{{{anchor}: <id>}}`")
        }
        (None, Some(t)) => format!(
            "Name the target with exactly one of `after_cue` (start after it ends), `with_cue` (start with it) or \
             `before_cue` (end as it starts), e.g. `{{after_cue: {t}}}`"
        ),
        (None, None) => {
            "Name the target with exactly one of `after_cue`, `with_cue` or `before_cue`, e.g. \
                         `{after_cue: cue_01, offset_ms: 150}`"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(doc: Value) -> Decoded {
        let Value::Object(map) = doc else {
            panic!("object")
        };
        decode(&map, &HashSet::new())
    }

    fn codes_paths(d: &Decoded) -> Vec<(String, String)> {
        d.diagnostics
            .iter()
            .map(|d| (d.code.clone(), d.path.clone().unwrap_or_default()))
            .collect()
    }

    fn hint_at(d: &Decoded, path: &str) -> String {
        d.diagnostics
            .iter()
            .find(|x| x.path.as_deref() == Some(path))
            .and_then(|x| x.hint.clone())
            .unwrap_or_else(|| panic!("no diagnostic at {path}: {:?}", codes_paths(d)))
    }

    #[test]
    fn clean_document_has_no_diagnostics() {
        let d = run(json!({
            "version": "1.0",
            "output": {"format": "mp3", "sample_rate": 48000.0},
            "speakers": {"narrator": {"provider": "mock"}, "guest": null},
            "tracks": [{"id": "v", "type": "speech", "normalize": {"target_lufs": -18},
                        "sequence": [{"text": "hi", "x-note": 1}, {"pause": "300ms"}]}]
        }));
        assert!(d.diagnostics.is_empty(), "{:?}", d.diagnostics);
        assert_eq!(
            d.timeline.output.as_ref().and_then(|o| o.sample_rate),
            Some(48000)
        );
        assert_eq!(d.timeline.speakers.len(), 2);
        assert_eq!(
            d.timeline.tracks[0].sequence.as_ref().map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn independent_type_errors_are_all_reported() {
        let d = run(json!({
            "version": "1.0",
            "tracks": [
                {"id": "a", "type": "music", "source": "a.wav", "volume": "loud"},
                {"id": "b", "type": "speech", "sequence": [{"text": "x", "gain_db": "big"}, "oops", {"text": "y"}]}
            ]
        }));
        let got = codes_paths(&d);
        assert!(
            got.contains(&("invalid_type".into(), "tracks[0].volume".into())),
            "{got:?}"
        );
        assert!(
            got.contains(&(
                "invalid_type".into(),
                "tracks[1].sequence[0].gain_db".into()
            )),
            "{got:?}"
        );
        assert!(
            got.contains(&("invalid_type".into(), "tracks[1].sequence[1]".into())),
            "{got:?}"
        );
        let vol = d
            .diagnostics
            .iter()
            .find(|x| x.path.as_deref() == Some("tracks[0].volume"))
            .unwrap();
        assert_eq!(
            vol.message,
            "`volume` must be a number, got string \"loud\""
        );
        // Elements survive with indices intact.
        assert_eq!(d.timeline.tracks[0].source.as_deref(), Some("a.wav"));
        let seq = d.timeline.tracks[1].sequence.as_ref().unwrap();
        assert_eq!(seq.len(), 3);
        assert_eq!(seq[2].text.as_deref(), Some("y"));
        assert!(d.dropped.contains(&"tracks[1].sequence[1]".to_string()));
    }

    #[test]
    fn unknown_keys_with_did_you_mean() {
        let d = run(json!({
            "version": "1.0",
            "output": {"formatt": "mp3", "sampleRate": 44100},
            "tracks": [{"id": "bed", "type": "music", "source": "a.wav", "fade_in": 100,
                        "ducking": {"triggers": ["v"]}, "trigger_tracks": ["v"],
                        "normalize": {"target": -20}}]
        }));
        let hint = |p: &str| hint_at(&d, p);
        assert!(
            hint("output.formatt").contains("`format`"),
            "{}",
            hint("output.formatt")
        );
        assert!(hint("output.sampleRate").contains("`sample_rate`"));
        assert!(hint("tracks[0].fade_in").contains("`fade_in_ms`"));
        assert!(hint("tracks[0].ducking.triggers").contains("`trigger_tracks`"));
        assert!(
            hint("tracks[0].trigger_tracks").contains("belongs in `ducking`"),
            "{}",
            hint("tracks[0].trigger_tracks")
        );
        assert!(hint("tracks[0].normalize.target").contains("`target_lufs`"));
        let msg = d
            .diagnostics
            .iter()
            .find(|x| x.path.as_deref() == Some("tracks[0].fade_in"))
            .unwrap();
        assert_eq!(msg.message, "unknown field `fade_in` in music track `bed`");
    }

    #[test]
    fn flag_or_object_inner_errors() {
        let d = run(json!({
            "version": "1.0",
            "output": {"export_subtitles": ["vtt", 3]},
            "tracks": [{"id": "v", "type": "speech", "sequence": [{"text": "x"}],
                        "trim_silence": {"threshold_db": "low", "pad": 3}, "normalize": "yes"}]
        }));
        let got = codes_paths(&d);
        assert!(
            got.contains(&("invalid_type".into(), "output.export_subtitles[1]".into())),
            "{got:?}"
        );
        assert!(
            got.contains(&(
                "invalid_type".into(),
                "tracks[0].trim_silence.threshold_db".into()
            )),
            "{got:?}"
        );
        assert!(
            got.contains(&("unknown_field".into(), "tracks[0].trim_silence.pad".into())),
            "{got:?}"
        );
        assert!(
            got.contains(&("invalid_type".into(), "tracks[0].normalize".into())),
            "{got:?}"
        );
    }

    #[test]
    fn list_element_errors_keep_indices() {
        let d = run(json!({
            "version": "1.0",
            "tracks": [{"id": "bed", "type": "music", "source": "a.wav",
                        "ducking": {"trigger_tracks": ["v", 3, "w"], "reduction_db": "x"}}]
        }));
        let got = codes_paths(&d);
        assert!(
            got.contains(&(
                "invalid_type".into(),
                "tracks[0].ducking.trigger_tracks[1]".into()
            )),
            "{got:?}"
        );
        assert!(
            got.contains(&(
                "invalid_type".into(),
                "tracks[0].ducking.reduction_db".into()
            )),
            "{got:?}"
        );
        let ducking = d.timeline.tracks[0].ducking.as_ref().unwrap();
        assert_eq!(
            ducking.trigger_tracks.as_ref().unwrap(),
            &["v".to_string(), String::new(), "w".to_string()]
        );
    }

    #[test]
    fn misplaced_and_typed_hints() {
        let d = run(json!({
            "version": "1.0",
            "sample_rate": 48000,
            "tracks": [{"id": 7, "type": "speech", "mute": "yes", "sequence": [{"text": "a", "after_cue": "x"}]}]
        }));
        let hint = |p: &str| hint_at(&d, p);
        assert!(
            hint("sample_rate").contains("belongs in `output`"),
            "{}",
            hint("sample_rate")
        );
        assert!(
            hint("tracks[0].id").contains("Quote the value: `id: \"7\"`"),
            "{}",
            hint("tracks[0].id")
        );
        assert!(
            hint("tracks[0].mute").contains("YAML 1.1"),
            "{}",
            hint("tracks[0].mute")
        );
        assert_eq!(
            hint("tracks[0].sequence[0].after_cue"),
            "To start after `x` ends, write `start_relative: {after_cue: x}`"
        );
    }

    #[test]
    fn suppressed_and_annotation_keys() {
        let doc = json!({"version": "1.0", "x-meta": {"a": 1}, "providers": {"e": {"api_key": "zzz"}, "x_note": "n"},
                         "tracks": []});
        let Value::Object(map) = doc else {
            unreachable!()
        };
        let suppress: HashSet<String> = ["providers.e.api_key".to_string()].into();
        let d = decode(&map, &suppress);
        assert!(d.diagnostics.is_empty(), "{:?}", d.diagnostics);
        assert_eq!(d.timeline.providers.len(), 1);
    }

    #[test]
    fn annotations_are_not_copied() {
        let doc = json!({"version": "1.0", "x-big": [1, 2, 3], "output": {"format": "wav", "x_n": {"a": 1}},
            "tracks": [{"id": "v", "type": "speech", "x-c": 1, "settings": {"x-keep": 1},
                        "sequence": [{"text": "a", "x-t": 2}]}]});
        let copied = clone_dsl(&doc, Some(Kind::Obj(Obj::Timeline)));
        assert_eq!(
            copied,
            json!({"version": "1.0", "output": {"format": "wav"},
                   "tracks": [{"id": "v", "type": "speech", "settings": {"x-keep": 1},
                               "sequence": [{"text": "a"}]}]}),
            "free-form settings keep every key"
        );
    }

    #[test]
    fn mechanical_fixes_are_reported_and_applied() {
        let d = run(json!({
            "version": "1.0",
            "output": {"format": "MP3", "loudness_preset": "YouTube", "target_lufs": 16, "true_peak_db": 1,
                       "export_subtitles": ["VTT", "srt"]},
            "timing": {"speech_overlap": " push "},
            "tracks": [{"id": "bed", "type": "Music", "source": "a.wav", "fade_curve": "equal-power",
                        "fade_in_ms": "350MS", "ducking": {"trigger_tracks": ["v"], "mode": "SideChain"},
                        "normalize": {"target_lufs": 20}},
                       {"id": "v", "type": "speech", "sequence": [{"text": "a", "start_at_ms": "1:99.5"}]}]
        }));
        let got = codes_paths(&d);
        for path in [
            "output.format",
            "output.loudness_preset",
            "output.target_lufs",
            "output.true_peak_db",
            "output.export_subtitles[0]",
            "timing.speech_overlap",
            "tracks[0].type",
            "tracks[0].fade_curve",
            "tracks[0].fade_in_ms",
            "tracks[0].ducking.mode",
            "tracks[0].normalize.target_lufs",
            "tracks[1].sequence[0].start_at_ms",
        ] {
            assert!(
                got.contains(&("invalid_value".into(), path.into())),
                "{path}: {got:?}"
            );
        }
        assert_eq!(got.len(), 12, "{got:?}");
        assert_eq!(
            hint_at(&d, "output.format"),
            "Did you mean `mp3`? Write `format: mp3`"
        );
        assert_eq!(
            hint_at(&d, "tracks[0].type"),
            "Did you mean `music`? Write `type: music`"
        );
        assert_eq!(
            hint_at(&d, "output.export_subtitles[0]"),
            "Did you mean `vtt`? Write `vtt` in the `export_subtitles` list"
        );
        assert!(
            hint_at(&d, "output.loudness_preset").starts_with("Use `loudness_preset: streaming`")
        );
        assert!(
            hint_at(&d, "output.target_lufs")
                .starts_with("Loudness values are negative: use `target_lufs: -16`")
        );
        assert_eq!(
            hint_at(&d, "tracks[0].fade_in_ms"),
            "Write `fade_in_ms: \"350ms\"`"
        );
        assert_eq!(
            hint_at(&d, "tracks[1].sequence[0].start_at_ms"),
            "Write `start_at_ms: \"159.5s\"`"
        );
        // Later stages see the fixed values.
        let out = d.timeline.output.as_ref().unwrap();
        assert_eq!(out.format.as_deref(), Some("mp3"));
        assert_eq!(out.loudness_preset.as_deref(), Some("streaming"));
        assert_eq!(out.target_lufs, Some(-16.0));
        assert_eq!(out.true_peak_db, Some(-1.0));
        let bed = &d.timeline.tracks[0];
        assert_eq!(bed.kind.as_deref(), Some("music"));
        assert_eq!(bed.fade_curve.as_deref(), Some("equal_power"));
        assert!(d.dropped.is_empty(), "{:?}", d.dropped);
    }

    #[test]
    fn accepted_spellings_and_aliases_are_never_fixed() {
        let d = run(json!({
            "version": "1.0",
            "output": {"format": "aac", "loudness_preset": "ebu_r128", "export_subtitles": ["webvtt", "words"]},
            "providers": {"k": {"type": "kokoro_fastapi"}, "s": {"type": "say"}},
            "tracks": [{"id": "bed", "type": "sfx", "source": "a.wav", "fade_curve": "exp",
                        "ducking": {"trigger_tracks": ["v"], "mode": "timeline"}}]
        }));
        assert!(d.diagnostics.is_empty(), "{:?}", d.diagnostics);
        for preset in LoudnessPresetName::SPELLINGS {
            assert!(
                enum_fix(Obj::Output, "loudness_preset", preset, false).is_none(),
                "{preset}"
            );
        }
    }

    #[test]
    fn values_without_one_fix_are_left_to_lowering() {
        let d = run(json!({
            "version": "1.0",
            "output": {"format": "aiff", "loudness_preset": "loud", "target_lufs": 3},
            "tracks": [{"id": "bed", "type": "musik", "source": "a.wav", "fade_in_ms": "1:1e2"}]
        }));
        assert!(d.diagnostics.is_empty(), "{:?}", d.diagnostics);
        assert_eq!(
            d.timeline.output.as_ref().and_then(|o| o.format.as_deref()),
            Some("aiff")
        );
    }

    #[test]
    fn type_hints_convert_the_value() {
        let d = run(json!({
            "version": "1.0",
            "output": {"sample_rate": "44.1k", "bit_depth": "24-bit", "channels": "stereo"},
            "providers": {"c": {"type": "command", "timeout_ms": "30s", "command": "piper -m x"}},
            "tracks": [{"id": "bed", "type": "music", "source": "a.wav", "volume": "50%", "gain_db": "-6dB",
                        "pan": "-25%"},
                       {"id": "v", "type": "speech", "sequence": [{"text": "a", "volume": "2x"}]}]
        }));
        let hint = |p: &str| hint_at(&d, p);
        assert_eq!(
            hint("output.sample_rate"),
            "Write a whole number: `sample_rate: 44100` (the sample rate is a plain number of Hz)"
        );
        assert!(hint("output.bit_depth").contains("`bit_depth: 24`"));
        assert!(hint("output.channels").contains("`channels: 2`"));
        assert!(hint("providers.c.timeout_ms").contains("`timeout_ms: 30000`"));
        assert!(hint("providers.c.command").contains("`command: [piper, ...]`"));
        assert_eq!(
            hint("tracks[0].volume"),
            "`volume` is a linear factor: write `volume: 0.5` (50 % as a linear gain; 1.0 = unchanged)"
        );
        assert!(hint("tracks[0].gain_db").contains("`gain_db: -6`"));
        assert!(hint("tracks[0].pan").contains("`pan: -0.25`"));
        assert!(hint("tracks[1].sequence[0].volume").contains("`volume: 2`"));
        // A float sample rate in kHz.
        let d = run(json!({"version": "1.0", "output": {"sample_rate": 44.1}}));
        assert!(hint_at(&d, "output.sample_rate").contains("`sample_rate: 44100`"));
    }

    #[test]
    fn synonyms_and_units() {
        let d = run(json!({
            "version": "1.0",
            "provider": "mock",
            "timing": {"overlap": "push"},
            "speakers": {"host": {"name": "Host", "volume": 0.5, "speed": 1.1}},
            "providers": {"k": {"type": "kokoro", "url": "http://localhost:8880", "timeout": 30}},
            "tracks": [
                {"id": "bed", "type": "music", "file": "assets/bed.wav", "duration": 30, "start_after": "a2",
                 "duck_under": "v", "label": "Bed"},
                {"id": "v", "type": "speech", "voice_name": "nova", "sequence": [
                    {"id": "a2", "text": "hi", "wait": 2},
                    {"pause_ms": 300},
                    {"silence": 1.5},
                    {"text": "b", "start_with": "a2", "label": "x"},
                    {"text": "c", "start_relative": {"cue": "a2", "edge": "end"}}
                ]}
            ]
        }));
        let hint = |p: &str| hint_at(&d, p);
        assert_eq!(
            hint("provider"),
            "`provider` is not a top-level key: set it for every cue with `defaults: {provider: mock}`"
        );
        assert_eq!(
            hint("timing.overlap"),
            "Did you mean `speech_overlap`? Write `speech_overlap: push`"
        );
        assert_eq!(
            hint("speakers.host.name"),
            "Did you mean `label`? Write `label: Host`"
        );
        assert!(
            hint("speakers.host.volume").contains("about `gain_db: -6`"),
            "{}",
            hint("speakers.host.volume")
        );
        assert!(hint("speakers.host.speed").contains("`settings: {speed: 1.1}`"));
        assert_eq!(
            hint("providers.k.url"),
            "Did you mean `base_url`? Write `base_url: \"http://localhost:8880\"`"
        );
        assert_eq!(
            hint("providers.k.timeout"),
            "Did you mean `timeout_ms`? Times are milliseconds: write `timeout_ms: 30000` if you meant 30 seconds"
        );
        assert_eq!(
            hint("tracks[0].file"),
            "Did you mean `source`? Write `source: \"assets/bed.wav\"`"
        );
        assert_eq!(
            hint("tracks[0].duration"),
            "Did you mean `duration_ms`? Times are milliseconds: write `duration_ms: \"30s\"` (30000 ms) if you \
             meant seconds"
        );
        assert_eq!(
            hint("tracks[0].start_after"),
            "To start after `a2` ends, write `start_relative: {after_cue: a2}`"
        );
        assert_eq!(
            hint("tracks[0].duck_under"),
            "Did you mean `ducking`? Write `ducking: {trigger_tracks: [v]}`"
        );
        assert!(
            hint("tracks[0].label")
                .contains("a `speakers` entry (label is the subtitle name of a speaker)"),
            "{}",
            hint("tracks[0].label")
        );
        assert_eq!(
            hint("tracks[1].voice_name"),
            "Did you mean `voice_id`? Write `voice_id: nova`"
        );
        assert!(
            hint("tracks[1].sequence[0].wait").contains("its own item"),
            "{}",
            hint("tracks[1].sequence[0].wait")
        );
        let pause = hint("tracks[1].sequence[1].pause_ms");
        assert_eq!(
            pause,
            "Did you mean `pause`? Write `pause: 300`; a pause item has only `pause` (or `pause_duration`)."
        );
        let silence = hint("tracks[1].sequence[2].silence");
        assert!(
            silence.contains("`pause: 1.5`") && silence.contains("\"1.5s\""),
            "{silence}"
        );
        assert_eq!(
            hint("tracks[1].sequence[3].start_with"),
            "To start together with `a2`, write `start_relative: {with_cue: a2}`"
        );
        assert!(hint("tracks[1].sequence[3].label").contains("a `speakers` entry"));
        assert_eq!(
            hint("tracks[1].sequence[4].start_relative.cue"),
            "Did you mean `after_cue`? Write the anchor as `{after_cue: a2}`"
        );
        assert_eq!(
            hint("tracks[1].sequence[4].start_relative.edge"),
            "Did you mean `after_cue`? Write the anchor as `{after_cue: a2}`"
        );
    }

    #[test]
    fn times_written_as_ids_point_to_anchors() {
        let d = run(json!({"version": "1.0", "tracks": [
            {"id": "v", "type": "speech", "sequence": [{"id": "a2", "text": "x"}, {"pause": 300}, {"text": "y"}]},
            {"id": "hit", "type": "sfx", "source": "h.wav", "start_at_ms": "a2", "end_at_ms": "v.cue2"},
            {"id": "x", "type": "sfx", "source": "h.wav", "start_at_ms": "soon", "fade_in_ms": "a2"}
        ]}));
        assert_eq!(
            hint_at(&d, "tracks[1].start_at_ms"),
            "To start after `a2` ends, write `start_relative: {after_cue: a2}` instead of `start_at_ms` (or \
             `with_cue: a2` to start together with it)"
        );
        assert!(
            hint_at(&d, "tracks[1].end_at_ms").contains("`end_relative: {after_cue: v.cue2}`"),
            "the automatic id of the second cue (the pause is not counted)"
        );
        assert!(d.dropped.contains(&"tracks[1].start_at_ms".to_string()));
        assert!(d.timeline.tracks[1].start_at_ms.is_none());
        // Other strings, and ids in other time fields, are left to lowering.
        assert_eq!(d.diagnostics.len(), 2, "{:?}", codes_paths(&d));
    }

    #[test]
    fn anchor_target_hints_keep_the_offset() {
        let d = run(
            json!({"version": "1.0", "tracks": [{"id": "v", "type": "speech", "sequence": [
                {"id": "a1", "text": "x"},
                {"text": "y", "start_relative": {"cue": "a1", "edge": "end", "offset_ms": 100}}
            ]}]}),
        );
        assert_eq!(
            hint_at(&d, "tracks[0].sequence[1].start_relative.cue"),
            "Did you mean `after_cue`? Write the anchor as `{after_cue: a1, offset_ms: 100}`"
        );
    }

    #[test]
    fn several_local_candidates_are_listed() {
        let d = run(json!({"version": "1.0", "timing": {"tolerance_overlap": "30ms"}}));
        assert_eq!(
            hint_at(&d, "timing.tolerance_overlap"),
            "Did you mean `overlap_tolerance_ms`?"
        );
        let d = run(json!({"version": "1.0", "subtitles": {"duration": 5}}));
        assert_eq!(
            hint_at(&d, "subtitles.duration"),
            "Did you mean `max_duration_ms` or `min_duration_ms`?"
        );
    }

    #[test]
    fn rendering_values_for_hints() {
        assert_eq!(render_str("push").as_deref(), Some("push"));
        assert_eq!(render_str("voice.cue1").as_deref(), Some("voice.cue1"));
        assert_eq!(render_str("a b").as_deref(), Some("\"a b\""));
        assert_eq!(render_str("true").as_deref(), Some("\"true\""));
        assert_eq!(render_str("1.5s").as_deref(), Some("\"1.5s\""));
        assert_eq!(render_str("sk-proj-AbCdEfGhIjKlMnOpQrStUvWx"), None);
        assert_eq!(render_str(&"x".repeat(200)), None);
        assert_eq!(
            render_value(&json!(["vtt", "srt"])).as_deref(),
            Some("[vtt, srt]")
        );
        assert_eq!(render_value(&json!({"a": 1})), None);
        assert_eq!(
            seconds_as_ms(&Number::from_f64(2.5).unwrap()).as_deref(),
            Some("2500")
        );
        assert_eq!(seconds_as_ms(&Number::from(30)).as_deref(), Some("30000"));
        assert_eq!(or_list(&["a", "b", "c"]), "`a`, `b` or `c`");
        assert!(shares_words("overlap", "speech_overlap"));
        assert!(!shares_words("ov", "overlap"));
    }
}
