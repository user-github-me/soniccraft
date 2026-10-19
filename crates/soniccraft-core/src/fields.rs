//! The one place that lists which keys are valid in which object of the DSL, and what kind of value
//! each key takes.
//!
//! The parser uses these tables for unknown-key detection with did-you-mean candidates "at that location"
//! and for precise type-error messages; lowering uses the track subsets for `field_not_applicable`. A unit
//! test in [`crate::schema`] asserts that every table equals the `properties` of the matching object in
//! the generated JSON Schema, so the tables cannot drift from [`crate::model`].

/// An object kind of the DSL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Obj {
    Timeline,
    Output,
    Subtitles,
    Timing,
    Mix,
    Voice,
    Provider,
    Track,
    SequenceItem,
    Clip,
    Anchor,
    Ducking,
    Normalize,
    TrimSilence,
}

/// What kind of value a key takes (used to describe expectations in type errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Str,
    Num,
    UInt,
    Bool,
    Millis,
    Bitrate,
    Version,
    /// A nested object of the given kind.
    Obj(Obj),
    /// A list of strings.
    StrList,
    /// A list of objects of the given kind (decoded element by element).
    ObjList(Obj),
    /// A map from user-chosen names to objects of the given kind (`speakers`, `providers`).
    NameMap(Obj),
    /// A free-form object (`settings`).
    FreeMap,
    /// `true`, `false` or `{target_lufs}`.
    Normalize,
    /// `true`, `false` or `{threshold_db, padding_ms}`.
    TrimSilence,
    /// `true`, `false`, a format name or a list of format names.
    SubtitleExport,
}

/// A key and the kind of value it takes.
pub(crate) type Field = (&'static str, Kind);

use Kind::*;

const TIMELINE: &[Field] = &[
    ("version", Version),
    ("title", Str),
    ("description", Str),
    ("output", Kind::Obj(Obj::Output)),
    ("subtitles", Kind::Obj(Obj::Subtitles)),
    ("timing", Kind::Obj(Obj::Timing)),
    ("mix", Kind::Obj(Obj::Mix)),
    ("defaults", Kind::Obj(Obj::Voice)),
    ("speakers", NameMap(Obj::Voice)),
    ("providers", NameMap(Obj::Provider)),
    ("tracks", ObjList(Obj::Track)),
];

const OUTPUT: &[Field] = &[
    ("format", Str),
    ("path", Str),
    ("sample_rate", UInt),
    ("channels", UInt),
    ("bitrate", Bitrate),
    ("bit_depth", UInt),
    ("loudness_preset", Str),
    ("target_lufs", Num),
    ("true_peak_db", Num),
    ("normalize", Bool),
    ("export_subtitles", SubtitleExport),
    ("export_stems", Bool),
    ("export_manifest", Bool),
    ("tail_ms", Millis),
    ("duration_ms", Millis),
    ("dither", Bool),
];

const SUBTITLES: &[Field] = &[
    ("max_chars_per_line", UInt),
    ("max_lines", UInt),
    ("max_duration_ms", Millis),
    ("min_duration_ms", Millis),
    ("speaker_labels", Bool),
];

const TIMING: &[Field] = &[
    ("speech_overlap", Str),
    ("min_gap_ms", Millis),
    ("overlap_tolerance_ms", Millis),
];

const MIX: &[Field] = &[("reference_lufs", Num)];

const VOICE: &[Field] = &[
    ("provider", Str),
    ("voice_id", Str),
    ("model", Str),
    ("language", Str),
    ("settings", FreeMap),
    ("gain_db", Num),
    ("label", Str),
];

const PROVIDER: &[Field] = &[
    ("type", Str),
    ("api_key_env", Str),
    ("base_url", Str),
    ("model", Str),
    ("output_format", Str),
    ("max_concurrency", UInt),
    ("timeout_ms", UInt),
    ("chars_per_second", Num),
    ("words_per_minute", Num),
    ("settings", FreeMap),
    ("command", StrList),
    ("stdin", Str),
    ("voices_dir", Str),
];

/// Track keys valid on every track type (DESIGN.md §2.9).
pub(crate) const TRACK_COMMON: &[Field] = &[
    ("id", Str),
    ("type", Str),
    ("volume", Num),
    ("gain_db", Num),
    ("pan", Num),
    ("mute", Bool),
    ("normalize", Normalize),
    ("start_at_ms", Millis),
    ("start_relative", Kind::Obj(Obj::Anchor)),
    ("ducking", Kind::Obj(Obj::Ducking)),
];

/// Track keys valid only on speech tracks (DESIGN.md §2.10).
pub(crate) const TRACK_SPEECH: &[Field] = &[
    ("sequence", ObjList(Obj::SequenceItem)),
    ("speaker", Str),
    ("provider", Str),
    ("voice_id", Str),
    ("model", Str),
    ("language", Str),
    ("settings", FreeMap),
    ("trim_silence", TrimSilence),
];

/// Track keys valid only on music/sfx tracks (DESIGN.md §2.11): the single-clip shorthand and `clips`.
pub(crate) const TRACK_AUDIO: &[Field] = &[
    ("source", Str),
    ("loop", Bool),
    ("loop_crossfade_ms", Millis),
    ("fade_in_ms", Millis),
    ("fade_out_ms", Millis),
    ("fade_curve", Str),
    ("trim_start_ms", Millis),
    ("duration_ms", Millis),
    ("end_at_ms", Millis),
    ("end_relative", Kind::Obj(Obj::Anchor)),
    ("clips", ObjList(Obj::Clip)),
];

/// Pause keys of a sequence item.
pub(crate) const PAUSE_KEYS: &[&str] = &["pause_duration", "pause"];

const SEQUENCE_ITEM: &[Field] = &[
    ("pause_duration", Millis),
    ("pause", Millis),
    ("id", Str),
    ("text", Str),
    ("caption", Str),
    ("subtitles", Bool),
    ("speaker", Str),
    ("provider", Str),
    ("voice_id", Str),
    ("model", Str),
    ("language", Str),
    ("settings", FreeMap),
    ("start_at_ms", Millis),
    ("start_relative", Kind::Obj(Obj::Anchor)),
    ("volume", Num),
    ("gain_db", Num),
    ("allow_overlap", Bool),
];

const CLIP: &[Field] = &[
    ("id", Str),
    ("source", Str),
    ("start_at_ms", Millis),
    ("start_relative", Kind::Obj(Obj::Anchor)),
    ("trim_start_ms", Millis),
    ("duration_ms", Millis),
    ("end_at_ms", Millis),
    ("end_relative", Kind::Obj(Obj::Anchor)),
    ("loop", Bool),
    ("loop_crossfade_ms", Millis),
    ("fade_in_ms", Millis),
    ("fade_out_ms", Millis),
    ("fade_curve", Str),
    ("crossfade_ms", Millis),
    ("volume", Num),
    ("gain_db", Num),
];

const ANCHOR: &[Field] = &[
    ("after_cue", Str),
    ("with_cue", Str),
    ("before_cue", Str),
    ("offset_ms", Millis),
];

const DUCKING: &[Field] = &[
    ("trigger_tracks", StrList),
    ("reduction_db", Num),
    ("attack_ms", Millis),
    ("release_ms", Millis),
    ("hold_ms", Millis),
    ("lookahead_ms", Millis),
    ("threshold_db", Num),
    ("mode", Str),
];

const NORMALIZE: &[Field] = &[("target_lufs", Num)];

const TRIM_SILENCE: &[Field] = &[("threshold_db", Num), ("padding_ms", Millis)];

/// Every object kind, for iteration (misplaced-key hints, schema equality test).
pub(crate) const ALL_OBJS: &[Obj] = &[
    Obj::Timeline,
    Obj::Output,
    Obj::Subtitles,
    Obj::Timing,
    Obj::Mix,
    Obj::Voice,
    Obj::Provider,
    Obj::Track,
    Obj::SequenceItem,
    Obj::Clip,
    Obj::Anchor,
    Obj::Ducking,
    Obj::Normalize,
    Obj::TrimSilence,
];

/// The fields of an object kind. Track fields are the concatenation of the common, speech and audio
/// subsets.
pub(crate) fn fields(obj: Obj) -> Vec<Field> {
    match obj {
        Obj::Timeline => TIMELINE.to_vec(),
        Obj::Output => OUTPUT.to_vec(),
        Obj::Subtitles => SUBTITLES.to_vec(),
        Obj::Timing => TIMING.to_vec(),
        Obj::Mix => MIX.to_vec(),
        Obj::Voice => VOICE.to_vec(),
        Obj::Provider => PROVIDER.to_vec(),
        Obj::Track => [TRACK_COMMON, TRACK_SPEECH, TRACK_AUDIO].concat(),
        Obj::SequenceItem => SEQUENCE_ITEM.to_vec(),
        Obj::Clip => CLIP.to_vec(),
        Obj::Anchor => ANCHOR.to_vec(),
        Obj::Ducking => DUCKING.to_vec(),
        Obj::Normalize => NORMALIZE.to_vec(),
        Obj::TrimSilence => TRIM_SILENCE.to_vec(),
    }
}

/// The key names of an object kind, in declaration order.
pub(crate) fn names(obj: Obj) -> Vec<&'static str> {
    fields(obj).into_iter().map(|(name, _)| name).collect()
}

/// The kind of value `key` takes in `obj`, if `key` is a field of `obj`.
pub(crate) fn kind_of(obj: Obj, key: &str) -> Option<Kind> {
    fields(obj)
        .into_iter()
        .find(|(name, _)| *name == key)
        .map(|(_, kind)| kind)
}

/// Name of the `$defs` entry describing `obj` in the generated schema (None for the root).
pub(crate) fn schema_def(obj: Obj) -> Option<&'static str> {
    Some(match obj {
        Obj::Timeline => return None,
        Obj::Output => "OutputSpec",
        Obj::Subtitles => "SubtitleSpec",
        Obj::Timing => "TimingSpec",
        Obj::Mix => "MixSpec",
        Obj::Voice => "VoiceSpec",
        Obj::Provider => "ProviderSpec",
        Obj::Track => "Track",
        Obj::SequenceItem => "SequenceItem",
        Obj::Clip => "Clip",
        Obj::Anchor => "Anchor",
        Obj::Ducking => "DuckingSpec",
        Obj::Normalize => "NormalizeSpec",
        Obj::TrimSilence => "TrimSilenceSpec",
    })
}

/// Where `key` belongs, for "this key belongs in …" hints: [`location_label`] of `obj`, except for `label`,
/// which only works in `speakers` entries (in `defaults` it is ignored with a warning).
pub(crate) fn key_location(obj: Obj, key: &str) -> &'static str {
    if obj == Obj::Voice && key == "label" {
        "a `speakers` entry (label is the subtitle name of a speaker)"
    } else {
        location_label(obj)
    }
}

// ---- synonyms --------------------------------------------------------------------------------------------

/// What a semantic synonym of a DSL key suggests (checked before edit distance, see [`synonym`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Synonym {
    /// Another key of the same object (`file` → `source`).
    Key(&'static str),
    /// An anchor: `<field>: {<key>: <value>}` (`start_after: a2` → `start_relative: {after_cue: a2}`).
    Anchor(&'static str, &'static str),
    /// A provider-specific voice setting written next to the voice fields (`speed: 1.1`).
    Setting,
    /// A pause written as a cue field or with another name (`wait: 1s`, `pause_ms: 300`).
    Pause,
    /// `gain`: either `gain_db` or the linear `volume`.
    Gain,
    /// A track or item offset: `trim_start_ms` (into the source) or a start time.
    Offset,
    /// An anchor target without its edge (`cue: a1`, `edge: end`).
    AnchorTarget,
    /// The provider written at the top level (`provider: mock`): `defaults: {provider: …}`.
    DefaultsField(&'static str),
    /// `subtitles: true` / `captions: [vtt]` outside `output`: `output.export_subtitles`.
    ExportSubtitles,
    /// An audio file on a speech track.
    AudioOnSpeech,
}

/// Where a synonym applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    In(Obj),
    /// Music and sfx tracks (and tracks whose type is unknown).
    AudioTrack,
    /// Speech tracks (and tracks whose type is unknown).
    SpeechTrack,
    /// Speech tracks only (known type).
    KnownSpeechTrack,
    /// Entries of `speakers` (not `defaults`).
    Speaker,
}

use crate::model::TrackKindName;

/// Synonyms by location. Keys are compared after [`crate::diagnostics::normalize_key`].
const SYNONYMS: &[(&[&str], &[Place], Synonym)] = {
    // `Kind::*` (imported above) has a variant named `Obj`; name the enum by its full path.
    use crate::fields::Obj as O;
    use Place::*;
    use Synonym::*;
    const AUDIO: &[Place] = &[AudioTrack, In(O::Clip)];
    const ITEMS: &[Place] = &[In(O::Track), In(O::SequenceItem), In(O::Clip)];
    const VOICE: &[Place] = &[In(O::Voice), In(O::SequenceItem), SpeechTrack];
    &[
        // ---- audio files, clip timing -------------------------------------------------------------------
        (
            &[
                "file",
                "path",
                "src",
                "audio",
                "url",
                "uri",
                "filename",
                "file_name",
                "sound",
                "sample",
                "file_path",
                "filepath",
                "audio_file",
                "audio_path",
                "sound_file",
                "asset",
                "media",
            ],
            AUDIO,
            Key("source"),
        ),
        (
            &[
                "file",
                "src",
                "audio",
                "filename",
                "sound",
                "sample",
                "file_path",
                "audio_file",
                "asset",
            ],
            &[KnownSpeechTrack],
            AudioOnSpeech,
        ),
        (
            &[
                "end", "end_time", "stop", "stop_at", "ends_at", "end_ms", "until_ms", "end_at",
            ],
            AUDIO,
            Key("end_at_ms"),
        ),
        (
            &[
                "length",
                "duration",
                "dur",
                "len",
                "play_length",
                "play_duration",
                "duration_s",
            ],
            AUDIO,
            Key("duration_ms"),
        ),
        (
            &[
                "repeat",
                "looping",
                "loops",
                "loop_count",
                "repeat_count",
                "looped",
                "is_loop",
            ],
            AUDIO,
            Key("loop"),
        ),
        (
            &["fade", "fadein", "fade_in_time", "fade_in_duration"],
            AUDIO,
            Key("fade_in_ms"),
        ),
        (
            &["fadeout", "fade_out_time", "fade_out_duration"],
            AUDIO,
            Key("fade_out_ms"),
        ),
        (
            &["crossfade", "cross_fade", "xfade", "crossfade_time"],
            &[In(O::Clip)],
            Key("crossfade_ms"),
        ),
        (
            &[
                "offset",
                "seek",
                "skip",
                "start_offset",
                "source_offset",
                "in_point",
                "trim",
                "trim_start",
            ],
            AUDIO,
            Offset,
        ),
        (
            &["curve", "fade_type", "fade_shape", "fade_mode"],
            AUDIO,
            Key("fade_curve"),
        ),
        // ---- starts and anchors ----------------------------------------------------------------------------
        (
            &[
                "start",
                "start_time",
                "begin",
                "start_ms",
                "starts_at",
                "start_at",
                "at",
                "time",
                "start_s",
            ],
            ITEMS,
            Key("start_at_ms"),
        ),
        (
            &[
                "start_after",
                "after",
                "follows",
                "follow",
                "starts_after",
                "play_after",
                "begin_after",
                "after_cue",
            ],
            ITEMS,
            Anchor("start_relative", "after_cue"),
        ),
        (
            &[
                "start_with",
                "with",
                "starts_with",
                "play_with",
                "sync_with",
                "together_with",
                "with_cue",
            ],
            ITEMS,
            Anchor("start_relative", "with_cue"),
        ),
        (
            &[
                "start_before",
                "before",
                "ends_before",
                "end_before",
                "before_cue",
            ],
            ITEMS,
            Anchor("start_relative", "before_cue"),
        ),
        (
            &["end_after", "ends_after", "until_after", "stop_after"],
            AUDIO,
            Anchor("end_relative", "after_cue"),
        ),
        (
            &["end_with", "ends_with", "until", "stop_with"],
            AUDIO,
            Anchor("end_relative", "with_cue"),
        ),
        (
            &["after", "end", "after_end", "following"],
            &[In(O::Anchor)],
            Key("after_cue"),
        ),
        (
            &["with", "start", "at_start", "together"],
            &[In(O::Anchor)],
            Key("with_cue"),
        ),
        (&["before"], &[In(O::Anchor)], Key("before_cue")),
        (
            &[
                "cue",
                "target",
                "ref",
                "id",
                "item",
                "anchor",
                "reference",
                "edge",
                "point",
                "side",
                "at",
                "mode",
            ],
            &[In(O::Anchor)],
            AnchorTarget,
        ),
        (
            &[
                "offset", "delay", "gap", "shift", "plus", "offset_s", "delay_ms",
            ],
            &[In(O::Anchor)],
            Key("offset_ms"),
        ),
        // ---- pauses -----------------------------------------------------------------------------------------
        (
            &[
                "pause_ms",
                "pause_s",
                "silence",
                "silence_ms",
                "wait",
                "wait_ms",
                "delay",
                "delay_ms",
                "gap",
                "gap_ms",
                "rest",
                "break",
                "sleep",
                "pause_for",
                "pause_length",
                "pause_time",
                "pause_secs",
            ],
            &[In(O::SequenceItem)],
            Pause,
        ),
        // ---- voices -------------------------------------------------------------------------------------------
        (
            &[
                "voice",
                "voice_name",
                "voicename",
                "tts_voice",
                "speaker_voice",
                "narrator_voice",
                "voiceid",
                "voice_preset",
                "voice_key",
            ],
            VOICE,
            Key("voice_id"),
        ),
        (
            &[
                "engine",
                "tts",
                "service",
                "vendor",
                "tts_provider",
                "backend",
            ],
            VOICE,
            Key("provider"),
        ),
        (&["lang", "locale", "language_code"], VOICE, Key("language")),
        (
            &["model_id", "tts_model", "voice_model"],
            VOICE,
            Key("model"),
        ),
        (
            &[
                "speed",
                "rate",
                "pitch",
                "stability",
                "style",
                "similarity_boost",
                "emotion",
                "instructions",
                "temperature",
                "exaggeration",
                "speaking_rate",
                "use_speaker_boost",
                "seed",
                "length_scale",
            ],
            VOICE,
            Setting,
        ),
        (
            &[
                "speaker_name",
                "character",
                "role",
                "who",
                "person",
                "narrator",
                "voice_of",
            ],
            &[In(O::SequenceItem), SpeechTrack],
            Key("speaker"),
        ),
        (
            &[
                "name",
                "display_name",
                "display",
                "title",
                "speaker_name",
                "subtitle_name",
            ],
            &[Speaker],
            Key("label"),
        ),
        (
            &["gain", "volume", "level", "loudness", "vol"],
            &[In(O::Voice)],
            Key("gain_db"),
        ),
        // ---- cues ---------------------------------------------------------------------------------------------
        (
            &[
                "content",
                "speech",
                "say",
                "line",
                "words",
                "script",
                "transcript",
                "utterance",
                "phrase",
                "sentence",
                "txt",
                "message",
                "prompt",
                "spoken",
                "spoken_text",
                "tts_text",
            ],
            &[In(O::SequenceItem)],
            Key("text"),
        ),
        (
            &[
                "subtitle",
                "subtitle_text",
                "display_text",
                "caption_text",
                "on_screen_text",
                "captions",
                "sub",
            ],
            &[In(O::SequenceItem)],
            Key("caption"),
        ),
        (
            &[
                "crosstalk",
                "overlap",
                "allow_overlaps",
                "overlapping",
                "overlaps",
            ],
            &[In(O::SequenceItem)],
            Key("allow_overlap"),
        ),
        (
            &[
                "show_subtitles",
                "in_subtitles",
                "show_in_subtitles",
                "subtitled",
            ],
            &[In(O::SequenceItem)],
            Key("subtitles"),
        ),
        // ---- tracks -------------------------------------------------------------------------------------------
        (
            &["kind", "track_type", "category"],
            &[In(O::Track)],
            Key("type"),
        ),
        (
            &["name", "track_id", "track_name"],
            &[In(O::Track)],
            Key("id"),
        ),
        (
            &[
                "cues",
                "items",
                "script",
                "lines",
                "segments",
                "dialogue",
                "dialog",
                "utterances",
                "turns",
                "steps",
                "entries",
                "content",
                "speech",
                "texts",
            ],
            &[SpeechTrack],
            Key("sequence"),
        ),
        (&["gain"], ITEMS, Gain),
        (
            &["vol", "level", "amplitude", "fader"],
            ITEMS,
            Key("volume"),
        ),
        (
            &["balance", "panning", "position", "stereo_position"],
            &[In(O::Track)],
            Key("pan"),
        ),
        (
            &["muted", "silent", "disabled"],
            &[In(O::Track)],
            Key("mute"),
        ),
        (
            &[
                "duck",
                "sidechain",
                "duck_under",
                "ducked_by",
                "duck_by",
                "duck_with",
                "auto_duck",
            ],
            &[In(O::Track)],
            Key("ducking"),
        ),
        (
            &[
                "normalise",
                "normalization",
                "normalisation",
                "loudness_normalize",
                "normalize_loudness",
            ],
            &[In(O::Track)],
            Key("normalize"),
        ),
        (
            &["trim", "silence_trim", "trim_silences", "strip_silence"],
            &[KnownSpeechTrack],
            Key("trim_silence"),
        ),
        // ---- top level ----------------------------------------------------------------------------------------
        (
            &["name", "show", "show_name", "episode", "episode_title"],
            &[In(O::Timeline)],
            Key("title"),
        ),
        (
            &["desc", "summary", "about"],
            &[In(O::Timeline)],
            Key("description"),
        ),
        (
            &["voices", "cast", "characters", "speaker", "voice_map"],
            &[In(O::Timeline)],
            Key("speakers"),
        ),
        (
            &[
                "dsl_version",
                "schema_version",
                "format_version",
                "spec_version",
            ],
            &[In(O::Timeline)],
            Key("version"),
        ),
        (
            &["provider", "tts", "engine"],
            &[In(O::Timeline)],
            DefaultsField("provider"),
        ),
        (
            &["voice", "voice_id"],
            &[In(O::Timeline)],
            DefaultsField("voice_id"),
        ),
        (
            &["export_subtitles", "captions", "export_captions"],
            &[In(O::Timeline)],
            ExportSubtitles,
        ),
        (
            &["layers", "timeline", "lanes"],
            &[In(O::Timeline)],
            Key("tracks"),
        ),
        (&["mixer", "mixing"], &[In(O::Timeline)], Key("mix")),
        // ---- output -------------------------------------------------------------------------------------------
        (
            &[
                "file",
                "filename",
                "file_name",
                "output_path",
                "out",
                "output_file",
                "filepath",
                "file_path",
                "destination",
                "dest",
            ],
            &[In(O::Output)],
            Key("path"),
        ),
        (
            &[
                "rate",
                "samplerate",
                "sr",
                "sample_rate_hz",
                "hz",
                "frequency",
                "sampling_rate",
            ],
            &[In(O::Output)],
            Key("sample_rate"),
        ),
        (
            &[
                "lufs",
                "loudness",
                "target_loudness",
                "integrated_lufs",
                "loudness_target",
                "target_lufs_db",
            ],
            &[In(O::Output)],
            Key("target_lufs"),
        ),
        (
            &[
                "preset",
                "platform",
                "normalization_preset",
                "loudness_standard",
                "standard",
                "target_platform",
                "destination_platform",
            ],
            &[In(O::Output)],
            Key("loudness_preset"),
        ),
        (
            &[
                "subtitles",
                "captions",
                "export_captions",
                "export_subtitle",
                "subtitle_formats",
                "subtitle",
                "srt",
                "vtt",
                "webvtt",
            ],
            &[In(O::Output)],
            Key("export_subtitles"),
        ),
        (
            &["stems", "export_stem", "write_stems"],
            &[In(O::Output)],
            Key("export_stems"),
        ),
        (
            &["manifest", "write_manifest"],
            &[In(O::Output)],
            Key("export_manifest"),
        ),
        (
            &[
                "peak",
                "true_peak",
                "ceiling",
                "max_peak",
                "peak_db",
                "true_peak_dbtp",
                "dbtp",
                "limit",
            ],
            &[In(O::Output)],
            Key("true_peak_db"),
        ),
        (
            &[
                "codec",
                "container",
                "type",
                "extension",
                "ext",
                "file_format",
                "audio_format",
                "file_type",
            ],
            &[In(O::Output)],
            Key("format"),
        ),
        (
            &["bits", "bitdepth", "bits_per_sample", "sample_bits"],
            &[In(O::Output)],
            Key("bit_depth"),
        ),
        (
            &["kbps", "bit_rate", "rate_kbps", "audio_bitrate"],
            &[In(O::Output)],
            Key("bitrate"),
        ),
        (
            &["mono", "stereo", "channel", "num_channels", "channel_count"],
            &[In(O::Output)],
            Key("channels"),
        ),
        (
            &[
                "length",
                "duration",
                "total_duration",
                "total_length",
                "max_duration",
            ],
            &[In(O::Output)],
            Key("duration_ms"),
        ),
        (
            &[
                "tail",
                "padding",
                "end_padding",
                "outro_silence",
                "trailing_silence",
            ],
            &[In(O::Output)],
            Key("tail_ms"),
        ),
        (
            &[
                "normalise",
                "master_normalize",
                "normalization",
                "loudness_normalize",
            ],
            &[In(O::Output)],
            Key("normalize"),
        ),
        // ---- timing, mix, subtitles, ducking --------------------------------------------------------------------
        (
            &[
                "overlap",
                "overlap_policy",
                "speech_overlap_policy",
                "on_overlap",
                "overlaps",
                "overlap_mode",
            ],
            &[In(O::Timing)],
            Key("speech_overlap"),
        ),
        (
            &["gap", "min_gap", "gap_ms", "minimum_gap", "push_gap"],
            &[In(O::Timing)],
            Key("min_gap_ms"),
        ),
        (
            &["tolerance", "overlap_tolerance"],
            &[In(O::Timing)],
            Key("overlap_tolerance_ms"),
        ),
        (
            &[
                "reference",
                "reference_level",
                "target_lufs",
                "lufs",
                "source_lufs",
                "normalize_to",
            ],
            &[In(O::Mix)],
            Key("reference_lufs"),
        ),
        (
            &[
                "chars",
                "max_chars",
                "line_length",
                "max_line_length",
                "chars_per_line",
                "width",
            ],
            &[In(O::Subtitles)],
            Key("max_chars_per_line"),
        ),
        (
            &["lines", "rows", "max_rows"],
            &[In(O::Subtitles)],
            Key("max_lines"),
        ),
        (
            &["max_duration", "max_length"],
            &[In(O::Subtitles)],
            Key("max_duration_ms"),
        ),
        (
            &["min_duration", "min_length"],
            &[In(O::Subtitles)],
            Key("min_duration_ms"),
        ),
        (
            &[
                "labels",
                "speaker_names",
                "show_speakers",
                "speakers",
                "show_speaker",
            ],
            &[In(O::Subtitles)],
            Key("speaker_labels"),
        ),
        (
            &[
                "triggers",
                "trigger",
                "trigger_track",
                "duck_on",
                "ducked_by",
                "duck_by",
                "sidechain",
                "sidechain_tracks",
                "keys",
                "key_tracks",
                "sources",
                "tracks",
            ],
            &[In(O::Ducking)],
            Key("trigger_tracks"),
        ),
        (
            &[
                "depth",
                "amount",
                "reduction",
                "duck_db",
                "attenuation",
                "duck_amount",
                "gain_reduction",
                "level",
            ],
            &[In(O::Ducking)],
            Key("reduction_db"),
        ),
        (
            &["target", "lufs", "level", "loudness"],
            &[In(O::Normalize)],
            Key("target_lufs"),
        ),
        (
            &["threshold", "level", "db", "threshold_dbfs"],
            &[In(O::TrimSilence)],
            Key("threshold_db"),
        ),
        (
            &["padding", "pad", "margin", "keep"],
            &[In(O::TrimSilence)],
            Key("padding_ms"),
        ),
        // ---- providers -------------------------------------------------------------------------------------------
        (
            &[
                "url",
                "endpoint",
                "host",
                "server",
                "api_base",
                "base",
                "api_url",
                "base_uri",
                "endpoint_url",
                "address",
            ],
            &[In(O::Provider)],
            Key("base_url"),
        ),
        (
            &[
                "env",
                "key_env",
                "api_key_var",
                "env_var",
                "api_env",
                "key_variable",
                "token_env",
                "api_key_env_var",
            ],
            &[In(O::Provider)],
            Key("api_key_env"),
        ),
        (
            &[
                "concurrency",
                "parallel",
                "parallelism",
                "workers",
                "max_parallel",
                "max_workers",
                "threads",
                "jobs",
            ],
            &[In(O::Provider)],
            Key("max_concurrency"),
        ),
        (
            &[
                "timeout",
                "timeout_s",
                "timeout_sec",
                "request_timeout",
                "timeout_seconds",
            ],
            &[In(O::Provider)],
            Key("timeout_ms"),
        ),
        (
            &["cps", "chars_per_sec", "characters_per_second"],
            &[In(O::Provider)],
            Key("chars_per_second"),
        ),
        (&["wpm"], &[In(O::Provider)], Key("words_per_minute")),
        (
            &[
                "cmd",
                "argv",
                "args",
                "program",
                "exec",
                "executable",
                "binary",
                "cli",
            ],
            &[In(O::Provider)],
            Key("command"),
        ),
        (
            &[
                "voices",
                "voices_path",
                "models_dir",
                "model_dir",
                "model_path",
                "voice_dir",
                "data_dir",
            ],
            &[In(O::Provider)],
            Key("voices_dir"),
        ),
        (
            &["format", "audio_format", "response_format", "encoding"],
            &[In(O::Provider)],
            Key("output_format"),
        ),
        (
            &["provider", "kind", "engine", "backend", "provider_type"],
            &[In(O::Provider)],
            Key("type"),
        ),
    ]
};

/// The synonym suggestion for `key` (already normalized) at a location: `obj`, the track type when `obj` is a
/// track (None = unknown), and whether a voice object is a `speakers` entry.
pub(crate) fn synonym(
    obj: Obj,
    track_kind: Option<TrackKindName>,
    speaker_entry: bool,
    key: &str,
) -> Option<Synonym> {
    let applies = |place: &Place| match *place {
        Place::In(o) => o == obj,
        Place::AudioTrack => obj == Obj::Track && track_kind != Some(TrackKindName::Speech),
        Place::SpeechTrack => {
            obj == Obj::Track && matches!(track_kind, None | Some(TrackKindName::Speech))
        }
        Place::KnownSpeechTrack => obj == Obj::Track && track_kind == Some(TrackKindName::Speech),
        Place::Speaker => obj == Obj::Voice && speaker_entry,
    };
    SYNONYMS
        .iter()
        .find(|(keys, places, _)| keys.contains(&key) && places.iter().any(applies))
        .map(|(_, _, s)| *s)
}

/// Where an object kind lives in a document, for "this key belongs in …" hints.
pub(crate) fn location_label(obj: Obj) -> &'static str {
    match obj {
        Obj::Timeline => "the top level of the timeline",
        Obj::Output => "`output`",
        Obj::Subtitles => "`subtitles`",
        Obj::Timing => "`timing`",
        Obj::Mix => "`mix`",
        Obj::Voice => "`defaults` or a `speakers` entry",
        Obj::Provider => "a `providers` entry",
        Obj::Track => "a track",
        Obj::SequenceItem => "a `sequence` item (cue or pause)",
        Obj::Clip => "a `clips` entry",
        Obj::Anchor => "`start_relative` / `end_relative`",
        Obj::Ducking => "`ducking`",
        Obj::Normalize => "`normalize: {...}`",
        Obj::TrimSilence => "`trim_silence: {...}`",
    }
}

/// True for annotation keys (`x-…` / `x_…`), which are ignored everywhere.
pub(crate) fn is_annotation(key: &str) -> bool {
    key.starts_with("x-") || key.starts_with("x_")
}

/// Human description of what a value of `kind` looks like ("a number", "a duration: …").
pub(crate) fn expectation(kind: Kind) -> &'static str {
    match kind {
        Str => "a string",
        Num => "a number",
        UInt => "a whole number",
        Bool => "`true` or `false`",
        Millis => {
            "a duration: a number of milliseconds (350) or a string like \"350ms\", \"1.5s\", \"2m\", \"1m30s\" or \
             \"00:01:30.500\""
        }
        Bitrate => {
            "a bitrate: a string like \"192k\" or \"320kbps\", or a number of bits per second (192000)"
        }
        Version => "a version string such as \"1.0\" (the numbers 1 and 1.0 are accepted too)",
        Kind::Obj(_) => "a mapping (key: value pairs)",
        StrList => "a list of strings",
        ObjList(_) => "a list",
        NameMap(_) => "a mapping from names to settings",
        FreeMap => "a mapping (key: value pairs)",
        Normalize => "`true`, `false` or a mapping like `{target_lufs: -20}`",
        TrimSilence => "`true`, `false` or a mapping like `{threshold_db: -50, padding_ms: 30}`",
        SubtitleExport => {
            "`true` (vtt + srt), `false`, a format name (vtt, srt, json, transcript) or a list of them, e.g. [vtt, srt]"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_keys_per_object() {
        for &obj in ALL_OBJS {
            let mut names = names(obj);
            let n = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), n, "{obj:?} has duplicate keys");
        }
    }

    #[test]
    fn track_subsets_are_disjoint() {
        for (a, _) in TRACK_SPEECH {
            assert!(!TRACK_AUDIO.iter().any(|(b, _)| a == b), "{a}");
            assert!(!TRACK_COMMON.iter().any(|(b, _)| a == b), "{a}");
        }
        for (a, _) in TRACK_AUDIO {
            assert!(!TRACK_COMMON.iter().any(|(b, _)| a == b), "{a}");
        }
    }

    #[test]
    fn annotations() {
        assert!(is_annotation("x-note"));
        assert!(is_annotation("x_note"));
        assert!(!is_annotation("xnote"));
        assert!(!is_annotation("x"));
    }
}
