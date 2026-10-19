# SonicCraft timeline DSL v1.0: complete reference

Every field of a SonicCraft timeline, with its type, default and exact semantics. The machine-readable
JSON Schema is [`timeline-schema.json`](timeline-schema.json) (also printed by `soniccraft schema`).
Field names, defaults and diagnostic codes here are the stable v1.0 contract.

Contents: [Document rules](#document-rules) · [Value types](#value-types) · [Top level](#top-level) ·
[output](#output) · [subtitles](#subtitles) · [timing](#timing) · [mix](#mix) ·
[Voices (defaults, speakers)](#voices-defaults-speakers-and-inline-voice-fields) · [providers](#providers) ·
[Tracks](#tracks-common-fields) · [Speech tracks](#speech-tracks) · [Music and sfx tracks](#music-and-sfx-tracks) ·
[ducking](#ducking) · [Anchors](#anchors-start_relative-end_relative) · [Timeline geometry](#timeline-geometry-how-items-are-placed)

---

## Document rules

- A timeline is one YAML 1.2 document or one JSON document. `.json` files are parsed as JSON,
  `.yaml`/`.yml` as YAML; text on stdin (or MCP `yaml_content`) that starts with `{` is JSON unless it is YAML
  flow style (unquoted keys). JSON allows no comments and no trailing commas (`json_syntax`); use an
  `x-note` key, or write YAML.
- A document is at most 5 MB, 250000 values and 64 levels of nesting, in YAML and JSON alike.
- **Unknown keys are errors** (`unknown_field`, with a did-you-mean hint that also knows common synonyms:
  `file` → `source`, `voice_name` → `voice_id`, `start_after: x` → `start_relative: {after_cue: x}`). Keys
  starting with `x-` or `x_` are reserved for your own annotations and ignored everywhere (`x-notes: "..."`).
- Enum-like values (`type`, `format`, `loudness_preset`, `mode`, ...) are strings checked after parsing, so a
  typo yields `invalid_value` with a did-you-mean hint instead of a parse failure. Spellings are exact:
  lowercase with `_` between words, as listed below (`speech`, `mp3`, `equal_power`, `vtt`). `Speech`, `MP3`,
  `equal-power` or `VTT` are `invalid_value`, and the hint gives the exact spelling. A platform name as
  `loudness_preset` (`youtube`, `spotify`, `tv`) is `invalid_value`, and the hint gives the documented preset.
  A few aliases are accepted (`wave`, `aac`, `ebu`, `sine`, `webvtt`, ...); prefer the canonical names.
- Every problem is reported at once, each with a `code`, a `path` (`tracks[2].sequence[0].text`), the
  line/column for YAML input, and usually a mechanical `hint`. See [diagnostics.md](diagnostics.md).
- **Secrets never live in timelines.** Each of these is the error `secret_in_timeline`: a key named
  `api_key`, `apikey`, `token`, `secret` or `password` anywhere (any case or separators, `apiKey`,
  `client_secret`, and as an annotation, `x-api-key`); a key that itself looks like a provider key; a string
  that looks like a provider key (`sk-...`, `sk_...`); and inside a `providers` entry or a `settings` map, a
  provider key anywhere in a string (after `Bearer `, in `--api-key=...`, in a URL), a bearer token, 32+ hex
  characters, a password in a URL (`https://user:pass@host`) or a secret URL parameter (`?api_key=...`).
  Diagnostics never repeat the secret. Put keys in environment variables and name the variable with
  `api_key_env`.
- Booleans are `true` / `false`. YAML 1.1 words such as `yes`, `no`, `on`, `off` are not booleans.
- `null` (in YAML also a key without a value, `fade_in_ms:`) means the key is absent. Required keys need a
  value.
- In YAML, quote any `text` or `caption` that contains ` #` or `: `. Unquoted, ` #` starts a comment and cuts
  the text short (`text: Tip #1: ...` speaks only "Tip"; warning `comment_in_text`).
- Relative paths (`source`, `output.path`, piper models) resolve against the timeline file's directory.

## Value types

| Type | Accepted forms | Notes |
|---|---|---|
| `Millis` | number of milliseconds (may be fractional; at most 86400000 = 24 h), or a string with lowercase units: `"350ms"`, `"1.5s"`, `"2m"`, `"1m30s"`, `"1m 30.5s"`, `"01:30.5"` (mm:ss.fff), `"00:01:30.500"` (hh:mm:ss.fff). Unit words `ms`/`msec`/`milliseconds`, `s`/`sec`/`seconds`, `m`/`min`/`minutes`, `h`/`hr`/`hours` also work, with or without a space (`"1.5 sec"`). | Negative values are only valid for `offset_ms`; lengths (`duration_ms`, `output.duration_ms`, `subtitles.max_duration_ms`) must be greater than 0. Units are lowercase (`"350MS"` is invalid; the hint gives `"350ms"`). Clock seconds, and the minutes of hh:mm:ss, are below 60 and use digits only (`"1:99.5"` and `"1:1e2"` are invalid). A time above 24 h (usually epoch milliseconds) is `invalid_value`. A malformed string (`"1,5s"`, `"soon"`) is `invalid_value`. Small bare numbers that look like seconds (`duration_ms: 30`, `fade_in_ms: 2`, `pause: 1.5`) get the warning `suspicious_value`: write the unit (`"30s"`). |
| `Bitrate` | `"320k"`, `"320K"`, `"320kbps"`, `"192 kbps"`, `320000`, `320` | Numbers below 1000 are kbps; at most 10 Mbit/s. A string is a plain number (no sign or exponent), an optional `k` and an optional `bps`. Read only for lossy formats (wav/flac ignore it: `bitrate_ignored`). |
| linear gain (`volume`) | number ≥ 0 | `1.0` = unity. Above `4.0` is an error, above `2.0` a warning. |
| dB (`gain_db`) | number in [-60, +24] | |
| `pan` | number in [-1, 1] | -1 = hard left, 0 = center, +1 = hard right. |
| id | non-empty string matching `^[A-Za-z0-9_][A-Za-z0-9_.:-]*$` | **One namespace shared by tracks, cues and clips** (`duplicate_id`, `invalid_id`). Ids are not trimmed: `" v "` is `invalid_id`. |

## Top level

```yaml
version: "1.0"        # required
title: "..."          # optional metadata
description: "..."    # optional metadata
output: {...}         # file, format, mastering, side files
subtitles: {...}      # caption layout (export is switched on in output.export_subtitles)
timing: {...}         # overlap policy
mix: {...}            # source-normalization reference level
defaults: {...}       # voice applied to every cue
speakers: {name: voice, ...}
providers: {name: provider instance, ...}
tracks: [...]         # required, non-empty
```

| Field | Type | Default | Semantics |
|---|---|---|---|
| `version` | string or number | required | `"1.0"`; `"1"`, `"v1.0"`, `1` and `1.0` are accepted (digits only: `"01.0"` is `invalid_value`). Another major version is `unsupported_version`. |
| `title`, `description` | string | none | Metadata only (the web preview shows the title). |
| `tracks` | list of tracks | required | Must be non-empty (`empty_tracks`). Tracks are mixed in declaration order. |

## `output`

| Field | Type | Default | Semantics |
|---|---|---|---|
| `format` | `wav` \| `mp3` \| `flac` \| `ogg` \| `opus` \| `m4a` | from the output path extension, else `wav` | `ogg` (Vorbis), `opus` and `m4a` (AAC) need ffmpeg on PATH (`format_requires_ffmpeg`). If `format` and an explicit path extension disagree: warning `format_extension_mismatch`, `format` wins, so `-o out.mp3` on a `format: wav` timeline writes WAV data into `out.mp3`. Pass `--format mp3` (CLI) or `format: "mp3"` (MCP) to override `format` too. |
| `path` | string | `<timeline file stem>.<ext>` next to the timeline | Relative to the timeline directory. CLI `-o` and MCP `output_path` override it. An existing file is only replaced by an output of its kind (an audio file by audio, side files only if they are files): `-o notes.txt` over an existing `notes.txt` is `invalid_value` ("refusing to overwrite"). Over MCP, the timeline's own `output.path` must stay inside the base directory (an `output_path` argument may point anywhere). |
| `sample_rate` | integer | `48000` | 8000 to 192000. mp3 allows only 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000 (`unsupported_sample_rate`). opus is always encoded at 48000. |
| `channels` | `1` \| `2` | `2` | Mono output downmixes stereo sources `(L + R) / 2` and ignores pan. |
| `bitrate` | Bitrate | mp3 `192k`, ogg `160k`, opus `96k`, m4a `160k` | Ignored for wav/flac (warning `bitrate_ignored`). mp3 is CBR: values that are not a standard LAME CBR rate (32 to 320 kbps) are rounded to the nearest valid one (info `bitrate_adjusted`). |
| `bit_depth` | `16` \| `24` \| `32` | `24` | wav: 16/24-bit integer or 32-bit float. flac: 16/24 only (`unsupported_bit_depth`). Ignored for lossy formats. |
| `loudness_preset` | `podcast` \| `streaming` \| `broadcast` \| `atsc` | `podcast` | podcast = -16 LUFS / -1.0 dBTP; streaming = -14 / -1.0; broadcast (EBU R128) = -23 / -1.0; atsc (A/85) = -24 / -2.0. |
| `target_lufs` | number in [-60, -5] | from the preset | Integrated loudness target of the master; overrides the preset. Loudness values are negative: `target_lufs: 16` is `invalid_value` (hint: `-16`). |
| `true_peak_db` | number in [-9, 0] | from the preset | True-peak ceiling (dBTP); overrides the preset. For mp3, ogg, opus and m4a the delivered file is measured and re-limited when the codec overshoots (warning `true_peak_exceeded` if it still does). |
| `normalize` | bool | `true` | `false` skips the loudness gain stage; the true-peak limiter still runs as a clip guard. |
| `export_subtitles` | bool, a format name, or a list of `vtt` \| `srt` \| `json` \| `transcript` | `false` | `true` = `[vtt, srt]`. `json` = captions + word timestamps; `transcript` = HyperFrames word array. |
| `export_stems` | bool | `false` | Writes `<stem>.stems/<track_id>.wav`: 32-bit float, after ducking and track gain, before the master. |
| `export_manifest` | bool | `false` | Writes `<stem>.manifest.json`: the render report with relative paths, without wall-clock timings, cache provenance (`synthesis`) or the `cache_write_failed` warning, so it is byte-identical whenever the audio is. |
| `tail_ms` | Millis | auto | Time after the last finite item. Auto = the longest `fade_out_ms` among open-ended clips (0 if none). |
| `duration_ms` | Millis | none | Forces the total length. Content beyond it is cut (warning `content_truncated`). |
| `dither` | bool | `true` | TPDF dither (fixed seed) when writing 16-bit integer PCM. 24-bit is rounded without dither. |

Side files are written next to the main output and named after its stem: `<stem>.vtt`, `<stem>.srt`,
`<stem>.words.json`, `<stem>.transcript.json`, `<stem>.manifest.json`, `<stem>.stems/`,
`<stem>.preview.html` (the preview only with CLI `--preview`). All outputs are put in place together when the
render succeeds; a failed or cancelled render removes its partial files and leaves earlier outputs untouched.

## `subtitles`

Caption layout. Exporting is switched on with `output.export_subtitles`.

| Field | Default | Semantics |
|---|---|---|
| `max_chars_per_line` | `42` | Hard limit; a single word longer than this gets its own line. |
| `max_lines` | `2` | Lines per caption. |
| `max_duration_ms` | `6000` | A caption never spans longer than this. |
| `min_duration_ms` | `800` | Short captions are extended to this, without overlapping the next caption of the same track. |
| `speaker_labels` | auto | `true` / `false`. Auto = on when the timeline has 2 or more distinct speaker labels. WebVTT uses `<v Label>`, SRT a `Label: ` prefix. |

Captions never merge cues. A cue with `subtitles: false` is left out of all subtitle files.

## `timing`

| Field | Default | Semantics |
|---|---|---|
| `speech_overlap` | `push` | What happens when speech cues overlap: `push` \| `warn` \| `error` \| `allow`. See [scheduling](#scheduling-and-the-speech-overlap-policy). |
| `min_gap_ms` | `250` | Gap used when `push` moves a cue that has no explicit preceding pause. |
| `overlap_tolerance_ms` | `20` | Speech overlaps up to this long are ignored. |

## `mix`

| Field | Default | Semantics |
|---|---|---|
| `reference_lufs` | `-20` | Source-normalization target for tracks whose `normalize` is enabled without an explicit target. |

## Voices: `defaults`, `speakers` and inline voice fields

A voice (VoiceSpec) appears as `defaults`, as each entry of `speakers`, and inline on speech tracks and cues.

| Field | Semantics |
|---|---|
| `provider` | Provider instance name: a key of `providers`, or a built-in provider name/alias. |
| `voice_id` | Provider voice identifier (ElevenLabs voice id, OpenAI voice name, Kokoro voice such as `af_heart`, macOS voice name, piper model). |
| `model` | Provider model id. |
| `language` | BCP-47-style language code passed to providers that support it (`en`, `en-GB`). |
| `settings` | Free-form object of provider-specific voice settings (see [providers.md](providers.md)). Merged key by key, later layers win. |
| `gain_db` | Extra gain for this speaker's cues (`speakers` and `defaults` only). |
| `label` | Display name in subtitles (`speakers` only). Default: the speaker key. |

**Resolution order for each cue** (lowest to highest priority): built-in provider defaults → provider
instance `settings` / `model` → `defaults` → `speakers[track.speaker]` → track-level voice fields →
`speakers[cue.speaker]` (only when the cue sets `speaker` itself) → cue-level voice fields. `settings`
objects merge one level deep.

- A `speaker` that is not a key of a non-empty `speakers` map is `unknown_speaker`: an error when it is a
  misspelling of a defined speaker (the hint names it), otherwise a warning. With no `speakers` map at all,
  a speaker is just a subtitle label.
- Voice keys belong next to `settings`, not inside it: `settings: {voice: nova}` is a `field_not_applicable`
  warning (providers never read it); write `voice_id: nova`. The same goes for `model`, `provider` and
  `language` inside `settings`.
- Speakers that resolve to the same voice (same provider instance, `voice_id`, `model` and settings) sound
  identical and get the warning `speakers_share_voice`; give each its own `voice_id` (for `mock` any string
  works and gives a distinct voice: `voice_id: alex`, `voice_id: sam`).
- A cue whose voice resolves no provider is `missing_provider`.
- When no `voice_id` resolves, these providers use a default (info `default_voice`): openai `alloy`,
  kokoro `af_heart`, macos_say `Samantha`, mock `mock`. elevenlabs, cartesia, piper and command require an
  explicit `voice_id` (`missing_voice`).
- The subtitle label of a cue is its speaker's `label`, else the speaker key, else the track id.

## `providers`

A map from **instance name** to configuration. Cues reference instance names with `provider:`. A name
that is not configured but matches a built-in provider name or alias uses that built-in with defaults, so
`provider: mock` works without any `providers` entry.

Built-in types and aliases (spelled exactly): `elevenlabs` (`eleven_labs`, `11labs`), `openai` (`open_ai`),
`cartesia`, `kokoro` (`local_kokoro`, `kokoro_fastapi`), `piper`, `macos_say` (`say`, `system`), `command`,
`mock`.

| Field | Semantics |
|---|---|
| `type` | Built-in provider type. Defaults to the instance name when that is a built-in name or alias; required otherwise (`missing_field`). |
| `api_key_env` | (HTTP providers: elevenlabs, openai, cartesia, kokoro; elsewhere a `field_not_applicable` warning) Environment variable holding the API key. Defaults: `ELEVENLABS_API_KEY`, `OPENAI_API_KEY`, `CARTESIA_API_KEY`; kokoro: none (optional `KOKORO_API_KEY`). The key is sent only to the provider's own API host, to servers on this machine or a private network, or to hosts listed in `SONICCRAFT_ALLOWED_HOSTS` (set in soniccraft's environment, never in the timeline); any other `base_url` is `provider_unavailable`. A variable holding an unexpanded placeholder such as `${ELEVENLABS_API_KEY}` counts as unset. |
| `base_url` | (HTTP providers) API base override. Defaults: `https://api.elevenlabs.io`, `https://api.openai.com/v1` (any OpenAI-compatible `/audio/speech` server works: Kokoro-FastAPI, Speaches, LocalAI), `https://api.cartesia.ai`; kokoro: env `KOKORO_BASE_URL`, else `http://localhost:8880`. No credentials in the URL (`secret_in_timeline`). |
| `model` | Default model for cues using this instance. |
| `output_format` | Provider-native audio format override, e.g. ElevenLabs `pcm_44100` (default `pcm_24000`). |
| `max_concurrency` | Parallel requests to this instance. Defaults: elevenlabs 2, openai 4, cartesia 2, kokoro 1, piper 2, macos_say 4, command 2, mock 8. |
| `timeout_ms` | Time limit for one cue's synthesis, retries and waits included (an SSE stream too). Default `120000`. |
| `chars_per_second` | Speaking-rate assumption for dry-run duration estimates (preferred). |
| `words_per_minute` | Alternative speaking-rate assumption, used only when `chars_per_second` is not set. |
| `settings` | Default voice settings for cues using this instance. |
| `command` | (`command`, `piper`) argv template, e.g. `["piper", "-m", "{voice_id}", "-f", "{output}"]`. |
| `stdin` | (`command`, `piper`) template written to the child's stdin, e.g. `"{text}"`. Omit for none. |
| `voices_dir` | (`piper`) directory searched first for `<voice_id>.onnx`, relative to the timeline. A bare voice name is then looked up in `$PIPER_VOICES_DIR`, the timeline directory and `~/.local/share/piper/voices`; a `voice_id` with `/` or ending in `.onnx` is a model path. A model found nowhere is `unknown_voice` (with the download command). |

Template placeholders: `{text}`, `{text_file}` (path of a UTF-8 temp file holding the text), `{output}`
(path the command must write, any decodable audio format), `{voice_id}`, `{model}`, `{language}`. Pass the
text with `{text_file}` or `stdin: "{text}"`, not as an argument of its own: a cue text starting with `-`
would read as an option, so SonicCraft refuses to run such a command, and `validate` warns about the
template (`suspicious_value`). Programs run without the provider API keys in their environment. Over MCP,
`command` providers (and piper with its own `command`) run only when the server was started with
`--allow-command`; otherwise they are `provider_unavailable`.

```yaml
providers:
  narration:              # custom name, so `type` is required
    type: elevenlabs
    model: eleven_multilingual_v2
    max_concurrency: 2
    settings: { stability: 0.45, similarity_boost: 0.8 }
  kokoro:                 # built-in name: type defaults to kokoro
    base_url: http://localhost:8880
  espeak:
    type: command          # runs a local program; over MCP only with `soniccraft mcp --allow-command`
    command: ["espeak-ng", "-v{voice_id}", "-w", "{output}", "-f", "{text_file}"]
```

## Tracks: common fields

| Field | Type | Default | Semantics |
|---|---|---|---|
| `id` | id | required | Unique across tracks, cues and clips. |
| `type` | `speech` \| `music` \| `sfx` | required | Selects which other fields apply. |
| `volume` | linear | `1.0` | Track fader. |
| `gain_db` | dB | `0` | Added to the fader: track gain = `volume × 10^(gain_db / 20)`. |
| `pan` | [-1, 1] | `0` | Constant-power pan for mono sources, balance for stereo sources. |
| `mute` | bool | `false` | Laid out (timing stays stable) and synthesized, but not mixed. |
| `normalize` | bool \| `{target_lufs: n}` | speech and music `true`, sfx `false` | Source-level loudness normalization: each cue (speech) or each distinct source file (music/sfx) is gained to `target_lufs`, else `mix.reference_lufs` (-20). Gain is capped at +24 dB; sources too short or quiet to measure are left unchanged (info `normalization_skipped`). |
| `start_at_ms` / `start_relative` | Millis / Anchor | `0` | Start of the track's first item when that item has no explicit start. Mutually exclusive (`conflicting_fields`). |
| `ducking` | DuckingSpec | none | Duck this track while other tracks are active ([ducking](#ducking)). Allowed on any track type. |

## Speech tracks

`type: speech`. Additional fields:

| Field | Default | Semantics |
|---|---|---|
| `sequence` | required, non-empty | Ordered cues and pauses (`empty_sequence`). |
| `speaker`, `provider`, `voice_id`, `model`, `language`, `settings` | none | Voice for every cue of the track (see the resolution order above). |
| `trim_silence` | `true` | `true` / `false` / `{threshold_db: -50, padding_ms: 30}`: trim leading and trailing silence from each synthesized cue, keeping `padding_ms` on each side. |

Audio-track fields (`source`, `clips`, `loop`, `loop_crossfade_ms`, `fade_in_ms`, `fade_out_ms`,
`fade_curve`, `trim_start_ms`, `duration_ms`, `end_at_ms`, `end_relative`) on a speech track are
`field_not_applicable`, and so are `sequence`, `trim_silence` and voice fields on music/sfx tracks.
`crossfade_ms` exists only in `clips` entries (`unknown_field` on a track).

### Sequence items

Each item is either a **pause** or a **cue**. A mapping with both pause and cue fields is
`conflicting_fields`.

**Pause:** `{pause_duration: Millis}` or `{pause: Millis}`, and nothing else. It advances the track cursor.
A misspelled pause (`{pause_ms: 300}`, `{wait: 1s}`) is one `unknown_field` error whose hint writes the pause
(`pause: 300`); it is not also reported as a cue without text.

**Cue:**

| Field | Default | Semantics |
|---|---|---|
| `id` | `<track_id>.cue<n>` | n = 1-based count of cues in that track. Give ids to every cue you anchor to. |
| `text` | required | What the provider speaks; non-empty after trimming (`empty_text`). Quote it in YAML when it contains ` #` or `: ` (`comment_in_text`). |
| `caption` | `text` | Subtitle text override (the text says "minus sixteen LUFS", the caption shows "-16 LUFS"). |
| `subtitles` | `true` | `false` leaves the cue out of subtitle files. |
| `speaker`, `provider`, `voice_id`, `model`, `language`, `settings` | inherited | Voice overrides for this cue. |
| `start_at_ms` / `start_relative` | none | Explicit start (mutually exclusive). Without either, the cue is **implicit**: it starts at the track cursor. |
| `volume`, `gain_db` | `1.0`, `0` | Per-cue gain on top of the track fader. |
| `allow_overlap` | `false` | `true` exempts this cue from overlap handling (intentional crosstalk). |

## Music and sfx tracks

`type: music` or `type: sfx`. A track has either the **single-clip shorthand** (clip fields directly on the
track; the clip's id is the track id) or a `clips` list, never both (`conflicting_fields`; move the fields
into each clip).

| Clip field | Default | Semantics |
|---|---|---|
| `id` | `<track_id>.clip<n>` | Shorthand clip: the track id. |
| `source` | required | Audio file relative to the timeline directory: wav, mp3, flac, ogg, m4a/aac, aiff, at 1 to 768 kHz. Missing file: `asset_not_found`; undecodable or an unsupported rate: `asset_unreadable`. Sources with more than two channels (5.1, 7.1) are mixed down to stereo (ITU-R BS.775, LFE dropped; info `channels_downmixed`); NaN/infinite samples become silence (warning `audio_repaired`). |
| `start_at_ms` / `start_relative` | sequential | First clip: the track start. Later clips: the end of the previous clip minus this clip's `crossfade_ms`. |
| `trim_start_ms` | `0` | Offset into the source where playback begins. Beyond the source end: `invalid_value`. |
| `duration_ms` / `end_at_ms` / `end_relative` | natural | At most one (`conflicting_fields`). See [clip length](#clip-length). `end_relative` takes `after_cue` or `with_cue` (not `before_cue`). |
| `loop` | `false` | Loop the source (from `trim_start_ms`) to fill the clip length. |
| `loop_crossfade_ms` | `0` | Equal-power crossfade at each loop seam (for sources that do not loop seamlessly). |
| `fade_in_ms`, `fade_out_ms` | `0` | Fades at the clip's own start and end. If both together exceed the clip length they are scaled down proportionally (info `fades_scaled`). |
| `fade_curve` | `equal_power` | `linear` \| `equal_power` \| `exponential` (a 60 dB exponential ramp). |
| `crossfade_ms` | `0` | (clips list only) Overlap with the previous clip, with equal-power fades on both sides. Only a clip that follows the previous clip sequentially (no `start_at_ms`/`start_relative`) crossfades; on a clip with its own start it is ignored with warning `field_not_applicable`. |
| `volume`, `gain_db` | `1.0`, `0` | Per-clip gain on top of the track fader. |

```yaml
- id: bed                       # shorthand: one looping clip
  type: music
  source: assets/bed.wav
  loop: true
  fade_in_ms: 1500
  fade_out_ms: 3000
- id: stingers                  # clips list
  type: sfx
  clips:
    - { source: assets/whoosh.wav, start_relative: { before_cue: intro } }
    - { source: assets/chime.wav, start_relative: { after_cue: outro, offset_ms: 400 } }
```

## `ducking`

Lowers a track while trigger tracks are active (sidechain ducking), on any track type.

| Field | Default | Semantics |
|---|---|---|
| `trigger_tracks` | required, non-empty | Track ids whose activity ducks this track. Unknown id: `unknown_reference`; the track itself: `self_reference`. |
| `reduction_db` | `-15` | Depth. The sign is ignored: `-18` and `18` both attenuate by 18 dB. |
| `attack_ms` | `80` | Ramp time from 0 dB to full depth (linear in dB). |
| `release_ms` | `400` | Ramp time from full depth back to 0 dB. |
| `hold_ms` | `250` | Activity gaps shorter than this keep the duck engaged; activity is also held this long after it stops. |
| `lookahead_ms` | = `attack_ms` | Activity is detected this much earlier, so the track is fully ducked when speech begins. |
| `threshold_db` | `-40` | dBFS RMS threshold for `sidechain` mode. |
| `mode` | `sidechain` | `sidechain`: signal-driven, from the trigger tracks' pre-fader audio (1 ms blocks, 10 ms RMS window). `cue`: timeline-driven, active wherever a trigger track item plays. |

In `sidechain` mode, muted trigger tracks are excluded (info `muted_trigger`). Dry runs always compute
ducking in `cue` mode from estimated intervals; renders use the configured mode. Reports list the active
regions `[start_ms, end_ms)` (after hold and lookahead) and the depth per ducked track.

## Anchors: `start_relative`, `end_relative`

```yaml
start_relative: { after_cue: cue_01, offset_ms: 150 }   # start = end(cue_01) + 150 ms
start_relative: { with_cue: cue_01, offset_ms: -200 }   # start = start(cue_01) - 200 ms
start_relative: { before_cue: cue_02, offset_ms: 0 }    # this item ENDS at start(cue_02) + offset
end_relative:   { after_cue: cue_03, offset_ms: 2000 }  # clip ends 2 s after cue_03 ends
end_relative:   { with_cue: cue_03 }                    # clip ends when cue_03 starts
```

- Exactly one of `after_cue` / `with_cue` / `before_cue` (`conflicting_fields` otherwise); `before_cue`
  is invalid in `end_relative`.
- `offset_ms` is a signed Millis, default 0.
- The referenced id may be a **cue, a clip or a track**. A track is anchored in declaration order:
  `with_cue`/`before_cue` use the start of its first declared item, `after_cue` the end of its last declared
  item. If another item of that track starts earlier or ends later (an item with its own `start_at_ms`
  declared out of order), you get the warning `track_anchor_out_of_order`: anchor to that item instead
  (`after_cue: outro`) or reorder the sequence. Pauses are not anchor targets.
- Referencing the end of an open-ended item (a looping clip with no length, or a track that ends with
  one) is `anchor_open_ended`. Self-references are `self_reference`; cycles are `dependency_cycle` (the
  message lists the cycle, e.g. `cue_02 → cue_03 → cue_02`, and names the item a track anchor resolves to).
- A clip's start and end are scheduled separately, so a bed can end after the last line
  (`end_relative: {after_cue: last_line}`) while the first line starts with the bed
  (`start_relative: {with_cue: bed, offset_ms: 2000}`): that is not a cycle.
- `start_at_ms` and `end_at_ms` take times, not ids: `start_at_ms: a2` is `invalid_value`, with the hint
  `start_relative: {after_cue: a2}`.
- A resolved start below 0 is clamped to 0 (warning `negative_start`).

## Timeline geometry: how items are placed

All layout math is done in integer sample frames at the output sample rate:
`frames(ms) = round(ms × sample_rate / 1000)`. Every cue, pause and clip is a node with a start rule
(sequential, absolute or relative) and a duration: cues use their synthesized audio length after silence
trimming (an estimate in dry runs), pauses their value, clips the [clip length](#clip-length).

### Sequential placement

Within a track a cursor starts at the track start (`start_at_ms` / `start_relative`, default 0). Items are
visited in order: a pause advances the cursor; an implicit cue or clip starts at the cursor; an explicit
one starts where its rule says. After each cue or clip the cursor moves to its end (for clips: the end
minus the next clip's `crossfade_ms`).

### Clip length

`natural = source length − trim_start_ms`.

| Specified | Not looping | Looping |
|---|---|---|
| nothing | natural | **open-ended**: until the timeline end |
| `duration_ms` | min(duration, natural); info `clip_shorter_than_requested` if cut short | duration |
| `end_at_ms` / `end_relative` | min(end − start, natural); same info | end − start |

An end before the start is `invalid_value`. Open-ended clips do not count toward the content end and their
end cannot be anchored to.

### Scheduling and the speech-overlap policy

Nodes are scheduled in dependency order. When several are ready: explicit starts (absolute/relative)
before sequential ones, then the smallest tentative start, then declaration order (track, item).

Two speech cues **overlap** when their intervals intersect by more than `overlap_tolerance_ms` (any speech
tracks, muted ones included), unless either has `allow_overlap: true`.

- **`push`** (default): when an *implicit* cue's tentative interval overlaps an already scheduled speech
  cue, it moves to `end(other) + gap`, where `gap` = the sum of the pauses directly before it in its
  sequence, or `timing.min_gap_ms` if there are none. This repeats until it overlaps nothing; it never moves
  earlier than its cursor. Each moved cue reports one info `cue_pushed` (from, to, the cue it waits for).
  **Explicit cues are never moved**, and an implicit cue is never pushed past an explicit cue anchored to it
  (an interjection with `with_cue`, a reply with a negative `after_cue` offset): such overlaps stay
  `speech_overlap` warnings. If an overlap remains between an implicit cue and an explicit cue scheduled
  after it, scheduling re-runs with the explicit cues as fixed obstacles (at most 4 passes, the best kept).
  Anything still overlapping is a warning `speech_overlap`.
- **`warn`**: nothing moves; overlaps are warnings `speech_overlap`.
- **`error`**: overlaps are errors; the render aborts.
- **`allow`**: nothing is reported.

Each `speech_overlap` names both cues, both intervals and the overlap length, and suggests a fix such as
`set start_relative: {after_cue: cue_02, offset_ms: 350} on cue_03, or allow_overlap: true for intentional crosstalk`.

Worked example (the README timeline): `cue_01` plays [0, d1). `cue_02` is explicit: `after cue_01 + 150 ms`.
`cue_03` is implicit after a 350 ms pause, so its tentative start d1 + 350 ms overlaps `cue_02`; `push` moves
it to `end(cue_02) + 350 ms` (the preceding pause). Result: a natural three-turn exchange.

### Content end and total length

`content_end` = the latest end over all finite nodes (cues, pauses, clips that are not open-ended).
`total` = `output.duration_ms` if set, else `content_end + tail`, where `tail` = `output.tail_ms` if set,
else the longest `fade_out_ms` of the open-ended clips (0 if none). Open-ended clips end at `total`, so their
fade-out finishes exactly at the end. An open-ended clip without a fade-out gets info `no_fade_out_on_bed`.

Limits: a timeline is at most 24 h long (`timeline_too_long`, naming the item that ends last; look for seconds
or minutes written as milliseconds). A total of 0 (only a looping bed, no tail, no finite content) is
`invalid_value`. A render also has a memory budget (`SONICCRAFT_MAX_RENDER_GB`, default 8 GB: about 4 to 6
hours of stereo 48 kHz audio, about 1.5 hours of 24-bit FLAC): above it the render stops before synthesizing
anything (`timeline_too_long`, a warning in validate and plan).

Durations are exact at render time and estimated in dry runs (see "Estimating durations" in
[providers.md](providers.md#estimating-durations-dry-runs); mock voices are exact in dry runs too); the overlap
policy and ducking are recomputed exactly when rendering. A layout error that depends on estimated lengths is
a warning in validate and plan, ending in "(based on estimated speech durations; the render checks again with
exact durations)".
