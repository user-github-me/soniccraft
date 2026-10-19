//! Lowering: raw [`Timeline`] → validated [`Program`] + diagnostics.
//!
//! Every semantic rule of DESIGN.md §2 (and the static parts of §3.3) is checked here and reported with
//! the stable codes of §7; lowering never stops at the first problem. The passes:
//!
//! 1. document-wide settings: `version`, `output` (+ CLI/MCP overrides), `subtitles`, `timing`, `mix`;
//! 2. provider instances and the `defaults` / `speakers` voice layers;
//! 3. tracks, pass A: ids (explicit and automatic), track types, item classification, the shared id
//!    namespace and duplicate detection;
//! 4. tracks, pass B: per-track settings, field applicability, cues (voice resolution, gains, labels),
//!    pauses, clips, anchors and ducking;
//! 5. the static dependency graph (self references, cycles, anchors on open-ended loops);
//! 6. speakers that share one voice;
//! 7. the automatic subtitle speaker-label switch, then assembly of the [`Program`].
//!
//! Every time value is finite and within ±[`crate::MAX_TIME_MS`] (24 h) once lowered. A [`Program`] is
//! returned only when no error was reported.

pub(crate) mod assets;
mod check;
mod graph;
mod output;
mod speakers;
mod tracks;
mod voices;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use indexmap::IndexMap;

use crate::diagnostics::{self, Diagnostic, codes};
use crate::ir::{AnchorTarget, Node, NodeId, OutputFormat, Program, TrackIdx, TrackIr};
use crate::model::Timeline;

pub use assets::AssetRef;
pub use output::{
    DEFAULT_BIT_DEPTH, DEFAULT_CHANNELS, DEFAULT_SAMPLE_RATE, MP3_SAMPLE_RATES,
    default_bitrate_bps, mp3_bitrates_kbps, preset_targets,
};

/// Inputs to lowering that do not come from the document.
#[derive(Debug, Clone, Default)]
pub struct LowerOptions {
    /// Directory that relative paths (sources, output path) resolve against.
    pub base_dir: PathBuf,
    /// Output path override (CLI `-o`, MCP `output_path`); its extension selects the format unless
    /// `format_override` or `output.format` is set.
    pub output_path_override: Option<String>,
    /// Output format override (CLI `--format`).
    pub format_override: Option<OutputFormat>,
}

/// Result of [`lower`].
#[derive(Debug, Clone)]
pub struct Lowered {
    /// Present only when lowering produced no errors.
    pub program: Option<Program>,
    /// Every problem and note found, in document order (not yet positioned or sorted; see
    /// [`crate::compile`]).
    pub diagnostics: Vec<Diagnostic>,
}

/// Validate and lower a timeline.
pub fn lower(timeline: &Timeline, options: &LowerOptions) -> Lowered {
    lower_parsed(timeline, options, Recovery::default())
}

/// What parse error recovery already reported, so lowering does not report its consequences again (see
/// [`crate::compile`]).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Recovery<'a> {
    /// Paths of values the parser dropped ([`crate::parse::Parsed::dropped`]): an anchor whose target key
    /// was dropped, or a loop whose length was dropped, is not reported again.
    pub dropped: &'a [String],
    /// Paths of sequence items whose only keys are misspelled pause keys (`{pause_ms: 300}`, `{wait: 1s}`),
    /// each reported as `unknown_field`: they are lowered as pauses, not as cues without text, provider or
    /// voice.
    pub misspelled_pauses: &'a [String],
}

/// The paths of dropped values ([`Recovery::dropped`]), indexed so that "is this path at or below a dropped
/// value" costs O(path length) instead of O(dropped values) (a large broken document has many of both).
pub(crate) struct DroppedPaths<'a> {
    paths: HashSet<&'a str>,
}

impl<'a> DroppedPaths<'a> {
    pub(crate) fn new(dropped: &'a [String]) -> Self {
        DroppedPaths {
            paths: dropped.iter().map(String::as_str).collect(),
        }
    }

    /// True when `path` is a dropped path or below one (`a.b` and `a[0]` are below `a`), exactly as
    /// [`crate::parse::Parsed::is_dropped`].
    pub(crate) fn covers(&self, path: &str) -> bool {
        !self.paths.is_empty()
            && (self.paths.contains(path)
                || path
                    .char_indices()
                    .any(|(i, c)| matches!(c, '.' | '[') && self.paths.contains(&path[..i])))
    }
}

/// [`lower`] for a parsed document whose parse reported (and dropped) the values in `recovery`.
pub(crate) fn lower_parsed(
    timeline: &Timeline,
    options: &LowerOptions,
    recovery: Recovery<'_>,
) -> Lowered {
    let mut lowerer = Lowerer::new(timeline, options, recovery);
    let program = lowerer.run();
    let program = if diagnostics::has_errors(&lowerer.diags) {
        None
    } else {
        program
    };
    Lowered {
        program,
        diagnostics: lowerer.diags,
    }
}

/// What an id in the shared namespace names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdKind {
    Track,
    Cue,
    Clip,
}

impl IdKind {
    fn as_str(self) -> &'static str {
        match self {
            IdKind::Track => "track",
            IdKind::Cue => "cue",
            IdKind::Clip => "clip",
        }
    }
}

/// An entry of the shared id namespace (tracks, cues, clips).
#[derive(Debug, Clone)]
struct IdEntry {
    target: AnchorTarget,
    kind: IdKind,
    /// The track the id belongs to (itself for tracks).
    track: TrackIdx,
    /// Diagnostic path of the declaring element.
    path: String,
}

/// State shared by the lowering passes.
struct Lowerer<'a> {
    timeline: &'a Timeline,
    options: &'a LowerOptions,
    /// Values the parser dropped ([`Recovery::dropped`]).
    dropped: DroppedPaths<'a>,
    /// Sequence items that are misspelled pauses ([`Recovery::misspelled_pauses`]).
    misspelled_pauses: HashSet<&'a str>,
    diags: Vec<Diagnostic>,
    providers: voices::Providers,
    reference_lufs: f64,
    /// The shared id namespace, in document order.
    ids: IndexMap<String, IdEntry>,
    /// Automatic pause ids → path (pauses are not anchor targets; used for hints).
    pause_ids: HashMap<String, String>,
    tracks: Vec<TrackIr>,
    nodes: Vec<Node>,
    /// Every resolved anchor, for the dependency graph and open-ended checks.
    anchors: Vec<graph::AnchorUse>,
    /// Clips whose length field (`duration_ms`, `end_at_ms`, `end_relative`) was reported invalid or dropped:
    /// their IR length falls back to `Natural`, but they are not open-ended loops for the graph checks.
    invalid_length: HashSet<NodeId>,
    /// Cues whose speaker (of the cue or its track) is a misspelled `speakers` entry (an error). Their voice
    /// is resolved with the speaker the did-you-mean names, but they are left out of `speakers_share_voice`.
    speaker_errors: HashSet<NodeId>,
    /// Cues whose `voice_id` comes from the cue or its track (not from a speaker, `defaults` or the provider
    /// default), for the hints of `speakers_share_voice`.
    local_voice_ids: HashSet<NodeId>,
}

impl<'a> Lowerer<'a> {
    fn new(timeline: &'a Timeline, options: &'a LowerOptions, recovery: Recovery<'a>) -> Self {
        let mut diags = Vec::new();
        let providers = voices::Providers::new(&mut diags, &timeline.providers);
        Lowerer {
            timeline,
            options,
            dropped: DroppedPaths::new(recovery.dropped),
            misspelled_pauses: recovery
                .misspelled_pauses
                .iter()
                .map(String::as_str)
                .collect(),
            diags,
            providers,
            reference_lufs: -20.0,
            ids: IndexMap::new(),
            pause_ids: HashMap::new(),
            tracks: Vec::new(),
            nodes: Vec::new(),
            anchors: Vec::new(),
            invalid_length: HashSet::new(),
            speaker_errors: HashSet::new(),
            local_voice_ids: HashSet::new(),
        }
    }

    /// True when `path` is at or below a value the parser dropped.
    fn is_dropped(&self, path: &str) -> bool {
        self.dropped.covers(path)
    }

    /// True for a sequence item at `path` whose only keys are misspelled pause keys.
    fn is_misspelled_pause(&self, path: &str) -> bool {
        self.misspelled_pauses.contains(path)
    }

    fn run(&mut self) -> Option<Program> {
        let t = self.timeline;
        let version = self.version();
        let output = output::output(&mut self.diags, t.output.as_ref(), self.options);
        let (mut subtitles, explicit_labels) =
            output::subtitles(&mut self.diags, t.subtitles.as_ref());
        let timing = output::timing(&mut self.diags, t.timing.as_ref());
        let mix = output::mix(&mut self.diags, t.mix.as_ref());
        self.reference_lufs = mix.reference_lufs;

        self.voice_specs();
        self.lower_tracks();
        graph::check(self);
        self.speakers_share_voice();

        subtitles.speaker_labels = explicit_labels.unwrap_or_else(|| {
            let mut labels: Vec<&str> = self
                .nodes
                .iter()
                .filter_map(Node::as_cue)
                .filter(|c| c.subtitles)
                .map(|c| c.speaker_label.as_str())
                .collect();
            labels.sort_unstable();
            labels.dedup();
            labels.len() >= 2
        });

        let version = version?;
        let providers = std::mem::replace(&mut self.providers, voices::Providers::empty())
            .into_program_providers();
        Some(Program {
            version,
            title: t.title.clone(),
            base_dir: self.options.base_dir.clone(),
            output,
            subtitles,
            timing,
            mix,
            tracks: std::mem::take(&mut self.tracks),
            nodes: std::mem::take(&mut self.nodes),
            providers,
            ids: self
                .ids
                .iter()
                .map(|(id, e)| (id.clone(), e.target))
                .collect(),
        })
    }

    /// `version` (DESIGN.md §2.2).
    fn version(&mut self) -> Option<String> {
        let Some(version) = &self.timeline.version else {
            self.diags.push(
                Diagnostic::error(codes::MISSING_FIELD, "the timeline has no `version`")
                    .with_path("version")
                    .with_hint("Add `version: \"1.0\"` as the first line of the timeline"),
            );
            return None;
        };
        match version.major_minor() {
            None => {
                self.diags.push(
                    Diagnostic::error(
                        codes::INVALID_VALUE,
                        format!("`version` {} is not a version number", version.0),
                    )
                    .with_path("version")
                    .with_hint("Use `version: \"1.0\"`"),
                );
                None
            }
            Some((1, minor)) => {
                if minor > 0 {
                    self.diags.push(
                        Diagnostic::warning(
                            codes::UNSUPPORTED_VERSION,
                            format!(
                                "DSL version 1.{minor} is newer than this SonicCraft ({}); features it does not know \
                                 are reported as unknown fields",
                                crate::DSL_VERSION
                            ),
                        )
                        .with_path("version")
                        .with_hint(format!("Use `version: \"{}\"` or upgrade SonicCraft", crate::DSL_VERSION)),
                    );
                }
                Some(format!("1.{minor}"))
            }
            Some((major, minor)) => {
                self.diags.push(
                    Diagnostic::error(
                        codes::UNSUPPORTED_VERSION,
                        format!("DSL version {major}.{minor} is not supported (this SonicCraft reads 1.x)"),
                    )
                    .with_path("version")
                    .with_hint(format!(
                        "Set `version: \"{}\"` and adapt the timeline to DSL 1.0, or use a SonicCraft release that \
                         supports version {major}",
                        crate::DSL_VERSION
                    )),
                );
                None
            }
        }
    }

    /// The `defaults` and `speakers` voice layers: field applicability, gains, provider references.
    fn voice_specs(&mut self) {
        let t = self.timeline;
        if let Some(defaults) = &t.defaults {
            self.voice_spec(defaults, "defaults", false);
        }
        for (name, spec) in &t.speakers {
            self.voice_spec(spec, &format!("speakers.{name}"), true);
        }
    }

    fn voice_spec(&mut self, spec: &crate::model::VoiceSpec, path: &str, is_speaker: bool) {
        voices::check_empty_voice_fields(
            &mut self.diags,
            path,
            [
                ("provider", spec.provider.as_ref()),
                ("voice_id", spec.voice_id.as_ref()),
                ("model", spec.model.as_ref()),
                ("language", spec.language.as_ref()),
            ],
        );
        if let Some(provider) = spec.provider.as_deref().filter(|p| !p.trim().is_empty()) {
            self.providers
                .check_ref(&mut self.diags, provider, &format!("{path}.provider"));
        }
        voices::check_settings_keys(
            &mut self.diags,
            spec.settings.as_ref(),
            path,
            if is_speaker {
                voices::SettingsOwner::Speaker
            } else {
                voices::SettingsOwner::Defaults
            },
            voices::VoiceFields::of_voice_spec(spec),
        );
        check::gain_db(&mut self.diags, spec.gain_db, &format!("{path}.gain_db"));
        if !is_speaker && spec.label.is_some() {
            self.diags.push(
                Diagnostic::warning(
                    codes::FIELD_NOT_APPLICABLE,
                    "`label` only applies to speakers and is ignored here",
                )
                .with_path(format!("{path}.label"))
                .with_hint(
                    "Set `label` on an entry of `speakers:` to change the name shown in subtitles",
                ),
            );
        }
    }

    /// Register `id` in the shared namespace; reports `duplicate_id` (at `report_path`) on collisions.
    fn register(&mut self, id: &str, entry: IdEntry, report_path: &str, auto: bool) -> bool {
        if let Some(existing) = self.ids.get(id) {
            let message = format!(
                "id `{id}` is already used by the {} at `{}`",
                existing.kind.as_str(),
                existing.path
            );
            let hint = if auto {
                format!(
                    "`{id}` is the automatic id of this {}; give it an explicit `id:` or rename the other {}",
                    entry.kind.as_str(),
                    existing.kind.as_str()
                )
            } else {
                "Rename one of them; tracks, cues and clips share one id namespace".to_string()
            };
            self.diags.push(
                Diagnostic::error(codes::DUPLICATE_ID, message)
                    .with_path(report_path)
                    .with_hint(hint)
                    .with_related([id.to_string()]),
            );
            return false;
        }
        self.ids.insert(id.to_string(), entry);
        true
    }
}

impl voices::Providers {
    /// A placeholder used while moving the providers out of the lowerer.
    fn empty() -> Self {
        voices::Providers::new(&mut Vec::new(), &IndexMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let o = LowerOptions::default();
        assert!(o.output_path_override.is_none());
        assert!(o.format_override.is_none());
    }

    #[test]
    fn dropped_paths_cover_their_subtrees() {
        let dropped = [
            "tracks[0].sequence[1].start_relative".to_string(),
            "output".to_string(),
        ];
        let index = DroppedPaths::new(&dropped);
        for (path, covered) in [
            ("tracks[0].sequence[1].start_relative", true),
            ("tracks[0].sequence[1].start_relative.after_cue", true),
            ("output", true),
            ("output.format", true),
            ("outputs", false),
            ("tracks[0].sequence[1].start_relative_x", false),
            ("tracks[0].sequence[1]", false),
            ("tracks[0].sequence[10].start_relative", false),
            ("", false),
        ] {
            assert_eq!(index.covers(path), covered, "{path}");
        }
        assert!(!DroppedPaths::new(&[]).covers("output"));
        // List elements are below their list.
        let list = ["tracks[2].clips".to_string()];
        assert!(DroppedPaths::new(&list).covers("tracks[2].clips[0].source"));
    }
}
