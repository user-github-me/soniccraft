//! Test support (cargo feature `test-support`): build lowered [`Program`]s and duration tables in code,
//! without going through parsing and lowering.
//!
//! The builder fills every IR field with the defaults lowering would produce (48 kHz, `push` policy,
//! 250 ms `min_gap_ms`, 20 ms tolerance, the `mock` provider, ...). Anchors are written as ids and resolved
//! when the program is built, so items may reference items declared later. Node ids are assigned in
//! document order (track by track, item by item), exactly like lowering does.
//!
//! ```
//! use soniccraft_layout::test_support::ProgramBuilder;
//!
//! let timeline = ProgramBuilder::new()
//!     .sample_rate(1000) // 1 frame = 1 ms keeps test arithmetic readable
//!     .speech("host", |t| {
//!         t.cue("cue_01", "Welcome.", 2000.0);
//!         t.pause(350.0);
//!         t.cue("cue_03", "So, tell us more.", 1500.0);
//!     })
//!     .speech("guest", |t| {
//!         t.cue("cue_02", "Thanks for having me.", 1800.0).after("cue_01", 150.0);
//!     })
//!     .build();
//! let layout = timeline.layout();
//! // cue_03 would start at 2350 ms, inside cue_02 [2150, 3950): it is pushed to 3950 + 350 ms.
//! assert_eq!(layout.placements[timeline.node("cue_03")].start, 4300);
//! ```

use std::path::PathBuf;

use soniccraft_core::ir::{
    AnchorMode, AnchorRule, AnchorTarget, ClipIr, ClipLength, CueIr, DuckingConfig, DuckingMode,
    FadeCurve, LoudnessPreset, MixConfig, Node, NodeId, NodeKind, OutputConfig, OutputFormat,
    OverlapPolicy, Program, ProviderInstance, ProviderType, StartRule, SubtitleConfig,
    TimingConfig, TrackIdx, TrackIr, TrackKind, TrimConfig, VoiceIr, ms_to_frames,
};

use crate::types::{DurationSource, Layout, NodeDuration};

/// A built test timeline: the program plus its duration table.
#[derive(Debug, Clone, PartialEq)]
pub struct TestTimeline {
    /// The lowered program.
    pub program: Program,
    /// One entry per node: cue audio length, whole clip source length, pause length.
    pub durations: Vec<NodeDuration>,
}

impl TestTimeline {
    /// Run [`crate::layout`] on this timeline.
    pub fn layout(&self) -> Layout {
        crate::layout(&self.program, &self.durations)
    }

    /// Run the layout with the straightforward implementation of the `push` policy, which scans every
    /// scheduled cue on every move, instead of the ordered structures [`crate::layout`] uses. The results are
    /// identical; property tests compare the two.
    pub fn reference_layout(&self) -> Layout {
        crate::schedule::run(
            &self.program,
            &self.durations,
            crate::schedule::Settings {
                reference_push: true,
            },
        )
    }

    /// The node id of a cue, clip or pause id, if it exists.
    pub fn try_node(&self, id: &str) -> Option<NodeId> {
        self.program.nodes.iter().position(|n| n.id == id)
    }

    /// The node id of a cue, clip or pause id.
    ///
    /// # Panics
    /// When no node has this id (a mistake in the test).
    pub fn node(&self, id: &str) -> NodeId {
        self.try_node(id)
            .unwrap_or_else(|| panic!("test timeline has no node `{id}`"))
    }

    /// The track index of a track id.
    ///
    /// # Panics
    /// When no track has this id (a mistake in the test).
    pub fn track(&self, id: &str) -> TrackIdx {
        self.program
            .track_index(id)
            .unwrap_or_else(|| panic!("test timeline has no track `{id}`"))
    }

    /// Replace the duration table entry of a node.
    ///
    /// # Panics
    /// When no node has this id (a mistake in the test).
    pub fn set_duration(&mut self, id: &str, duration: NodeDuration) {
        let v = self.node(id);
        self.durations[v] = duration;
    }
}

#[derive(Debug, Clone, PartialEq)]
enum StartSpec {
    Sequential,
    Absolute(f64),
    Relative {
        target: String,
        mode: AnchorMode,
        offset_ms: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum LengthSpec {
    Natural,
    Duration(f64),
    EndAt(f64),
    EndRelative {
        target: String,
        mode: AnchorMode,
        offset_ms: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum ItemKindSpec {
    Cue {
        text: String,
        speaker: Option<String>,
        caption: Option<String>,
        allow_overlap: bool,
    },
    Pause,
    Clip {
        source: String,
        trim_start_ms: f64,
        length: LengthSpec,
        looped: bool,
        loop_crossfade_ms: f64,
        fade_in_ms: f64,
        fade_out_ms: f64,
        fade_curve: FadeCurve,
        crossfade_ms: f64,
        shorthand: bool,
    },
}

/// One cue, pause or clip under construction.
#[derive(Debug, Clone, PartialEq)]
struct ItemSpec {
    id: Option<String>,
    start: StartSpec,
    /// Cue audio length, pause length, or whole clip source length (ms).
    length_ms: f64,
    source: DurationSource,
    kind: ItemKindSpec,
}

/// Chainable modifiers for the item just added. Clip-only modifiers are ignored on cues and pauses;
/// start modifiers are ignored on pauses (which are always sequential).
#[derive(Debug)]
pub struct ItemHandle<'a> {
    item: &'a mut ItemSpec,
}

impl ItemHandle<'_> {
    fn relative(self, target: &str, mode: AnchorMode, offset_ms: f64) -> Self {
        self.item.start = StartSpec::Relative {
            target: target.to_string(),
            mode,
            offset_ms,
        };
        self
    }

    fn clip(self, edit: impl FnOnce(&mut ItemKindSpec)) -> Self {
        if matches!(self.item.kind, ItemKindSpec::Clip { .. }) {
            edit(&mut self.item.kind);
        }
        self
    }

    /// `start_at_ms`.
    pub fn at(self, ms: f64) -> Self {
        self.item.start = StartSpec::Absolute(ms);
        self
    }

    /// `start_relative: {after_cue: target, offset_ms}`.
    pub fn after(self, target: &str, offset_ms: f64) -> Self {
        self.relative(target, AnchorMode::After, offset_ms)
    }

    /// `start_relative: {with_cue: target, offset_ms}`.
    pub fn with(self, target: &str, offset_ms: f64) -> Self {
        self.relative(target, AnchorMode::With, offset_ms)
    }

    /// `start_relative: {before_cue: target, offset_ms}` (the item ends at `start(target) + offset`).
    pub fn before(self, target: &str, offset_ms: f64) -> Self {
        self.relative(target, AnchorMode::Before, offset_ms)
    }

    /// Where the duration table entry says the length came from (default: `exact` for cues, `source` for
    /// clips).
    pub fn duration_source(self, source: DurationSource) -> Self {
        self.item.source = source;
        self
    }

    /// Cue `speaker`.
    pub fn speaker(self, name: &str) -> Self {
        if let ItemKindSpec::Cue { speaker, .. } = &mut self.item.kind {
            *speaker = Some(name.to_string());
        }
        self
    }

    /// Cue `caption`.
    pub fn caption(self, text: &str) -> Self {
        if let ItemKindSpec::Cue { caption, .. } = &mut self.item.kind {
            *caption = Some(text.to_string());
        }
        self
    }

    /// Cue `allow_overlap: true`.
    pub fn allow_overlap(self) -> Self {
        if let ItemKindSpec::Cue { allow_overlap, .. } = &mut self.item.kind {
            *allow_overlap = true;
        }
        self
    }

    /// Clip `trim_start_ms`.
    pub fn trim_start(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { trim_start_ms, .. } = k {
                *trim_start_ms = ms;
            }
        })
    }

    /// Clip `duration_ms`.
    pub fn length(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { length, .. } = k {
                *length = LengthSpec::Duration(ms);
            }
        })
    }

    /// Clip `end_at_ms`.
    pub fn end_at(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { length, .. } = k {
                *length = LengthSpec::EndAt(ms);
            }
        })
    }

    fn end_relative(self, target: &str, mode: AnchorMode, offset_ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { length, .. } = k {
                *length = LengthSpec::EndRelative {
                    target: target.to_string(),
                    mode,
                    offset_ms,
                };
            }
        })
    }

    /// Clip `end_relative: {after_cue: target, offset_ms}`.
    pub fn end_after(self, target: &str, offset_ms: f64) -> Self {
        self.end_relative(target, AnchorMode::After, offset_ms)
    }

    /// Clip `end_relative: {with_cue: target, offset_ms}`.
    pub fn end_with(self, target: &str, offset_ms: f64) -> Self {
        self.end_relative(target, AnchorMode::With, offset_ms)
    }

    /// Clip `end_relative: {before_cue: ...}` — invalid in the DSL; only for defensive tests.
    pub fn end_before(self, target: &str, offset_ms: f64) -> Self {
        self.end_relative(target, AnchorMode::Before, offset_ms)
    }

    /// Clip `loop: true`.
    pub fn looped(self) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { looped, .. } = k {
                *looped = true;
            }
        })
    }

    /// Clip `loop_crossfade_ms`.
    pub fn loop_crossfade(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip {
                loop_crossfade_ms, ..
            } = k
            {
                *loop_crossfade_ms = ms;
            }
        })
    }

    /// Clip `fade_in_ms`.
    pub fn fade_in(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { fade_in_ms, .. } = k {
                *fade_in_ms = ms;
            }
        })
    }

    /// Clip `fade_out_ms`.
    pub fn fade_out(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { fade_out_ms, .. } = k {
                *fade_out_ms = ms;
            }
        })
    }

    /// Clip `fade_curve`.
    pub fn fade_curve(self, curve: FadeCurve) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { fade_curve, .. } = k {
                *fade_curve = curve;
            }
        })
    }

    /// Clip `crossfade_ms`.
    pub fn crossfade(self, ms: f64) -> Self {
        self.clip(|k| {
            if let ItemKindSpec::Clip { crossfade_ms, .. } = k {
                *crossfade_ms = ms;
            }
        })
    }
}

/// A track under construction (see [`ProgramBuilder::speech`], [`ProgramBuilder::music`],
/// [`ProgramBuilder::sfx`]).
#[derive(Debug, Clone, PartialEq)]
pub struct TrackBuilder {
    id: String,
    kind: TrackKind,
    mute: bool,
    start: StartSpec,
    ducking: Option<(Vec<String>, f64)>,
    items: Vec<ItemSpec>,
}

impl TrackBuilder {
    fn push(&mut self, item: ItemSpec) -> ItemHandle<'_> {
        self.items.push(item);
        let last = self.items.len() - 1;
        ItemHandle {
            item: &mut self.items[last],
        }
    }

    fn cue_spec(id: Option<&str>, text: &str, duration_ms: f64) -> ItemSpec {
        ItemSpec {
            id: id.map(str::to_string),
            start: StartSpec::Sequential,
            length_ms: duration_ms,
            source: DurationSource::Exact,
            kind: ItemKindSpec::Cue {
                text: text.to_string(),
                speaker: None,
                caption: None,
                allow_overlap: false,
            },
        }
    }

    fn clip_spec(id: Option<&str>, source: &str, source_ms: f64, shorthand: bool) -> ItemSpec {
        ItemSpec {
            id: id.map(str::to_string),
            start: StartSpec::Sequential,
            length_ms: source_ms,
            source: DurationSource::Source,
            kind: ItemKindSpec::Clip {
                source: source.to_string(),
                trim_start_ms: 0.0,
                length: LengthSpec::Natural,
                looped: false,
                loop_crossfade_ms: 0.0,
                fade_in_ms: 0.0,
                fade_out_ms: 0.0,
                fade_curve: FadeCurve::EqualPower,
                crossfade_ms: 0.0,
                shorthand,
            },
        }
    }

    /// Add a cue with an explicit id whose (trimmed) audio lasts `duration_ms`.
    pub fn cue(&mut self, id: &str, text: &str, duration_ms: f64) -> ItemHandle<'_> {
        self.push(Self::cue_spec(Some(id), text, duration_ms))
    }

    /// Add a cue with a generated id (`<track>.cue<n>`).
    pub fn auto_cue(&mut self, text: &str, duration_ms: f64) -> ItemHandle<'_> {
        self.push(Self::cue_spec(None, text, duration_ms))
    }

    /// Add a pause (generated id `<track>.pause<n>`).
    pub fn pause(&mut self, ms: f64) -> ItemHandle<'_> {
        self.push(ItemSpec {
            id: None,
            start: StartSpec::Sequential,
            length_ms: ms,
            source: DurationSource::Fixed,
            kind: ItemKindSpec::Pause,
        })
    }

    /// Add a clip (from a `clips` list) whose whole source lasts `source_ms`.
    pub fn clip(&mut self, id: &str, source: &str, source_ms: f64) -> ItemHandle<'_> {
        self.push(Self::clip_spec(Some(id), source, source_ms, false))
    }

    /// Add a clip with a generated id (`<track>.clip<n>`).
    pub fn auto_clip(&mut self, source: &str, source_ms: f64) -> ItemHandle<'_> {
        self.push(Self::clip_spec(None, source, source_ms, false))
    }

    /// Add the track-level single-clip shorthand (the clip's id is the track id).
    pub fn shorthand(&mut self, source: &str, source_ms: f64) -> ItemHandle<'_> {
        let id = self.id.clone();
        self.push(Self::clip_spec(Some(&id), source, source_ms, true))
    }

    /// Track `start_at_ms`.
    pub fn start_at(&mut self, ms: f64) -> &mut Self {
        self.start = StartSpec::Absolute(ms);
        self
    }

    /// Track `start_relative: {after_cue: target, offset_ms}`.
    pub fn start_after(&mut self, target: &str, offset_ms: f64) -> &mut Self {
        self.start = StartSpec::Relative {
            target: target.to_string(),
            mode: AnchorMode::After,
            offset_ms,
        };
        self
    }

    /// Track `start_relative: {with_cue: target, offset_ms}`.
    pub fn start_with(&mut self, target: &str, offset_ms: f64) -> &mut Self {
        self.start = StartSpec::Relative {
            target: target.to_string(),
            mode: AnchorMode::With,
            offset_ms,
        };
        self
    }

    /// Track `start_relative: {before_cue: target, offset_ms}`.
    pub fn start_before(&mut self, target: &str, offset_ms: f64) -> &mut Self {
        self.start = StartSpec::Relative {
            target: target.to_string(),
            mode: AnchorMode::Before,
            offset_ms,
        };
        self
    }

    /// Track `mute: true`.
    pub fn mute(&mut self) -> &mut Self {
        self.mute = true;
        self
    }

    /// Track `ducking: {trigger_tracks, reduction_db}` (other ducking fields get their defaults).
    pub fn ducked_by(&mut self, triggers: &[&str], reduction_db: f64) -> &mut Self {
        self.ducking = Some((
            triggers.iter().map(|t| t.to_string()).collect(),
            reduction_db,
        ));
        self
    }
}

/// Builder for a lowered [`Program`] plus duration table.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgramBuilder {
    sample_rate: u32,
    title: Option<String>,
    policy: OverlapPolicy,
    min_gap_ms: f64,
    tolerance_ms: f64,
    tail_ms: Option<f64>,
    duration_ms: Option<f64>,
    tracks: Vec<TrackBuilder>,
}

impl Default for ProgramBuilder {
    fn default() -> Self {
        ProgramBuilder {
            sample_rate: 48_000,
            title: None,
            policy: OverlapPolicy::Push,
            min_gap_ms: 250.0,
            tolerance_ms: 20.0,
            tail_ms: None,
            duration_ms: None,
            tracks: Vec::new(),
        }
    }
}

impl ProgramBuilder {
    /// A builder with lowering's defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// `output.sample_rate` (default 48000).
    pub fn sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// `title`.
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }

    /// `timing.speech_overlap` (default push).
    pub fn overlap_policy(mut self, policy: OverlapPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// `timing.min_gap_ms` (default 250).
    pub fn min_gap_ms(mut self, ms: f64) -> Self {
        self.min_gap_ms = ms;
        self
    }

    /// `timing.overlap_tolerance_ms` (default 20).
    pub fn overlap_tolerance_ms(mut self, ms: f64) -> Self {
        self.tolerance_ms = ms;
        self
    }

    /// `output.tail_ms`.
    pub fn tail_ms(mut self, ms: f64) -> Self {
        self.tail_ms = Some(ms);
        self
    }

    /// `output.duration_ms`.
    pub fn duration_ms(mut self, ms: f64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    fn track(mut self, id: &str, kind: TrackKind, build: impl FnOnce(&mut TrackBuilder)) -> Self {
        let mut track = TrackBuilder {
            id: id.to_string(),
            kind,
            mute: false,
            start: StartSpec::Sequential,
            ducking: None,
            items: Vec::new(),
        };
        build(&mut track);
        self.tracks.push(track);
        self
    }

    /// Add a speech track.
    pub fn speech(self, id: &str, build: impl FnOnce(&mut TrackBuilder)) -> Self {
        self.track(id, TrackKind::Speech, build)
    }

    /// Add a music track.
    pub fn music(self, id: &str, build: impl FnOnce(&mut TrackBuilder)) -> Self {
        self.track(id, TrackKind::Music, build)
    }

    /// Add an sfx track.
    pub fn sfx(self, id: &str, build: impl FnOnce(&mut TrackBuilder)) -> Self {
        self.track(id, TrackKind::Sfx, build)
    }

    /// Build the program and its duration table.
    ///
    /// # Panics
    /// When an anchor or ducking trigger references an unknown id, or ids collide (mistakes in the test);
    /// use [`ProgramBuilder::try_build`] to get the error instead.
    pub fn build(self) -> TestTimeline {
        self.try_build()
            .unwrap_or_else(|e| panic!("invalid test timeline: {e}"))
    }

    /// Build the program and its duration table, or describe why the description is inconsistent.
    pub fn try_build(self) -> Result<TestTimeline, String> {
        let sample_rate = self.sample_rate;
        let frames = |ms: f64| u64::try_from(ms_to_frames(ms, sample_rate).max(0)).unwrap_or(0);

        // Pass 1: node ids (document order) and the id namespace.
        let mut ids: Vec<(String, AnchorTarget)> = Vec::new();
        let mut node_ids: Vec<Vec<String>> = Vec::new();
        let mut next_node: NodeId = 0;
        for (t, track) in self.tracks.iter().enumerate() {
            ids.push((track.id.clone(), AnchorTarget::Track(t)));
            let (mut cues, mut clips, mut pauses) = (0, 0, 0);
            let mut names = Vec::with_capacity(track.items.len());
            for item in &track.items {
                let (id, referenceable) = match &item.kind {
                    ItemKindSpec::Cue { .. } => {
                        cues += 1;
                        (
                            item.id
                                .clone()
                                .unwrap_or_else(|| format!("{}.cue{cues}", track.id)),
                            true,
                        )
                    }
                    ItemKindSpec::Pause => {
                        pauses += 1;
                        (format!("{}.pause{pauses}", track.id), false)
                    }
                    ItemKindSpec::Clip { shorthand, .. } => {
                        clips += 1;
                        let id = item
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("{}.clip{clips}", track.id));
                        // The shorthand clip shares the track id, which already refers to the track.
                        (id, !*shorthand)
                    }
                };
                if referenceable {
                    ids.push((id.clone(), AnchorTarget::Node(next_node)));
                }
                names.push(id);
                next_node += 1;
            }
            node_ids.push(names);
        }
        for (i, (id, _)) in ids.iter().enumerate() {
            if ids[..i].iter().any(|(other, _)| other == id) {
                return Err(format!("duplicate id `{id}`"));
            }
        }
        let lookup = |id: &str| -> Result<AnchorTarget, String> {
            ids.iter()
                .find(|(k, _)| k == id)
                .map(|(_, target)| *target)
                .ok_or_else(|| format!("unknown id `{id}`"))
        };
        let anchor =
            |target: &str, mode: AnchorMode, offset_ms: f64| -> Result<AnchorRule, String> {
                Ok(AnchorRule {
                    target: lookup(target)?,
                    target_id: target.to_string(),
                    mode,
                    offset_ms,
                })
            };
        let start_rule = |spec: &StartSpec| -> Result<StartRule, String> {
            Ok(match spec {
                StartSpec::Sequential => StartRule::Sequential,
                StartSpec::Absolute(ms) => StartRule::Absolute { ms: *ms },
                StartSpec::Relative {
                    target,
                    mode,
                    offset_ms,
                } => StartRule::Relative(anchor(target, *mode, *offset_ms)?),
            })
        };

        // Pass 2: nodes, tracks and durations.
        let base_dir = PathBuf::from(".");
        let mut nodes = Vec::new();
        let mut tracks = Vec::new();
        let mut durations = Vec::new();
        for (t, track) in self.tracks.iter().enumerate() {
            let track_path = format!("tracks[{t}]");
            let mut members = Vec::with_capacity(track.items.len());
            for (i, item) in track.items.iter().enumerate() {
                let id = node_ids[t][i].clone();
                let (kind, path, duration) = match &item.kind {
                    ItemKindSpec::Cue {
                        text,
                        speaker,
                        caption,
                        allow_overlap,
                    } => (
                        NodeKind::Cue(CueIr {
                            text: text.clone(),
                            caption: caption.clone(),
                            subtitles: true,
                            speaker: speaker.clone(),
                            speaker_label: speaker.clone().unwrap_or_else(|| track.id.clone()),
                            voice: VoiceIr {
                                provider: "mock".to_string(),
                                provider_type: ProviderType::Mock,
                                voice_id: Some("mock".to_string()),
                                model: None,
                                language: None,
                                settings: Default::default(),
                            },
                            gain: 1.0,
                            allow_overlap: *allow_overlap,
                        }),
                        format!("{track_path}.sequence[{i}]"),
                        NodeDuration::new(frames(item.length_ms), item.source),
                    ),
                    ItemKindSpec::Pause => (
                        NodeKind::Pause {
                            duration_ms: item.length_ms,
                        },
                        format!("{track_path}.sequence[{i}]"),
                        NodeDuration::fixed(frames(item.length_ms)),
                    ),
                    ItemKindSpec::Clip {
                        source,
                        trim_start_ms,
                        length,
                        looped,
                        loop_crossfade_ms,
                        fade_in_ms,
                        fade_out_ms,
                        fade_curve,
                        crossfade_ms,
                        shorthand,
                    } => (
                        NodeKind::Clip(ClipIr {
                            source: source.clone(),
                            source_path: base_dir.join(source),
                            trim_start_ms: *trim_start_ms,
                            length: match length {
                                LengthSpec::Natural => ClipLength::Natural,
                                LengthSpec::Duration(ms) => ClipLength::Duration { ms: *ms },
                                LengthSpec::EndAt(ms) => ClipLength::EndAbsolute { ms: *ms },
                                LengthSpec::EndRelative {
                                    target,
                                    mode,
                                    offset_ms,
                                } => ClipLength::EndRelative(anchor(target, *mode, *offset_ms)?),
                            },
                            looped: *looped,
                            loop_crossfade_ms: *loop_crossfade_ms,
                            fade_in_ms: *fade_in_ms,
                            fade_out_ms: *fade_out_ms,
                            fade_curve: *fade_curve,
                            crossfade_ms: *crossfade_ms,
                            gain: 1.0,
                            shorthand: *shorthand,
                        }),
                        if *shorthand {
                            track_path.clone()
                        } else {
                            format!("{track_path}.clips[{i}]")
                        },
                        NodeDuration::new(frames(item.length_ms), item.source),
                    ),
                };
                members.push(nodes.len());
                nodes.push(Node {
                    id,
                    auto_id: item.id.is_none(),
                    track: t,
                    index_in_track: i,
                    path,
                    start: start_rule(&item.start)?,
                    kind,
                });
                durations.push(duration);
            }
            let ducking = match &track.ducking {
                None => None,
                Some((triggers, reduction_db)) => {
                    let triggers = triggers
                        .iter()
                        .map(|id| match lookup(id)? {
                            AnchorTarget::Track(t) => Ok(t),
                            AnchorTarget::Node(_) => {
                                Err(format!("ducking trigger `{id}` is not a track"))
                            }
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    Some(DuckingConfig {
                        triggers,
                        reduction_db: reduction_db.abs(),
                        attack_ms: 80.0,
                        release_ms: 400.0,
                        hold_ms: 250.0,
                        lookahead_ms: 80.0,
                        threshold_db: -40.0,
                        mode: DuckingMode::Sidechain,
                    })
                }
            };
            tracks.push(TrackIr {
                id: track.id.clone(),
                kind: track.kind,
                path: track_path,
                gain: 1.0,
                pan: 0.0,
                mute: track.mute,
                normalize_lufs: (track.kind != TrackKind::Sfx).then_some(-20.0),
                trim_silence: (track.kind == TrackKind::Speech).then_some(TrimConfig {
                    threshold_db: -50.0,
                    padding_ms: 30.0,
                }),
                ducking,
                start: start_rule(&track.start)?,
                nodes: members,
            });
        }

        let mut program = Program {
            version: "1.0".to_string(),
            title: self.title.clone(),
            base_dir,
            output: OutputConfig {
                format: OutputFormat::Wav,
                format_explicit: false,
                path: None,
                sample_rate,
                channels: 2,
                bitrate_bps: None,
                bit_depth: 24,
                loudness_preset: LoudnessPreset::Podcast,
                target_lufs: -16.0,
                true_peak_db: -1.0,
                normalize: true,
                subtitle_formats: Vec::new(),
                export_stems: false,
                export_manifest: false,
                tail_ms: self.tail_ms,
                duration_ms: self.duration_ms,
                dither: true,
            },
            subtitles: SubtitleConfig {
                max_chars_per_line: 42,
                max_lines: 2,
                max_duration_ms: 6000.0,
                min_duration_ms: 800.0,
                speaker_labels: false,
            },
            timing: TimingConfig {
                speech_overlap: self.policy,
                min_gap_ms: self.min_gap_ms,
                overlap_tolerance_ms: self.tolerance_ms,
            },
            mix: MixConfig {
                reference_lufs: -20.0,
            },
            tracks,
            nodes,
            providers: Default::default(),
            ids: Default::default(),
        };
        for (id, target) in ids {
            program.ids.insert(id, target);
        }
        if program.nodes.iter().any(Node::is_cue) {
            program.providers.insert(
                "mock".to_string(),
                ProviderInstance {
                    name: "mock".to_string(),
                    provider_type: ProviderType::Mock,
                    configured: false,
                    api_key_env: None,
                    base_url: None,
                    model: None,
                    output_format: None,
                    max_concurrency: 8,
                    timeout_ms: 120_000,
                    chars_per_second: None,
                    words_per_minute: None,
                    settings: Default::default(),
                    command: None,
                    stdin: None,
                    voices_dir: None,
                },
            );
        }
        Ok(TestTimeline { program, durations })
    }
}
