//! Tracks, sequence items, clips, anchors and ducking (DESIGN.md §2.9–§2.13, static parts of §3.3).

use indexmap::IndexMap;

use super::check::{self, Sign, TimeUse, fmt_ms};
use super::graph::{AnchorUse, Owner, Slot};
use super::voices::{self, Layer, VoiceOutcome};
use super::{IdEntry, IdKind, Lowerer};
use crate::diagnostics::{self, Diagnostic, codes};
use crate::fields;
use crate::ir::{
    AnchorMode, AnchorRule, AnchorTarget, ClipIr, ClipLength, CueIr, DuckingConfig, DuckingMode,
    FadeCurve, Node, NodeId, NodeKind, ProviderType, StartRule, TrackIdx, TrackIr, TrackKind,
    TrimConfig, VoiceIr,
};
use crate::model::{Anchor, Clip, DuckingSpec, SequenceItem, Track};
use crate::values::{Millis, NormalizeInput, TrimSilenceInput};

/// Default silence-trim threshold (dBFS) and padding (ms).
pub const DEFAULT_TRIM: TrimConfig = TrimConfig {
    threshold_db: -50.0,
    padding_ms: 30.0,
};

/// Ducking defaults (DESIGN.md §2.12).
const DUCK_REDUCTION_DB: f64 = 15.0;
const DUCK_ATTACK_MS: f64 = 80.0;
const DUCK_RELEASE_MS: f64 = 400.0;
const DUCK_HOLD_MS: f64 = 250.0;
const DUCK_THRESHOLD_DB: f64 = -40.0;

fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// What a track is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Speech,
    Audio,
}

/// Pass-A result for one track.
struct TrackPlan<'a> {
    raw: &'a Track,
    index: TrackIdx,
    path: String,
    id: String,
    /// Declared and valid type.
    kind: Option<TrackKind>,
    shape: Shape,
    items: Vec<ItemPlan<'a>>,
}

enum ItemSrc<'a> {
    Cue(&'a SequenceItem),
    Pause(&'a SequenceItem),
    Clip(&'a Clip),
    Shorthand,
}

struct ItemPlan<'a> {
    node: NodeId,
    src: ItemSrc<'a>,
    path: String,
    id: String,
    auto_id: bool,
}

/// Names of the fields present on a track.
fn present_track_fields(t: &Track) -> Vec<&'static str> {
    let present = [
        ("sequence", t.sequence.is_some()),
        ("speaker", t.speaker.is_some()),
        ("provider", t.provider.is_some()),
        ("voice_id", t.voice_id.is_some()),
        ("model", t.model.is_some()),
        ("language", t.language.is_some()),
        ("settings", t.settings.is_some()),
        ("trim_silence", t.trim_silence.is_some()),
        ("source", t.source.is_some()),
        ("loop", t.looped.is_some()),
        ("loop_crossfade_ms", t.loop_crossfade_ms.is_some()),
        ("fade_in_ms", t.fade_in_ms.is_some()),
        ("fade_out_ms", t.fade_out_ms.is_some()),
        ("fade_curve", t.fade_curve.is_some()),
        ("trim_start_ms", t.trim_start_ms.is_some()),
        ("duration_ms", t.duration_ms.is_some()),
        ("end_at_ms", t.end_at_ms.is_some()),
        ("end_relative", t.end_relative.is_some()),
        ("clips", t.clips.is_some()),
    ];
    present
        .into_iter()
        .filter(|(_, p)| *p)
        .map(|(n, _)| n)
        .collect()
}

/// The single-clip shorthand fields present on a track.
fn shorthand_fields(t: &Track) -> Vec<&'static str> {
    present_track_fields(t)
        .into_iter()
        .filter(|f| *f != "clips" && fields::TRACK_AUDIO.iter().any(|(n, _)| n == f))
        .collect()
}

/// Cue-only fields present on a sequence item.
fn cue_fields(item: &SequenceItem) -> Vec<&'static str> {
    let present = [
        ("id", item.id.is_some()),
        ("text", item.text.is_some()),
        ("caption", item.caption.is_some()),
        ("subtitles", item.subtitles.is_some()),
        ("speaker", item.speaker.is_some()),
        ("provider", item.provider.is_some()),
        ("voice_id", item.voice_id.is_some()),
        ("model", item.model.is_some()),
        ("language", item.language.is_some()),
        ("settings", item.settings.is_some()),
        ("start_at_ms", item.start_at_ms.is_some()),
        ("start_relative", item.start_relative.is_some()),
        ("volume", item.volume.is_some()),
        ("gain_db", item.gain_db.is_some()),
        ("allow_overlap", item.allow_overlap.is_some()),
    ];
    present
        .into_iter()
        .filter(|(_, p)| *p)
        .map(|(n, _)| n)
        .collect()
}

/// Hint for a field that does not apply to a track type.
fn not_applicable_hint(field: &str, kind: TrackKind) -> &'static str {
    match (kind, field) {
        (
            TrackKind::Speech,
            "source" | "clips" | "loop" | "loop_crossfade_ms" | "trim_start_ms",
        ) => {
            "Speech tracks are synthesized from `sequence`; to play an audio file, put it on a separate `type: music` \
             or `type: sfx` track"
        }
        (TrackKind::Speech, "fade_in_ms" | "fade_out_ms" | "fade_curve") => {
            "Fades apply to music/sfx clips; remove it (speech cues are trimmed with `trim_silence` instead)"
        }
        (TrackKind::Speech, _) => {
            "A speech track lasts as long as its cues; remove it (use `output.duration_ms` to cut the whole mix)"
        }
        (_, "sequence") => {
            "Only speech tracks have a `sequence`; use `type: speech` for TTS cues, or `source` / `clips` for audio \
             files"
        }
        (_, "trim_silence") => {
            "Silence trimming applies to synthesized speech; remove it (use `trim_start_ms` to skip the start of an \
             audio file)"
        }
        _ => {
            "Voice fields only apply to speech tracks; move it to the speech track (or the cues) that should use \
              this voice"
        }
    }
}

/// A cue that belongs to a grouped voice diagnostic.
struct GroupedCue {
    id: String,
    path: String,
    /// The provider default voice that was used (`default_voice` only).
    default_voice: Option<&'static str>,
}

/// Grouped voice diagnostics of one track: (code, provider) → cues.
#[derive(Default)]
struct VoiceGroups {
    groups: IndexMap<(&'static str, String), Vec<GroupedCue>>,
}

impl VoiceGroups {
    fn add(
        &mut self,
        code: &'static str,
        provider: &str,
        cue_id: &str,
        path: &str,
        default: Option<&'static str>,
    ) {
        self.groups
            .entry((code, provider.to_string()))
            .or_default()
            .push(GroupedCue {
                id: cue_id.to_string(),
                path: path.to_string(),
                default_voice: default,
            });
    }

    fn flush(self, diags: &mut Vec<Diagnostic>, track_id: &str, track_path: &str) {
        for ((code, provider), cues) in self.groups {
            let n = cues.len();
            let ids: Vec<&str> = cues.iter().map(|c| c.id.as_str()).collect();
            let subject = if n == 1 {
                format!("cue `{}`", ids[0])
            } else {
                format!(
                    "{n} cues of track `{track_id}` ({})",
                    diagnostics::list_names(ids.iter().copied(), 5)
                )
            };
            let (verb_has, verb_uses) = if n == 1 {
                ("has", "uses")
            } else {
                ("have", "use")
            };
            let path = if n == 1 {
                cues[0].path.clone()
            } else {
                track_path.to_string()
            };
            let d = match code {
                codes::MISSING_PROVIDER => Diagnostic::error(code, format!("{subject} {verb_has} no TTS provider"))
                    .with_hint(format!(
                        "Set `provider:` on the cue, on its track, on its speaker, or once for every cue in `defaults: \
                         {{provider: ...}}`; built-in providers: {} (`mock` works offline)",
                        diagnostics::list_names(ProviderType::NAMES.iter().copied(), 8)
                    )),
                codes::MISSING_VOICE => Diagnostic::error(
                    code,
                    format!("{subject} {verb_has} no `voice_id`, and provider `{provider}` has no default voice"),
                )
                .with_hint(format!(
                    "Set `voice_id:` on the cue, its track, its speaker or in `defaults`; run `soniccraft voices \
                     {provider}` to list the available voices"
                )),
                _ => {
                    let default = cues[0].default_voice.unwrap_or("default");
                    Diagnostic::info(
                        code,
                        format!("{subject} {verb_uses} the default voice `{default}` of provider `{provider}`"),
                    )
                    .with_hint("Set `voice_id:` to choose a voice explicitly")
                }
            };
            diags.push(
                d.with_path(path)
                    .with_related(ids.iter().map(|s| s.to_string())),
            );
        }
    }
}

impl<'a> Lowerer<'a> {
    /// Passes A and B over all tracks.
    pub(super) fn lower_tracks(&mut self) {
        let timeline = self.timeline;
        if timeline.tracks.is_empty() {
            self.diags.push(
                Diagnostic::error(codes::EMPTY_TRACKS, "the timeline has no tracks (`tracks` is missing or empty)")
                    .with_path("tracks")
                    .with_hint(
                        "Add at least one track, e.g. `tracks: [{id: narration, type: speech, sequence: [{text: \
                         \"Hello\", provider: mock}]}]`",
                    ),
            );
            return;
        }
        let mut next_node = 0;
        let plans: Vec<TrackPlan<'_>> = timeline
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| self.plan_track(i, t, &mut next_node))
            .collect();
        for plan in &plans {
            self.build_track(plan);
        }
    }

    // ---- pass A ------------------------------------------------------------------------------------

    fn plan_track<'t>(
        &mut self,
        index: TrackIdx,
        raw: &'t Track,
        next_node: &mut NodeId,
    ) -> TrackPlan<'t> {
        let path = format!("tracks[{index}]");
        let kind = match raw.kind.as_deref() {
            None => {
                self.diags.push(
                    Diagnostic::error(codes::MISSING_FIELD, format!("track `{}` has no `type`", raw.id.as_deref().unwrap_or(&path)))
                        .with_path(format!("{path}.type"))
                        .with_hint(
                            "Add `type: speech` (TTS cues in `sequence`), `type: music` or `type: sfx` (audio files \
                             in `source` / `clips`)",
                        ),
                );
                None
            }
            Some(k) => check::enum_name(
                &mut self.diags,
                Some(k),
                &format!("{path}.type"),
                TrackKind::parse,
                TrackKind::NAMES,
                "track type",
            ),
        };
        let shape = match kind {
            Some(TrackKind::Speech) => Shape::Speech,
            Some(_) => Shape::Audio,
            None if raw.sequence.is_some() => Shape::Speech,
            None => Shape::Audio,
        };

        // Track id. Ids are never trimmed silently (`invalid_id` below), but the trimmed id is registered so
        // references to it do not cascade into `unknown_reference`.
        let id = match raw.id.as_deref().map(|id| (id, id.trim())) {
            None => {
                let example = match shape {
                    Shape::Speech => "narration",
                    Shape::Audio if kind == Some(TrackKind::Sfx) => "sfx",
                    Shape::Audio => "music_bed",
                };
                self.diags.push(
                    Diagnostic::error(
                        codes::MISSING_FIELD,
                        format!("track {} has no `id`", index + 1),
                    )
                    .with_path(format!("{path}.id"))
                    .with_hint(format!("Add a unique `id:`, e.g. `id: {example}`")),
                );
                format!("track{}", index + 1)
            }
            Some((_, "")) => {
                self.diags.push(
                    Diagnostic::error(codes::INVALID_ID, "track id is empty")
                        .with_path(format!("{path}.id"))
                        .with_hint(
                            "Use a name made of letters, digits and `_`, e.g. `id: narration`",
                        ),
                );
                format!("track{}", index + 1)
            }
            Some((written, id)) => {
                self.check_id(written, &format!("{path}.id"), "track");
                self.register(
                    id,
                    IdEntry {
                        target: AnchorTarget::Track(index),
                        kind: IdKind::Track,
                        track: index,
                        path: path.clone(),
                    },
                    &format!("{path}.id"),
                    false,
                );
                id.to_string()
            }
        };
        let auto_prefix_ok = raw.id.as_deref().is_some_and(|s| !s.trim().is_empty());

        // Items.
        let mut items = Vec::new();
        let mut take_node = || {
            let n = *next_node;
            *next_node += 1;
            n
        };
        match shape {
            Shape::Speech => {
                let (mut cues, mut pauses) = (0usize, 0usize);
                for (j, item) in raw.sequence.iter().flatten().enumerate() {
                    let item_path = format!("{path}.sequence[{j}]");
                    // A misspelled pause (`{pause_ms: 300}`, reported by the parser) is a pause too: lowering
                    // it as a cue would add "no `text`" and voice errors whose hints turn it into speech.
                    let is_pause = ((item.pause_duration.is_some() || item.pause.is_some())
                        && item.text.is_none())
                        || self.is_misspelled_pause(&item_path);
                    let node = take_node();
                    if is_pause {
                        pauses += 1;
                        let id = format!("{id}.pause{pauses}");
                        if auto_prefix_ok {
                            self.pause_ids.insert(id.clone(), item_path.clone());
                        }
                        items.push(ItemPlan {
                            node,
                            src: ItemSrc::Pause(item),
                            path: item_path,
                            id,
                            auto_id: true,
                        });
                        continue;
                    }
                    cues += 1;
                    let (cue_id, auto) = match item.id.as_deref().map(|id| (id, id.trim())) {
                        Some((written, explicit)) if !explicit.is_empty() => {
                            self.check_id(written, &format!("{item_path}.id"), "cue");
                            self.register(
                                explicit,
                                IdEntry {
                                    target: AnchorTarget::Node(node),
                                    kind: IdKind::Cue,
                                    track: index,
                                    path: item_path.clone(),
                                },
                                &format!("{item_path}.id"),
                                false,
                            );
                            (explicit.to_string(), false)
                        }
                        explicit => {
                            if explicit.is_some() {
                                self.diags.push(
                                    Diagnostic::error(codes::INVALID_ID, "cue id is empty")
                                        .with_path(format!("{item_path}.id"))
                                        .with_hint("Remove `id` to use the automatic id, or set a name like `id: intro`"),
                                );
                            }
                            let auto_id = format!("{id}.cue{cues}");
                            if auto_prefix_ok {
                                self.register(
                                    &auto_id,
                                    IdEntry {
                                        target: AnchorTarget::Node(node),
                                        kind: IdKind::Cue,
                                        track: index,
                                        path: item_path.clone(),
                                    },
                                    &item_path,
                                    true,
                                );
                            }
                            (auto_id, true)
                        }
                    };
                    items.push(ItemPlan {
                        node,
                        src: ItemSrc::Cue(item),
                        path: item_path,
                        id: cue_id,
                        auto_id: auto,
                    });
                }
            }
            Shape::Audio => {
                if let Some(clips) = &raw.clips {
                    for (j, clip) in clips.iter().enumerate() {
                        let item_path = format!("{path}.clips[{j}]");
                        let node = take_node();
                        let (clip_id, auto) = match clip.id.as_deref().map(|id| (id, id.trim())) {
                            Some((written, explicit)) if !explicit.is_empty() => {
                                self.check_id(written, &format!("{item_path}.id"), "clip");
                                self.register(
                                    explicit,
                                    IdEntry {
                                        target: AnchorTarget::Node(node),
                                        kind: IdKind::Clip,
                                        track: index,
                                        path: item_path.clone(),
                                    },
                                    &format!("{item_path}.id"),
                                    false,
                                );
                                (explicit.to_string(), false)
                            }
                            explicit => {
                                if explicit.is_some() {
                                    self.diags.push(
                                        Diagnostic::error(codes::INVALID_ID, "clip id is empty")
                                            .with_path(format!("{item_path}.id"))
                                            .with_hint("Remove `id` to use the automatic id, or set a name like `id: intro_music`"),
                                    );
                                }
                                let auto_id = format!("{id}.clip{}", j + 1);
                                if auto_prefix_ok {
                                    self.register(
                                        &auto_id,
                                        IdEntry {
                                            target: AnchorTarget::Node(node),
                                            kind: IdKind::Clip,
                                            track: index,
                                            path: item_path.clone(),
                                        },
                                        &item_path,
                                        true,
                                    );
                                }
                                (auto_id, true)
                            }
                        };
                        items.push(ItemPlan {
                            node,
                            src: ItemSrc::Clip(clip),
                            path: item_path,
                            id: clip_id,
                            auto_id: auto,
                        });
                    }
                } else if !shorthand_fields(raw).is_empty() {
                    // The single-clip shorthand: the clip is anchored by the track id itself.
                    let node = take_node();
                    items.push(ItemPlan {
                        node,
                        src: ItemSrc::Shorthand,
                        path: path.clone(),
                        id: id.clone(),
                        auto_id: false,
                    });
                }
            }
        }
        TrackPlan {
            raw,
            index,
            path,
            id,
            kind,
            shape,
            items,
        }
    }

    /// `invalid_id` for ids (as written) that do not match the id pattern; surrounding whitespace gets its own
    /// message with the trimmed id as the fix.
    fn check_id(&mut self, id: &str, path: &str, what: &str) {
        let trimmed = id.trim();
        if trimmed != id && check::is_valid_id(trimmed) {
            self.diags.push(
                Diagnostic::error(
                    codes::INVALID_ID,
                    format!("{what} id {id:?} has whitespace around it"),
                )
                .with_path(path)
                .with_hint(format!(
                    "Ids contain no whitespace; remove it: `id: {trimmed}`"
                )),
            );
        } else if !check::is_valid_id(id) {
            self.diags.push(
                Diagnostic::error(codes::INVALID_ID, format!("{what} id `{id}` contains characters that are not allowed"))
                    .with_path(path)
                    .with_hint(format!(
                        "Ids use letters, digits, `_`, `.`, `:` and `-` and must not start with `.`, `:` or `-`; e.g. \
                         `id: {}`",
                        check::sanitize_id(id)
                    )),
            );
        }
    }

    // ---- pass B ------------------------------------------------------------------------------------

    fn build_track(&mut self, plan: &TrackPlan<'_>) {
        let raw = plan.raw;
        let path = plan.path.as_str();
        let i = plan.index;

        // Field applicability.
        if let Some(kind) = plan.kind {
            let wrong: &[(&str, fields::Kind)] = if kind == TrackKind::Speech {
                fields::TRACK_AUDIO
            } else {
                fields::TRACK_SPEECH
            };
            for field in present_track_fields(raw) {
                if wrong.iter().any(|(n, _)| *n == field) {
                    self.diags.push(
                        Diagnostic::error(
                            codes::FIELD_NOT_APPLICABLE,
                            format!("`{field}` does not apply to {kind} track `{}`", plan.id),
                        )
                        .with_path(format!("{path}.{field}"))
                        .with_hint(not_applicable_hint(field, kind)),
                    );
                }
            }
        }

        // Common fields.
        let volume =
            check::volume(&mut self.diags, raw.volume, &format!("{path}.volume")).unwrap_or(1.0);
        let gain_db =
            check::gain_db(&mut self.diags, raw.gain_db, &format!("{path}.gain_db")).unwrap_or(0.0);
        let pan = check::range(
            &mut self.diags,
            raw.pan,
            &format!("{path}.pan"),
            -1.0,
            1.0,
            "Use a pan position from -1 (hard left) through 0 (center) to 1 (hard right)",
        )
        .unwrap_or(0.0);
        let normalize_lufs = self.normalize(raw.normalize.as_ref(), plan.kind, path);
        let start = self.start_rule(
            raw.start_at_ms.as_ref(),
            raw.start_relative.as_ref(),
            path,
            Owner::Track(i),
        );
        let trim_silence = match plan.shape {
            Shape::Speech => self.trim_silence(raw.trim_silence.as_ref(), path),
            Shape::Audio => None,
        };
        let ducking = raw
            .ducking
            .as_ref()
            .and_then(|d| self.ducking(d, path, i, &plan.id));

        // Speech-track voice fields. A misspelled track speaker is replaced by the speaker it is a misspelling
        // of when the cues' voices are resolved (see `build_cue`).
        let mut track_speaker_fix = None;
        if plan.shape == Shape::Speech {
            voices::check_empty_voice_fields(
                &mut self.diags,
                path,
                [
                    ("provider", raw.provider.as_ref()),
                    ("voice_id", raw.voice_id.as_ref()),
                    ("model", raw.model.as_ref()),
                    ("language", raw.language.as_ref()),
                ],
            );
            if let Some(p) = raw.provider.as_deref().filter(|p| !p.trim().is_empty()) {
                self.providers
                    .check_ref(&mut self.diags, p, &format!("{path}.provider"));
            }
            if let Some(s) = raw.speaker.as_deref() {
                track_speaker_fix = self.check_speaker(s, &format!("{path}.speaker"));
            }
            voices::check_settings_keys(
                &mut self.diags,
                raw.settings.as_ref(),
                path,
                voices::SettingsOwner::CueOrTrack,
                voices::VoiceFields::of_track(raw),
            );
        }

        // Content requirements.
        match (plan.shape, plan.kind) {
            (Shape::Speech, _) => match &raw.sequence {
                None => self.diags.push(
                    Diagnostic::error(
                        codes::MISSING_FIELD,
                        format!("speech track `{}` has no `sequence`", plan.id),
                    )
                    .with_path(format!("{path}.sequence"))
                    .with_hint("Add the cues to speak: `sequence: [{text: \"Hello!\"}]`"),
                ),
                Some(seq) if seq.is_empty() => self.diags.push(
                    Diagnostic::error(
                        codes::EMPTY_SEQUENCE,
                        format!("speech track `{}` has an empty `sequence`", plan.id),
                    )
                    .with_path(format!("{path}.sequence"))
                    .with_hint("Add at least one cue (`- text: \"...\"`), or remove the track"),
                ),
                Some(_)
                    if !plan
                        .items
                        .iter()
                        .any(|it| matches!(it.src, ItemSrc::Cue(_))) =>
                {
                    self.diags.push(
                        Diagnostic::warning(
                            codes::EMPTY_SEQUENCE,
                            format!("speech track `{}` contains only pauses", plan.id),
                        )
                        .with_path(format!("{path}.sequence"))
                        .with_hint("Add a cue with `text`, or remove the track"),
                    )
                }
                Some(_) => {}
            },
            (Shape::Audio, kind) => {
                let shorthand = shorthand_fields(raw);
                match &raw.clips {
                    Some(clips) if clips.is_empty() => self.diags.push(
                        Diagnostic::error(codes::EMPTY_SEQUENCE, format!("track `{}` has an empty `clips` list", plan.id))
                            .with_path(format!("{path}.clips"))
                            .with_hint("Add at least one clip (`- source: audio/bed.wav`), or use `source:` directly on the track"),
                    ),
                    Some(_) if !shorthand.is_empty() => {
                        let listed = diagnostics::list_names(shorthand.iter().copied(), 12);
                        self.diags.push(
                            Diagnostic::error(
                                codes::CONFLICTING_FIELDS,
                                format!("track `{}` has both a `clips` list and single-clip fields ({listed})", plan.id),
                            )
                            .with_path(format!("{path}.{}", shorthand[0]))
                            .with_hint(format!(
                                "Move {listed} into each entry of `clips`, or remove `clips` to use the single-clip \
                                 shorthand"
                            )),
                        );
                    }
                    Some(_) => {}
                    None if raw.source.is_none() && kind.is_some() => {
                        let k = kind.map(|k| k.as_str()).unwrap_or("audio");
                        self.diags.push(
                            Diagnostic::error(codes::MISSING_FIELD, format!("{k} track `{}` has no `source`", plan.id))
                                .with_path(format!("{path}.source"))
                                .with_hint(
                                    "Add `source: path/to/audio.wav` (relative to the timeline file), or a `clips:` \
                                     list for several clips",
                                ),
                        );
                    }
                    None => {}
                }
            }
        }

        // Items.
        let mut groups = VoiceGroups::default();
        let mut node_ids = Vec::with_capacity(plan.items.len());
        let mut first_clip = true;
        for (index_in_track, item) in plan.items.iter().enumerate() {
            debug_assert_eq!(self.nodes.len(), item.node);
            let node = match item.src {
                ItemSrc::Cue(cue) => self.build_cue(
                    plan,
                    item,
                    cue,
                    index_in_track,
                    track_speaker_fix,
                    &mut groups,
                ),
                ItemSrc::Pause(pause) => self.build_pause(plan, item, pause, index_in_track),
                ItemSrc::Clip(clip) => {
                    let n = self.build_clip(plan, item, clip, index_in_track, first_clip);
                    first_clip = false;
                    n
                }
                ItemSrc::Shorthand => self.build_shorthand(plan, item, &start),
            };
            self.nodes.push(node);
            node_ids.push(item.node);
        }
        groups.flush(&mut self.diags, &plan.id, path);

        let kind = plan.kind.unwrap_or(match plan.shape {
            Shape::Speech => TrackKind::Speech,
            Shape::Audio => TrackKind::Music,
        });
        self.tracks.push(TrackIr {
            id: plan.id.clone(),
            kind,
            path: plan.path.clone(),
            gain: volume * db_to_linear(gain_db),
            pan,
            mute: raw.mute.unwrap_or(false),
            normalize_lufs,
            trim_silence,
            ducking,
            start,
            nodes: node_ids,
        });
    }

    fn normalize(
        &mut self,
        input: Option<&NormalizeInput>,
        kind: Option<TrackKind>,
        path: &str,
    ) -> Option<f64> {
        let reference = self.reference_lufs;
        match input {
            None => (kind != Some(TrackKind::Sfx)).then_some(reference),
            Some(NormalizeInput::Flag(true)) => Some(reference),
            Some(NormalizeInput::Flag(false)) => None,
            Some(NormalizeInput::Config(spec)) => Some(
                check::lufs(
                    &mut self.diags,
                    spec.target_lufs,
                    &format!("{path}.normalize.target_lufs"),
                )
                .unwrap_or(reference),
            ),
        }
    }

    fn trim_silence(&mut self, input: Option<&TrimSilenceInput>, path: &str) -> Option<TrimConfig> {
        match input {
            None | Some(TrimSilenceInput::Flag(true)) => Some(DEFAULT_TRIM),
            Some(TrimSilenceInput::Flag(false)) => None,
            Some(TrimSilenceInput::Config(spec)) => Some(TrimConfig {
                threshold_db: check::range(
                    &mut self.diags,
                    spec.threshold_db,
                    &format!("{path}.trim_silence.threshold_db"),
                    -100.0,
                    0.0,
                    "Use a level between -100 and 0 dBFS (default -50)",
                )
                .unwrap_or(DEFAULT_TRIM.threshold_db),
                padding_ms: check::millis(
                    &mut self.diags,
                    spec.padding_ms.as_ref(),
                    &format!("{path}.trim_silence.padding_ms"),
                    Sign::NonNegative,
                )
                .unwrap_or(DEFAULT_TRIM.padding_ms),
            }),
        }
    }

    fn ducking(
        &mut self,
        spec: &DuckingSpec,
        path: &str,
        track: TrackIdx,
        track_id: &str,
    ) -> Option<DuckingConfig> {
        let dpath = format!("{path}.ducking");
        let track_ids: Vec<&str> = self
            .ids
            .iter()
            .filter(|(_, e)| e.kind == IdKind::Track)
            .map(|(id, _)| id.as_str())
            .collect();
        let mut triggers = Vec::new();
        match &spec.trigger_tracks {
            None => self.diags.push(
                Diagnostic::error(codes::MISSING_FIELD, format!("`ducking` of track `{track_id}` has no `trigger_tracks`"))
                    .with_path(format!("{dpath}.trigger_tracks"))
                    .with_hint("List the tracks whose activity ducks this one, e.g. `trigger_tracks: [narration]`"),
            ),
            Some(list) if list.is_empty() => self.diags.push(
                Diagnostic::error(codes::INVALID_VALUE, "`trigger_tracks` is empty")
                    .with_path(format!("{dpath}.trigger_tracks"))
                    .with_hint("List at least one track id, e.g. `trigger_tracks: [narration]`, or remove `ducking`"),
            ),
            Some(list) => {
                for (k, raw_id) in list.iter().enumerate() {
                    let item_path = format!("{dpath}.trigger_tracks[{k}]");
                    let id = raw_id.trim();
                    match self.ids.get(id) {
                        Some(entry) if entry.kind == IdKind::Track => {
                            if entry.target == AnchorTarget::Track(track) {
                                self.diags.push(
                                    Diagnostic::error(codes::SELF_REFERENCE, format!("track `{track_id}` cannot duck itself"))
                                        .with_path(item_path)
                                        .with_hint(format!(
                                            "Remove `{id}` from `trigger_tracks`; list the tracks (usually speech) that \
                                             should duck it"
                                        ))
                                        .with_related([id.to_string()]),
                                );
                            } else if let AnchorTarget::Track(t) = entry.target
                                && !triggers.contains(&t)
                            {
                                triggers.push(t);
                            }
                        }
                        Some(entry) => {
                            let owner_track = self
                                .timeline
                                .tracks
                                .get(entry.track)
                                .and_then(|t| t.id.as_deref())
                                .map(|s| s.trim().to_string());
                            self.diags.push(
                                Diagnostic::error(
                                    codes::UNKNOWN_REFERENCE,
                                    format!("`{id}` is a {}, not a track", entry.kind.as_str()),
                                )
                                .with_path(item_path)
                                .with_hint(match owner_track {
                                    Some(t) => format!("`trigger_tracks` lists track ids; use its track `{t}`"),
                                    None => "`trigger_tracks` lists track ids".to_string(),
                                })
                                .with_related([id.to_string()]),
                            );
                        }
                        None => {
                            let suggestion = check::suggest(id, track_ids.iter().copied());
                            let known = diagnostics::list_names(track_ids.iter().copied(), 12);
                            let hint = match suggestion {
                                Some(s) => format!("Did you mean `{s}`? Track ids: {known}"),
                                None => format!("Track ids: {known}"),
                            };
                            let mut d = Diagnostic::error(
                                codes::UNKNOWN_REFERENCE,
                                format!("`trigger_tracks[{k}]` refers to `{id}`, which is not a track id"),
                            )
                            .with_path(item_path)
                            .with_hint(hint);
                            if let Some(s) = suggestion {
                                d = d.with_related([s.to_string()]);
                            }
                            self.diags.push(d);
                        }
                    }
                }
            }
        }

        let reduction_db = match spec.reduction_db {
            None => DUCK_REDUCTION_DB,
            Some(r) if r.is_finite() && r.abs() <= 60.0 => r.abs(),
            Some(r) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::INVALID_VALUE,
                        format!("`reduction_db` must be between -60 and 60 dB (the sign is ignored), got {}", check::fmt_num(r)),
                    )
                    .with_path(format!("{dpath}.reduction_db"))
                    .with_hint("Typical ducking depths are 10–20 dB, e.g. `reduction_db: -15`"),
                );
                DUCK_REDUCTION_DB
            }
        };
        let ms = |s: &mut Self, v: Option<&Millis>, field: &str, default: f64| {
            check::millis(
                &mut s.diags,
                v,
                &format!("{dpath}.{field}"),
                Sign::NonNegative,
            )
            .unwrap_or(default)
        };
        let attack_ms = ms(self, spec.attack_ms.as_ref(), "attack_ms", DUCK_ATTACK_MS);
        let release_ms = ms(
            self,
            spec.release_ms.as_ref(),
            "release_ms",
            DUCK_RELEASE_MS,
        );
        let hold_ms = ms(self, spec.hold_ms.as_ref(), "hold_ms", DUCK_HOLD_MS);
        let lookahead_ms = ms(self, spec.lookahead_ms.as_ref(), "lookahead_ms", attack_ms);
        let threshold_db = check::range(
            &mut self.diags,
            spec.threshold_db,
            &format!("{dpath}.threshold_db"),
            -100.0,
            0.0,
            "Use an RMS threshold between -100 and 0 dBFS (default -40)",
        )
        .unwrap_or(DUCK_THRESHOLD_DB);
        let mode = check::enum_name(
            &mut self.diags,
            spec.mode.as_deref(),
            &format!("{dpath}.mode"),
            DuckingMode::parse,
            DuckingMode::NAMES,
            "ducking mode",
        )
        .unwrap_or(DuckingMode::Sidechain);
        Some(DuckingConfig {
            triggers,
            reduction_db,
            attack_ms,
            release_ms,
            hold_ms,
            lookahead_ms,
            threshold_db,
            mode,
        })
    }

    /// `unknown_speaker` for a speaker that is not in a non-empty `speakers` map: an error when it is a
    /// misspelling of a defined speaker (the cue would silently get another voice and label), else a warning
    /// (an unregistered speaker is just a subtitle label). Returns the defined speaker it is a misspelling
    /// of, for the error case.
    fn check_speaker(&mut self, speaker: &str, path: &str) -> Option<&'a str> {
        let timeline = self.timeline;
        let speakers = &timeline.speakers;
        if speaker.trim().is_empty() {
            self.diags.push(
                Diagnostic::error(codes::INVALID_VALUE, "`speaker` is empty")
                    .with_path(path)
                    .with_hint("Set a speaker name or remove `speaker`"),
            );
            return None;
        }
        let speaker = speaker.trim();
        if speakers.is_empty() || speakers.contains_key(speaker) {
            return None;
        }
        let names: Vec<&str> = speakers.keys().map(String::as_str).collect();
        let defined = diagnostics::list_names(names.iter().copied(), 12);
        let suggestion = check::suggest(speaker, names.iter().copied());
        let message = format!("speaker `{speaker}` is not defined in `speakers`");
        let d = match suggestion {
            Some(s) => Diagnostic::error(codes::UNKNOWN_SPEAKER, message)
                .with_hint(format!(
                    "Did you mean `{s}`? Use `speaker: {s}` (defined speakers: {defined}), or add `{speaker}` under \
                     `speakers:` if it is a different speaker"
                ))
                .with_related([s.to_string()]),
            None => Diagnostic::warning(codes::UNKNOWN_SPEAKER, message).with_hint(format!(
                "Add `{speaker}` under `speakers:` or use one of {defined}; until then it is only a subtitle label"
            )),
        };
        self.diags.push(d.with_path(path));
        suggestion
    }

    // ---- items -------------------------------------------------------------------------------------

    fn build_cue(
        &mut self,
        plan: &TrackPlan<'_>,
        item: &ItemPlan<'_>,
        cue: &SequenceItem,
        index_in_track: usize,
        track_speaker_fix: Option<&'a str>,
        groups: &mut VoiceGroups,
    ) -> Node {
        let path = item.path.as_str();
        let label = format!("cue `{}`", item.id);
        for field in fields::PAUSE_KEYS {
            let present = if *field == "pause" {
                cue.pause.is_some()
            } else {
                cue.pause_duration.is_some()
            };
            if present {
                self.diags.push(
                    Diagnostic::error(
                        codes::CONFLICTING_FIELDS,
                        format!("{label} mixes pause and cue fields: `{field}` cannot be combined with `text`"),
                    )
                    .with_path(format!("{path}.{field}"))
                    .with_hint(format!(
                        "A sequence item is either a pause or a cue; split it into two items: `- {field}: ...` followed \
                         by `- text: \"...\"`"
                    )),
                );
            }
        }
        let text = match cue.text.as_deref() {
            None => {
                self.diags.push(
                    Diagnostic::error(codes::MISSING_FIELD, format!("{label} has no `text`"))
                        .with_path(format!("{path}.text"))
                        .with_hint("Add the words to speak: `text: \"...\"` (or make it a pause with only `pause_duration`)"),
                );
                String::new()
            }
            Some(t) if t.trim().is_empty() => {
                self.diags.push(
                    Diagnostic::error(codes::EMPTY_TEXT, format!("{label} has empty `text`"))
                        .with_path(format!("{path}.text"))
                        .with_hint("Write the words to speak, or remove the cue"),
                );
                String::new()
            }
            Some(t) => t.trim().to_string(),
        };
        let caption = match cue.caption.as_deref() {
            Some(c) if c.trim().is_empty() => {
                self.diags.push(
                    Diagnostic::error(codes::EMPTY_TEXT, format!("{label} has an empty `caption`"))
                        .with_path(format!("{path}.caption"))
                        .with_hint("Remove `caption` to show `text`, or use `subtitles: false` to leave the cue out of subtitles"),
                );
                None
            }
            Some(c) => Some(c.trim().to_string()),
            None => None,
        };

        // Voice.
        voices::check_empty_voice_fields(
            &mut self.diags,
            path,
            [
                ("provider", cue.provider.as_ref()),
                ("voice_id", cue.voice_id.as_ref()),
                ("model", cue.model.as_ref()),
                ("language", cue.language.as_ref()),
            ],
        );
        if let Some(p) = cue.provider.as_deref().filter(|p| !p.trim().is_empty()) {
            self.providers
                .check_ref(&mut self.diags, p, &format!("{path}.provider"));
        }
        let cue_speaker_fix = cue
            .speaker
            .as_deref()
            .and_then(|s| self.check_speaker(s, &format!("{path}.speaker")));
        // A misspelled speaker (an `unknown_speaker` error) is resolved as the speaker its did-you-mean names,
        // so the voice diagnostics below are the ones that remain once the hint is applied, not consequences
        // of the typo.
        if cue_speaker_fix.is_some() || track_speaker_fix.is_some() {
            self.speaker_errors.insert(item.node);
        }
        voices::check_settings_keys(
            &mut self.diags,
            cue.settings.as_ref(),
            path,
            voices::SettingsOwner::CueOrTrack,
            voices::VoiceFields::of_cue(cue),
        );
        let speakers = &self.timeline.speakers;
        let track_speaker = plan
            .raw
            .speaker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let cue_speaker = cue
            .speaker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let mut layers = Vec::with_capacity(5);
        if let Some(d) = &self.timeline.defaults {
            layers.push(Layer::from_voice_spec(d, "defaults"));
        }
        if let Some(s) = track_speaker_fix.or(track_speaker)
            && let Some(spec) = speakers.get(s)
        {
            layers.push(Layer::from_voice_spec(spec, &format!("speakers.{s}")));
        }
        let track_layer = layers.len();
        layers.push(Layer::from_track(plan.raw, &plan.path));
        if let Some(s) = cue_speaker_fix.or(cue_speaker)
            && let Some(spec) = speakers.get(s)
        {
            layers.push(Layer::from_voice_spec(spec, &format!("speakers.{s}")));
        }
        layers.push(Layer::from_cue(cue, path));
        // Where the effective `voice_id` is written (hints of `speakers_share_voice`).
        let voice_layer = layers.iter().rposition(|l| l.voice_id.is_some());
        if voice_layer.is_some_and(|i| i == track_layer || i == layers.len() - 1) {
            self.local_voice_ids.insert(item.node);
        }
        let voice = match self.providers.resolve_voice(&mut self.diags, &layers) {
            VoiceOutcome::Ok(v) => v,
            VoiceOutcome::DefaultVoice { voice, default } => {
                groups.add(
                    codes::DEFAULT_VOICE,
                    &voice.provider,
                    &item.id,
                    path,
                    Some(default),
                );
                voice
            }
            VoiceOutcome::MissingProvider => {
                groups.add(codes::MISSING_PROVIDER, "", &item.id, path, None);
                placeholder_voice()
            }
            VoiceOutcome::MissingVoice { provider } => {
                groups.add(codes::MISSING_VOICE, &provider, &item.id, path, None);
                placeholder_voice()
            }
            VoiceOutcome::Unusable => placeholder_voice(),
        };

        // Speaker label and gains.
        let speaker = cue_speaker.or(track_speaker);
        let speaker_spec = speaker.and_then(|s| speakers.get(s));
        let speaker_label = speaker_spec
            .and_then(|s| s.label.as_deref())
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .or(speaker)
            .unwrap_or(&plan.id)
            .to_string();
        let in_range = |g: Option<f64>| g.filter(|g| g.is_finite() && (-60.0..=24.0).contains(g));
        let speaker_gain_db = in_range(speaker_spec.and_then(|s| s.gain_db))
            .or_else(|| in_range(self.timeline.defaults.as_ref().and_then(|d| d.gain_db)))
            .unwrap_or(0.0);
        let volume =
            check::volume(&mut self.diags, cue.volume, &format!("{path}.volume")).unwrap_or(1.0);
        let gain_db =
            check::gain_db(&mut self.diags, cue.gain_db, &format!("{path}.gain_db")).unwrap_or(0.0);
        let start = self.start_rule(
            cue.start_at_ms.as_ref(),
            cue.start_relative.as_ref(),
            path,
            Owner::Node(item.node, plan.index),
        );

        Node {
            id: item.id.clone(),
            auto_id: item.auto_id,
            track: plan.index,
            index_in_track,
            path: item.path.clone(),
            start,
            kind: NodeKind::Cue(CueIr {
                text,
                caption,
                subtitles: cue.subtitles.unwrap_or(true),
                speaker: speaker.map(str::to_string),
                speaker_label,
                voice,
                gain: volume * db_to_linear(gain_db + speaker_gain_db),
                allow_overlap: cue.allow_overlap.unwrap_or(false),
            }),
        }
    }

    fn build_pause(
        &mut self,
        plan: &TrackPlan<'_>,
        item: &ItemPlan<'_>,
        pause: &SequenceItem,
        index_in_track: usize,
    ) -> Node {
        let path = item.path.as_str();
        if pause.pause_duration.is_some() && pause.pause.is_some() {
            self.diags.push(
                Diagnostic::error(
                    codes::CONFLICTING_FIELDS,
                    "both `pause_duration` and `pause` are set",
                )
                .with_path(format!("{path}.pause"))
                .with_hint("Keep one of them; `pause` is an alias of `pause_duration`"),
            );
        }
        for field in cue_fields(pause) {
            self.diags.push(
                Diagnostic::error(codes::CONFLICTING_FIELDS, format!("a pause cannot have `{field}`"))
                    .with_path(format!("{path}.{field}"))
                    .with_hint(if field == "id" {
                        "A pause takes only `pause_duration`; remove `id` (pauses are not anchor targets — anchor to the \
                         cue before or after it)"
                            .to_string()
                    } else {
                        format!(
                            "A sequence item is either a pause or a cue: remove `{field}`, or move it to a separate cue \
                             item with `text`"
                        )
                    }),
            );
        }
        let (value, field) = match (&pause.pause_duration, &pause.pause) {
            (Some(v), _) => (Some(v), "pause_duration"),
            (None, Some(v)) => (Some(v), "pause"),
            (None, None) => (None, "pause_duration"),
        };
        let field_path = format!("{path}.{field}");
        let duration_ms =
            check::millis(&mut self.diags, value, &field_path, Sign::NonNegative).unwrap_or(0.0);
        check::seconds_as_ms(
            &mut self.diags,
            value,
            duration_ms,
            &field_path,
            TimeUse::Pause,
        );
        Node {
            id: item.id.clone(),
            auto_id: true,
            track: plan.index,
            index_in_track,
            path: item.path.clone(),
            start: StartRule::Sequential,
            kind: NodeKind::Pause { duration_ms },
        }
    }

    fn build_clip(
        &mut self,
        plan: &TrackPlan<'_>,
        item: &ItemPlan<'_>,
        clip: &Clip,
        index_in_track: usize,
        first: bool,
    ) -> Node {
        let path = item.path.as_str();
        let owner = Owner::Node(item.node, plan.index);
        let start = self.start_rule(
            clip.start_at_ms.as_ref(),
            clip.start_relative.as_ref(),
            path,
            owner,
        );
        let source = self.source(clip.source.as_deref(), path, &format!("clip `{}`", item.id));
        let length = self.clip_length(
            ClipFields {
                duration: clip.duration_ms.as_ref(),
                end_at: clip.end_at_ms.as_ref(),
                end_relative: clip.end_relative.as_ref(),
                looped: clip.looped.unwrap_or(false),
                start_at: clip.start_at_ms.as_ref(),
            },
            path,
            owner,
            &start,
            &item.id,
        );
        let crossfade_path = format!("{path}.crossfade_ms");
        let mut crossfade_ms = check::millis(
            &mut self.diags,
            clip.crossfade_ms.as_ref(),
            &crossfade_path,
            Sign::NonNegative,
        )
        .unwrap_or(0.0);
        if first && crossfade_ms > 0.0 {
            self.diags.push(
                Diagnostic::warning(
                    codes::FIELD_NOT_APPLICABLE,
                    format!("`crossfade_ms` has no effect on the first clip of track `{}`", plan.id),
                )
                .with_path(crossfade_path)
                .with_hint("There is no previous clip to crossfade with; use `fade_in_ms` to fade the first clip in"),
            );
            crossfade_ms = 0.0;
        } else {
            check::seconds_as_ms(
                &mut self.diags,
                clip.crossfade_ms.as_ref(),
                crossfade_ms,
                &crossfade_path,
                TimeUse::Fade,
            );
        }
        let volume =
            check::volume(&mut self.diags, clip.volume, &format!("{path}.volume")).unwrap_or(1.0);
        let gain_db = check::gain_db(&mut self.diags, clip.gain_db, &format!("{path}.gain_db"))
            .unwrap_or(0.0);
        let clip_ir = self.clip_ir(
            source,
            ClipCommon {
                trim_start: clip.trim_start_ms.as_ref(),
                looped: clip.looped,
                loop_crossfade: clip.loop_crossfade_ms.as_ref(),
                fade_in: clip.fade_in_ms.as_ref(),
                fade_out: clip.fade_out_ms.as_ref(),
                fade_curve: clip.fade_curve.as_deref(),
            },
            path,
            length,
            crossfade_ms,
            volume * db_to_linear(gain_db),
            false,
        );
        Node {
            id: item.id.clone(),
            auto_id: item.auto_id,
            track: plan.index,
            index_in_track,
            path: item.path.clone(),
            start,
            kind: NodeKind::Clip(clip_ir),
        }
    }

    fn build_shorthand(
        &mut self,
        plan: &TrackPlan<'_>,
        item: &ItemPlan<'_>,
        track_start: &StartRule,
    ) -> Node {
        let raw = plan.raw;
        let path = plan.path.as_str();
        let owner = Owner::Node(item.node, plan.index);
        let source = self.source(raw.source.as_deref(), path, &format!("track `{}`", plan.id));
        let length = self.clip_length(
            ClipFields {
                duration: raw.duration_ms.as_ref(),
                end_at: raw.end_at_ms.as_ref(),
                end_relative: raw.end_relative.as_ref(),
                looped: raw.looped.unwrap_or(false),
                start_at: raw.start_at_ms.as_ref(),
            },
            path,
            owner,
            track_start,
            &plan.id,
        );
        let clip_ir = self.clip_ir(
            source,
            ClipCommon {
                trim_start: raw.trim_start_ms.as_ref(),
                looped: raw.looped,
                loop_crossfade: raw.loop_crossfade_ms.as_ref(),
                fade_in: raw.fade_in_ms.as_ref(),
                fade_out: raw.fade_out_ms.as_ref(),
                fade_curve: raw.fade_curve.as_deref(),
            },
            path,
            length,
            0.0,
            1.0,
            true,
        );
        Node {
            id: item.id.clone(),
            auto_id: false,
            track: plan.index,
            index_in_track: 0,
            path: plan.path.clone(),
            start: StartRule::Sequential,
            kind: NodeKind::Clip(clip_ir),
        }
    }

    /// The clip `source` (required, non-empty).
    fn source(&mut self, source: Option<&str>, path: &str, what: &str) -> String {
        match source.map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            Some(_) => {
                self.diags.push(
                    Diagnostic::error(codes::INVALID_VALUE, format!("{what} has an empty `source`"))
                        .with_path(format!("{path}.source"))
                        .with_hint("Set `source` to an audio file path relative to the timeline, e.g. `audio/bed.wav`"),
                );
                String::new()
            }
            None => {
                self.diags.push(
                    Diagnostic::error(codes::MISSING_FIELD, format!("{what} has no `source`"))
                        .with_path(format!("{path}.source"))
                        .with_hint(
                            "Add `source: path/to/audio.wav` (relative to the timeline file)",
                        ),
                );
                String::new()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn clip_ir(
        &mut self,
        source: String,
        common: ClipCommon<'_>,
        path: &str,
        length: ClipLength,
        crossfade_ms: f64,
        gain: f64,
        shorthand: bool,
    ) -> ClipIr {
        let ms = |s: &mut Self, v: Option<&Millis>, field: &str| {
            check::millis(
                &mut s.diags,
                v,
                &format!("{path}.{field}"),
                Sign::NonNegative,
            )
            .unwrap_or(0.0)
        };
        let trim_start_ms = ms(self, common.trim_start, "trim_start_ms");
        let loop_crossfade_ms = ms(self, common.loop_crossfade, "loop_crossfade_ms");
        let fade_in_ms = ms(self, common.fade_in, "fade_in_ms");
        let fade_out_ms = ms(self, common.fade_out, "fade_out_ms");
        for (value, fade_ms, field) in [
            (common.fade_in, fade_in_ms, "fade_in_ms"),
            (common.fade_out, fade_out_ms, "fade_out_ms"),
        ] {
            check::seconds_as_ms(
                &mut self.diags,
                value,
                fade_ms,
                &format!("{path}.{field}"),
                TimeUse::Fade,
            );
        }
        let fade_curve = check::enum_name(
            &mut self.diags,
            common.fade_curve,
            &format!("{path}.fade_curve"),
            FadeCurve::parse,
            FadeCurve::NAMES,
            "fade curve",
        )
        .unwrap_or(FadeCurve::EqualPower);
        let source_path = self.options.base_dir.join(&source);
        ClipIr {
            source,
            source_path,
            trim_start_ms,
            length,
            looped: common.looped.unwrap_or(false),
            loop_crossfade_ms,
            fade_in_ms,
            fade_out_ms,
            fade_curve,
            crossfade_ms,
            gain,
            shorthand,
        }
    }

    /// The length rule of a clip (DESIGN.md §3.3), with the static checks: at most one of `duration_ms` /
    /// `end_at_ms` / `end_relative`; an absolute end after an absolute start; no end rule (or open-ended
    /// loop) together with a `before_cue` start, which already fixes the end.
    fn clip_length(
        &mut self,
        f: ClipFields<'_>,
        path: &str,
        owner: Owner,
        start: &StartRule,
        id: &str,
    ) -> ClipLength {
        let present: Vec<&str> = [
            ("duration_ms", f.duration.is_some()),
            ("end_at_ms", f.end_at.is_some()),
            ("end_relative", f.end_relative.is_some()),
        ]
        .into_iter()
        .filter(|(_, p)| *p)
        .map(|(n, _)| n)
        .collect();
        if present.len() > 1 {
            self.diags.push(
                Diagnostic::error(
                    codes::CONFLICTING_FIELDS,
                    format!(
                        "`{}` and `{}` both set the length of `{id}`",
                        present[0], present[1]
                    ),
                )
                .with_path(format!("{path}.{}", present[1]))
                .with_hint("Keep exactly one of `duration_ms`, `end_at_ms` or `end_relative`"),
            );
        }
        let before_start =
            matches!(start, StartRule::Relative(rule) if rule.mode == AnchorMode::Before);
        if before_start && (f.end_at.is_some() || f.end_relative.is_some()) {
            let field = if f.end_at.is_some() {
                "end_at_ms"
            } else {
                "end_relative"
            };
            self.diags.push(
                Diagnostic::error(
                    codes::CONFLICTING_FIELDS,
                    format!("`start_relative.before_cue` already fixes where `{id}` ends; it cannot also have `{field}`"),
                )
                .with_path(format!("{path}.{field}"))
                .with_hint("Use `duration_ms` together with `before_cue`, or anchor the start with `after_cue` / `with_cue`"),
            );
        } else if before_start && f.looped && f.duration.is_none() {
            self.diags.push(
                Diagnostic::error(
                    codes::CONFLICTING_FIELDS,
                    format!("`{id}` loops without a length, so it cannot end at `before_cue`"),
                )
                .with_path(format!("{path}.start_relative.before_cue"))
                .with_hint(
                    "Give the loop a `duration_ms`, or start it with `after_cue` / `with_cue`",
                ),
            );
        }
        // A length the document sets but that is invalid (reported) or was dropped by the parser falls back
        // to `Natural`; the clip is then not treated as an open-ended loop by the graph checks.
        let node = match owner {
            Owner::Node(n, _) => Some(n),
            Owner::Track(_) => None,
        };
        let dropped = ["duration_ms", "end_at_ms", "end_relative"]
            .iter()
            .any(|field| self.is_dropped(&format!("{path}.{field}")));
        let invalid = |s: &mut Self| {
            if let Some(n) = node {
                s.invalid_length.insert(n);
            }
            ClipLength::Natural
        };
        if let Some(d) = f.duration {
            let field = format!("{path}.duration_ms");
            return match check::millis(&mut self.diags, Some(d), &field, Sign::Positive) {
                Some(ms) => {
                    check::seconds_as_ms(&mut self.diags, Some(d), ms, &field, TimeUse::Length);
                    ClipLength::Duration { ms }
                }
                None => invalid(self),
            };
        }
        if let Some(e) = f.end_at {
            let field = format!("{path}.end_at_ms");
            let Some(end) = check::millis(&mut self.diags, Some(e), &field, Sign::NonNegative)
            else {
                return invalid(self);
            };
            if let StartRule::Absolute { ms: begin } = start
                && end <= *begin
            {
                self.diags.push(
                    Diagnostic::error(
                        codes::INVALID_VALUE,
                        format!("`{id}` ends at {} ms, before it starts ({} ms)", fmt_ms(end), fmt_ms(*begin)),
                    )
                    .with_path(field)
                    .with_hint("Use an `end_at_ms` later than `start_at_ms`, or `duration_ms` for a length"),
                );
            } else {
                self.end_at_in_seconds(e, end, f.start_at, start, &field);
            }
            return ClipLength::EndAbsolute { ms: end };
        }
        if let Some(anchor) = f.end_relative {
            return match self.resolve_anchor(
                anchor,
                &format!("{path}.end_relative"),
                owner,
                Slot::End,
            ) {
                Some(rule) => ClipLength::EndRelative(rule),
                None => invalid(self),
            };
        }
        if dropped {
            return invalid(self);
        }
        ClipLength::Natural
    }

    /// `suspicious_value` for an `end_at_ms` that is a bare number below 100 (so the clip is shorter than
    /// 100 ms): when the start is a bare number too, the hint converts both.
    fn end_at_in_seconds(
        &mut self,
        end_value: &Millis,
        end: f64,
        start_value: Option<&Millis>,
        start: &StartRule,
        path: &str,
    ) {
        if !check::looks_like_seconds(end_value, end, TimeUse::Length) {
            return;
        }
        let begin = match start {
            StartRule::Absolute { ms }
                if *ms > 0.0 && start_value.is_some_and(check::is_bare_number) =>
            {
                Some(*ms)
            }
            _ => None,
        };
        let e = check::fmt_num(end);
        let unit = if end == 1.0 {
            "millisecond"
        } else {
            "milliseconds"
        };
        let hint = match begin {
            Some(b) => {
                let b = check::fmt_num(b);
                format!(
                    "Times are milliseconds: write `start_at_ms: \"{b}s\"` and `end_at_ms: \"{e}s\"` if you meant \
                     {b} to {e} seconds"
                )
            }
            None => format!(
                "Times are milliseconds: write `end_at_ms: \"{e}s\"` if you meant {e} seconds"
            ),
        };
        self.diags.push(
            Diagnostic::warning(
                codes::SUSPICIOUS_VALUE,
                format!("`end_at_ms: {e}` is {e} {unit}"),
            )
            .with_path(path)
            .with_hint(hint),
        );
    }

    // ---- start rules and anchors -------------------------------------------------------------------

    /// The start rule from `start_at_ms` / `start_relative` (mutually exclusive).
    fn start_rule(
        &mut self,
        at: Option<&Millis>,
        relative: Option<&Anchor>,
        path: &str,
        owner: Owner,
    ) -> StartRule {
        if at.is_some() && relative.is_some() {
            self.diags.push(
                Diagnostic::error(codes::CONFLICTING_FIELDS, "`start_at_ms` and `start_relative` are both set")
                    .with_path(format!("{path}.start_relative"))
                    .with_hint("Keep one: `start_at_ms` for an absolute time, or `start_relative` to follow another item"),
            );
        }
        if let Some(at) = at {
            let field = format!("{path}.start_at_ms");
            return match check::millis(&mut self.diags, Some(at), &field, Sign::NonNegative) {
                Some(ms) => {
                    check::seconds_as_ms(&mut self.diags, Some(at), ms, &field, TimeUse::Start);
                    StartRule::Absolute { ms }
                }
                None => StartRule::Sequential,
            };
        }
        if let Some(anchor) = relative
            && let Some(rule) = self.resolve_anchor(
                anchor,
                &format!("{path}.start_relative"),
                owner,
                Slot::Start,
            )
        {
            return StartRule::Relative(rule);
        }
        StartRule::Sequential
    }

    /// Resolve an anchor written at `path` (`…start_relative` / `…end_relative`).
    fn resolve_anchor(
        &mut self,
        anchor: &Anchor,
        path: &str,
        owner: Owner,
        slot: Slot,
    ) -> Option<AnchorRule> {
        let field_name = if slot == Slot::Start {
            "start_relative"
        } else {
            "end_relative"
        };
        let mut targets: Vec<(&str, AnchorMode, &str)> = Vec::new();
        if let Some(t) = &anchor.after_cue {
            targets.push(("after_cue", AnchorMode::After, t));
        }
        if let Some(t) = &anchor.with_cue {
            targets.push(("with_cue", AnchorMode::With, t));
        }
        if let Some(t) = &anchor.before_cue {
            if slot == Slot::End {
                self.diags.push(
                    Diagnostic::error(codes::CONFLICTING_FIELDS, "`before_cue` cannot be used in `end_relative`")
                        .with_path(format!("{path}.before_cue"))
                        .with_hint(
                            "Use `after_cue` (end after the item ends) or `with_cue` (end when the item starts); \
                             `before_cue` only works in `start_relative`",
                        ),
                );
                if targets.is_empty() {
                    return None;
                }
            } else {
                targets.push(("before_cue", AnchorMode::Before, t));
            }
        }
        let (key, mode, raw_target) = match targets.as_slice() {
            [] => {
                // A target the parser dropped (`after_cue: [cue_01]`) was reported there already.
                let dropped = ["after_cue", "with_cue", "before_cue"]
                    .iter()
                    .any(|k| self.is_dropped(&format!("{path}.{k}")));
                if !dropped {
                    self.diags.push(
                        Diagnostic::error(
                            codes::CONFLICTING_FIELDS,
                            format!("`{field_name}` needs exactly one of `after_cue`, `with_cue` or `before_cue`"),
                        )
                        .with_path(path)
                        .with_hint(format!("e.g. `{field_name}: {{after_cue: cue_01, offset_ms: 150}}`")),
                    );
                }
                return None;
            }
            [single] => *single,
            [first, second, ..] => {
                self.diags.push(
                    Diagnostic::error(
                        codes::CONFLICTING_FIELDS,
                        format!("`{field_name}` sets both `{}` and `{}`", first.0, second.0),
                    )
                    .with_path(format!("{path}.{}", second.0))
                    .with_hint("Keep exactly one anchor target"),
                );
                return None;
            }
        };
        let offset_path = format!("{path}.offset_ms");
        let offset_ms = check::millis(
            &mut self.diags,
            anchor.offset_ms.as_ref(),
            &offset_path,
            Sign::Any,
        )
        .unwrap_or(0.0);
        check::seconds_as_ms(
            &mut self.diags,
            anchor.offset_ms.as_ref(),
            offset_ms,
            &offset_path,
            TimeUse::Start,
        );
        let target_path = format!("{path}.{key}");
        let target_id = raw_target.trim();
        let Some(entry) = self.ids.get(target_id).cloned() else {
            self.unknown_reference(key, target_id, &target_path);
            return None;
        };

        // Self references.
        let self_ref = match owner {
            Owner::Node(n, t) => {
                if entry.target == AnchorTarget::Node(n) {
                    Some(format!("`{target_id}` cannot be anchored to itself"))
                } else if entry.target == AnchorTarget::Track(t) {
                    Some(format!(
                        "an item of track `{target_id}` cannot be anchored to its own track"
                    ))
                } else {
                    None
                }
            }
            Owner::Track(t) => {
                if entry.target == AnchorTarget::Track(t) {
                    Some(format!(
                        "track `{target_id}` cannot start relative to itself"
                    ))
                } else if entry.track == t {
                    Some(format!(
                        "a track cannot start relative to its own {} `{target_id}`",
                        entry.kind.as_str()
                    ))
                } else {
                    None
                }
            }
        };
        if let Some(message) = self_ref {
            self.diags.push(
                Diagnostic::error(codes::SELF_REFERENCE, message)
                    .with_path(target_path)
                    .with_hint(
                        "Anchor to a different cue, clip or track (e.g. the previous cue of another track), or use \
                         `start_at_ms` for an absolute time",
                    )
                    .with_related([target_id.to_string()]),
            );
            return None;
        }

        let rule = AnchorRule {
            target: entry.target,
            target_id: target_id.to_string(),
            mode,
            offset_ms,
        };
        self.anchors.push(AnchorUse {
            owner,
            slot,
            rule: rule.clone(),
            path: target_path,
        });
        Some(rule)
    }

    fn unknown_reference(&mut self, key: &str, target: &str, path: &str) {
        let known: Vec<&str> = self.ids.keys().map(String::as_str).collect();
        let suggestion = check::suggest(target, known.iter().copied());
        let listed = diagnostics::list_names(known.iter().copied(), 12);
        let hint = if let Some(pause_path) = self.pause_ids.get(target) {
            format!(
                "`{target}` is the pause at `{pause_path}`; pauses cannot be anchors — anchor to the cue before or after \
                 it"
            )
        } else if target.is_empty() {
            format!("Set `{key}` to the id of a cue, clip or track. Known ids: {listed}")
        } else {
            match suggestion {
                Some(s) => format!("Did you mean `{s}`? Known ids: {listed}"),
                None => format!("Known ids: {listed}"),
            }
        };
        let message = if target.is_empty() {
            format!("`{key}` is empty")
        } else {
            format!("`{key}` refers to `{target}`, which is not the id of a cue, clip or track")
        };
        let mut d = Diagnostic::error(codes::UNKNOWN_REFERENCE, message)
            .with_path(path)
            .with_hint(hint);
        if let Some(s) = suggestion {
            d = d.with_related([s.to_string()]);
        }
        self.diags.push(d);
    }
}

/// Length-related clip fields (clip entry or track shorthand).
struct ClipFields<'a> {
    duration: Option<&'a Millis>,
    end_at: Option<&'a Millis>,
    end_relative: Option<&'a Anchor>,
    looped: bool,
    /// The clip's `start_at_ms` as written (the track's for the shorthand), for hints.
    start_at: Option<&'a Millis>,
}

/// Playback-related clip fields (clip entry or track shorthand).
struct ClipCommon<'a> {
    trim_start: Option<&'a Millis>,
    looped: Option<bool>,
    loop_crossfade: Option<&'a Millis>,
    fade_in: Option<&'a Millis>,
    fade_out: Option<&'a Millis>,
    fade_curve: Option<&'a str>,
}

/// Voice of a cue whose voice could not be resolved (only present in programs that are discarded).
fn placeholder_voice() -> VoiceIr {
    VoiceIr {
        provider: String::new(),
        provider_type: ProviderType::Mock,
        voice_id: None,
        model: None,
        language: None,
        settings: serde_json::Map::new(),
    }
}
