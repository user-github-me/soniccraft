# SonicCraft — Design Specification (DSL v1.0, engine v0.1)

This document is the single source of truth for SonicCraft's behaviour. Code, tests, the JSON Schema,
the Agent Skill and the MCP tool descriptions must all agree with it. If an implementation detail is not
covered here, choose the most deterministic, least surprising option and document it in code comments.

---------------------------------------------------------------------------------------------------------

## 0. Principles

1. **Declarative in, deterministic out.** The same timeline + the same TTS cache + the same binary must
   produce byte-identical audio, subtitles and manifests. No wall-clock timestamps, random seeds, hostnames
   or absolute paths may leak into rendered artifacts. Any pseudo-randomness (dither, mock voices) uses a
   fixed-seed PRNG.
2. **Agent-first diagnostics.** Never stop at the first problem when more can be found. Every diagnostic
   carries a stable `code`, a precise `path` (and line/column when the source is YAML), a human message and,
   wherever possible, a concrete `hint` that an LLM can apply mechanically ("did you mean `fade_in_ms`?",
   "set `start_relative: {after_cue: cue_02, offset_ms: 350}`").
3. **Native, dependency-light DSP.** All signal processing (decode, resample, mix, ducking, loudness, limiting)
   is done in Rust on f32 buffers. FFmpeg is *never* required for the core formats (wav, mp3, flac); it is an
   optional encoder backend for ogg/opus/m4a and an optional decode fallback.
4. **Sample-accurate geometry.** All layout math is done in integer sample frames at the output sample rate.
   Milliseconds only exist at the edges (input DSL and output reports).
5. **Cheap iteration.** TTS results are cached by content hash; dry runs never call paid APIs.

---------------------------------------------------------------------------------------------------------

## 1. Repository layout and crate boundaries

```
crates/
  soniccraft-core     DSL raw AST (serde + schemars), parsing (YAML/JSON), diagnostics, lowering/validation
                      to an IR, JSON Schema generation, static provider registry metadata. No I/O except
                      reading env vars (pure, unit-testable).
  soniccraft-layout   Timeline geometry (§3): a pure function of the IR and a duration table; ASCII plan diagram.
  soniccraft-captions Word alignment, caption segmentation, WebVTT/SRT/words.json/transcript.json writers (§6, §10).
  soniccraft-dsp      AudioBuffer, decoding (symphonia; ffmpeg fallback), resampling (rubato), analysis
                      (silence trim bounds, speech regions), clip rendering (offset/loop/fades/gain), pan,
                      ducking envelopes, loudness measurement (ebur128), true-peak limiter, mastering loop,
                      encoding (hound WAV, LAME MP3, FLAC; ffmpeg for ogg/opus/m4a).
  soniccraft-tts      Provider trait + built-in providers (elevenlabs, openai, cartesia, kokoro, piper, macos_say,
                      command, mock), content-addressed cache, duration estimator, retry/concurrency control,
                      credential checks, voice listing. Depends on core (provider metadata types) and dsp.
  soniccraft-engine   Orchestration: validate(), plan() (dry run), render(). Owns environment checks (assets
                      exist, credentials, ffmpeg availability, cache status) and all report types (serde).
  soniccraft-mcp      MCP stdio server (newline-delimited JSON-RPC 2.0) exposing engine operations as tools.
  soniccraft-cli      The `soniccraft` binary (clap). Human-readable and `--json` output.
packages/
  soniccraft-web-preview   Zero-dependency static WebAudio visualizer; the engine embeds a vendored copy (§10.2)
                           and writes it with `render --preview`.
  soniccraft-mcp           npm launcher (`npx -y soniccraft-mcp`): finds or downloads the release binary
                           (checksums pinned in the package) and runs `soniccraft mcp`.
skills/soniccraft          SKILL.md + reference/ + templates/ (Agent Skills standard).
examples/                  Runnable example timelines with royalty-free, procedurally generated assets.
```

Dependency direction (no cycles): `layout` → core; `captions` (standalone); `tts` → {core, dsp};
`engine` → {core, layout, captions, dsp, tts}; `mcp` → engine; `cli` → {engine, mcp}.

---------------------------------------------------------------------------------------------------------

## 2. The timeline DSL (v1.0)

A timeline is a YAML (1.2) or JSON document. Unknown keys are **errors** (code `unknown_field`, with a
did-you-mean hint) unless they start with `x-` or `x_`, which are reserved for user annotations and ignored.
The did-you-mean hint also covers common synonyms (`file` → `source`, `voice_name` → `voice_id`,
`start_after: x` → `start_relative: {after_cue: x}`) and says that times are milliseconds. `null` (in YAML also
a key without a value) means the key is absent; required keys need a value. A document is at most 5 MiB, 250000
values (mappings, lists, scalars, keys) and 64 levels of nesting, the same limits for YAML and JSON
(`yaml_syntax` / `json_syntax`). The JSON Schema (`soniccraft schema`) encodes every structural rule (required
keys, conflicts, applicability by track type, sign and length of times); references, duration strings above
24 h and format-specific values are checked by the compiler only.

### 2.1 Value types

| Type | Accepted forms | Notes |
|---|---|---|
| `Millis` | number (milliseconds, may be fractional, \|v\| ≤ 86400000 = 24 h) or string with lowercase units: `"350ms"`, `"1.5s"`, `"2m"`, `"1m30s"`, `"1m 30.5s"`, `"01:30.5"` (mm:ss.fff), `"00:01:30.500"` (hh:mm:ss.fff). Unit words: `ms`/`msec(s)`/`millisecond(s)`, `s`/`sec(s)`/`second(s)`, `m`/`min(s)`/`minute(s)`, `h`/`hr(s)`/`hour(s)`, with optional spaces (`"1.5 sec"`). | Negative values are only valid for `offset_ms`; lengths (`duration_ms`, `output.duration_ms`, `subtitles.max_duration_ms`) must be > 0. Every time is at most 24 h (86 400 000 ms; `offset_ms` between -24 h and +24 h); larger values, typically epoch milliseconds, are `invalid_value`. Units are lowercase (`"350MS"` is `invalid_value`, hint `"350ms"`); clock seconds, and the minutes of hh:mm:ss, are < 60 and use decimal digits only (no exponents). One grammar is shared by the parser and the JSON Schema pattern. Small bare numbers that are almost certainly seconds (a clip or output length below 100, a fade, crossfade or pause above 0 and below 10, a fractional start or offset below 10) get the warning `suspicious_value` with the seconds spelling as hint; strings with units never do. Parsing happens after the syntax stage, so a bad string yields an `invalid_value` diagnostic (with the corrected spelling in the hint when there is one), never a parse failure. |
| `Bitrate` | `"320k"`, `"320K"`, `"320kbps"`, `"192 kbps"`, `320000`, `320` | numbers < 1000 are kbps; result is bits per second, at most 10 Mbit/s. Strings: a decimal number (no sign or exponent), optional `k`, optional `bps`, case-insensitive. |
| linear gain (`volume`) | number ≥ 0 | 1.0 = unity. Values > 4.0 are errors, > 2.0 a warning. |
| dB (`gain_db`) | number in [-60, +24] | |
| `pan` | number in [-1, 1] | -1 = hard left, +1 = hard right. |
| ids | non-empty string without whitespace, matching `^[A-Za-z0-9_][A-Za-z0-9_.:-]*$` | One namespace shared by tracks, cues and clips. Ids are not trimmed: `" v "` is `invalid_id`. |

Enum-valued fields are deserialized as strings and validated after parsing, so a typo produces an
`invalid_value` diagnostic with a did-you-mean hint instead of aborting the parse. Spellings are exact (the
listed names and aliases, lowercase, `_` between words). A case, separator or whitespace variant (`Speech`,
`MP3`, `equal-power`) is `invalid_value` with the exact spelling in the hint, and the hint's value is used for
the remaining checks. Accepted aliases: format `wave` (wav), `vorbis` (ogg), `aac`/`mp4` (m4a); loudness preset
`ebu_r128`/`ebu` (broadcast), `atsc_a85`/`a85` (atsc); fade curve `equalpower`/`sine`/`constant_power`
(equal_power), `exp`/`log`/`logarithmic` (exponential); ducking mode `timeline` (cue); subtitle format
`webvtt` (vtt), `words` (json), `hyperframes` (transcript); provider types as in §2.8. A platform name as
`loudness_preset` (`youtube`, `spotify`, `tv`) is `invalid_value` whose hint names the preset to use
(`streaming`). Values with one mechanical fix are corrected in the hint and used for the remaining checks: a
positive `target_lufs`/`reference_lufs`/`true_peak_db` (`target_lufs: 16` → `-16`), a duration with uppercase
units. A `start_at_ms`/`end_at_ms` written as an id (`start_at_ms: a2`) is `invalid_value` with the anchor
to write instead (`start_relative: {after_cue: a2}`).

### 2.2 Top level

```yaml
version: "1.0"        # required. "1", "1.0", "v1.0", 1 and 1.0 are accepted (digits only: "01.0" is invalid). Any other major version → unsupported_version.
title: "..."          # optional metadata
description: "..."    # optional metadata
output: {...}         # §2.3
subtitles: {...}      # §2.4 caption styling (export is switched on in output.export_subtitles)
timing: {...}         # §2.5
mix: {...}            # §2.6
defaults: {...}       # VoiceSpec (§2.7) applied to every cue
speakers: {name: VoiceSpec, ...}   # §2.7
providers: {name: ProviderSpec, ...} # §2.8
tracks: [Track, ...]  # required, non-empty (§2.9–2.11)
```

### 2.3 `output`

| Field | Type | Default | Semantics |
|---|---|---|---|
| `format` | `wav` \| `mp3` \| `flac` \| `ogg` \| `opus` \| `m4a` | from the output path extension, else `wav` | `ogg` (Vorbis), `opus`, `m4a` (AAC) need ffmpeg on PATH (`format_requires_ffmpeg`). If both `format` and an explicit output path extension are given and disagree → warning `format_extension_mismatch`, `format` wins (so CLI `-o out.mp3` on a `format: wav` timeline writes WAV data). The CLI's `--format` and MCP's `format` argument override both. |
| `path` | string | `<timeline file stem>.<ext>` next to the timeline | Relative to the timeline directory. CLI `-o` / MCP `output_path` override it. An existing file is replaced only by an output of its kind: the audio file by a file with an audio extension (wav, wave, mp3, flac, ogg, oga, opus, m4a, aac, mp4, aif, aiff), side files only when they are files, `<stem>.stems` only when it is a directory. Anything else (`notes.txt` as the audio, a directory where `<stem>.srt` goes) is `invalid_value` ("refusing to overwrite `notes.txt`: it is not a SonicCraft output"), reported before any work. Engines that confine outputs (the MCP server: the call's base directory) reject an `output.path` that resolves outside it through `..` or symlinks (`invalid_value` at `output.path`); CLI `-o`, MCP `output_path` and the default path next to the timeline are exempt. |
| `sample_rate` | int | `48000` | 8000–192000. mp3 only allows 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000 (`unsupported_sample_rate`). opus is always encoded at 48000 by ffmpeg. |
| `channels` | `1` \| `2` | `2` | |
| `bitrate` | Bitrate | mp3 192k, ogg 160k, opus 96k, m4a 160k | Ignored (warning `bitrate_ignored`) for wav/flac. mp3 CBR must be a valid LAME 3.100 bitrate for the sample rate (MPEG-1 32–48 kHz: 32–320 kbps; MPEG-2 16–24 kHz: 8–160; MPEG-2.5 8–12 kHz: 8–64); others are rounded to the nearest valid value with a `bitrate_adjusted` info. The encoder also raises bitrates whose frames cannot hold LAME's gapless Info tag (e.g. 16 kHz mono 32 → 40 kbps); the engine reports that as `bitrate_adjusted` too. |
| `bit_depth` | `16` \| `24` \| `32` | `24` | wav: 16/24 integer, 32 float. flac: 16/24 only. Ignored for lossy formats. |
| `loudness_preset` | `podcast` \| `streaming` \| `broadcast` \| `atsc` | `podcast` | podcast = -16 LUFS / -1.0 dBTP; streaming = -14 / -1.0; broadcast (EBU R128) = -23 / -1.0; atsc (A/85) = -24 / -2.0. |
| `target_lufs` | number in [-60, -5] | from preset | Overrides the preset target. Quieter targets would fall under the BS.1770 absolute gate (-70 LUFS), where loudness is undefined. |
| `true_peak_db` | number in [-9, 0] | from preset | Overrides the preset ceiling (dBTP). |
| `normalize` | bool | `true` | `false` skips the loudness gain stage; the true-peak limiter still runs as a clip guard. |
| `export_subtitles` | bool \| list of `vtt` \| `srt` \| `json` \| `transcript` \| single string | `false` | `true` = `[vtt, srt]`. `json` = captions + word timestamps (§10.1); `transcript` = HyperFrames-compatible word array (§10.3). |
| `export_stems` | bool | `false` | Writes `<out>.stems/<track_id>.wav` (32-bit float, post-ducking, post-track-gain, pre-master). |
| `export_manifest` | bool | `false` | Writes `<out>.manifest.json` (the render report with relative paths, without `timings_ms`, the `synthesis` cache counters and the `cache_write_failed` warning; cached cue durations read `exact`). |
| `tail_ms` | Millis | auto (§3.5) | Silence/bed time after the last finite item. |
| `duration_ms` | Millis | none | Forces the total length. Content beyond it is cut (`content_truncated` warning). |
| `dither` | bool | `true` | TPDF dither (fixed seed) when quantizing to 16-bit integer PCM. |

Output side files, all next to the main output and derived from its stem: `<stem>.vtt`, `<stem>.srt`,
`<stem>.words.json`, `<stem>.transcript.json`, `<stem>.manifest.json`, `<stem>.stems/`, `<stem>.preview.html`.

### 2.4 `subtitles`

| Field | Default | Semantics |
|---|---|---|
| `max_chars_per_line` | `42` | Hard limit (a single word longer than this gets its own line). |
| `max_lines` | `2` | Lines per caption. |
| `max_duration_ms` | `6000` | A caption never spans longer than this. |
| `min_duration_ms` | `800` | Captions are extended (without overlapping the next caption of the same track) to at least this. |
| `speaker_labels` | auto | `true`/`false`; auto = on when the timeline has ≥ 2 distinct speaker labels. |

### 2.5 `timing`

| Field | Default | Semantics |
|---|---|---|
| `speech_overlap` | `push` | `push` \| `warn` \| `error` \| `allow` — see §3.4. |
| `min_gap_ms` | `250` | Gap used when `push` moves a cue that has no explicit preceding pause. |
| `overlap_tolerance_ms` | `20` | Speech overlaps up to this long are ignored. |

### 2.6 `mix`

| Field | Default | Semantics |
|---|---|---|
| `reference_lufs` | `-20` | Source-level normalization target (§4.2) used by tracks whose `normalize` is enabled without an explicit target. |

### 2.7 VoiceSpec (`defaults`, `speakers.<name>`, and inline on tracks/cues)

| Field | Semantics |
|---|---|
| `provider` | Provider instance name (§2.8). |
| `voice_id` | Provider voice identifier. |
| `model` | Provider model id. |
| `language` | BCP-47-ish language code passed to providers that support it. |
| `settings` | Free-form object of provider-specific voice settings, merged key-by-key (later layers win). |
| `gain_db` | Extra gain applied to this speaker's cues (speakers/defaults only). |
| `label` | Display name for subtitles (speakers only). Default: the speaker key. |

**Resolution order for a cue (lowest → highest priority):** built-in provider defaults → provider instance
`settings`/`model` (§2.8) → `defaults` → `speakers[track.speaker]` → track-level voice fields →
`speakers[cue.speaker]` (only when the cue sets `speaker` explicitly) → cue-level voice fields. `settings`
objects merge one level deep. A `speaker` that is not in a non-empty `speakers` map yields `unknown_speaker`:
an error when the did-you-mean finds a defined speaker (a misspelling; the cue's voice is then checked as if
the hint were applied), otherwise a warning (an unregistered speaker is just a label). Voice-level keys
written inside `settings` (`voice`, `voice_id`, `voice_name`, `model`, `model_id`, `provider`, `language`, a
non-numeric `speaker`) are a `field_not_applicable` warning with a hint to move them (`settings.voice: nova` →
`voice_id: nova`); providers never read them there. Two or more distinct speakers whose cues resolve to an
identical voice (provider instance, `voice_id`, `model` and merged `settings`) get the warning
`speakers_share_voice` at the first cue of the second speaker; give each speaker its own `voice_id` (for mock
any string works, e.g. `voice_id: alex` / `voice_id: sam`).

Per-provider voice defaults (used when no `voice_id` resolves, with info `default_voice`): openai → `alloy`,
kokoro → `af_heart`, macos_say → `Samantha`, mock → `mock`. elevenlabs, cartesia, piper and command require
an explicit `voice_id` (`missing_voice`).

### 2.8 `providers`

A map from **instance name** to configuration. Cues reference instance names via `provider:`. If a name is
not configured but matches a built-in provider name or alias, the built-in is used with defaults.

Built-in provider types and aliases (spelled exactly, §2.1): `elevenlabs` (`eleven_labs`, `11labs`), `openai`
(`open_ai`), `cartesia`, `kokoro` (`local_kokoro`, `kokoro_fastapi`), `piper`, `macos_say` (`say`, `system`),
`command`, `mock`.

| Field | Semantics |
|---|---|
| `type` | Built-in provider type. Defaults to the instance name if that is a built-in name/alias; required otherwise (`missing_field`). |
| `api_key_env` | (HTTP providers: elevenlabs, openai, cartesia, kokoro; a `field_not_applicable` warning elsewhere) Env var holding the API key. Defaults: `ELEVENLABS_API_KEY`, `OPENAI_API_KEY`, `CARTESIA_API_KEY`; kokoro: none (optional `KOKORO_API_KEY`). A key is sent only to the provider's own API host (exactly the host of its default `base_url`), to servers on this machine (localhost, `*.localhost`, 127.0.0.0/8, ::1) but only the provider type's own variable (`ELEVENLABS_API_KEY`, `OPENAI_API_KEY`, `CARTESIA_API_KEY`, `KOKORO_API_KEY`; a variable chosen with `api_key_env`, and any key for a private-network host such as RFC 1918, link-local, fc00::/7 or `*.local`, needs the host listed, because a timeline is data and must not pick which secret goes to whatever listens there), or to hosts listed in the environment variable `SONICCRAFT_ALLOWED_HOSTS` (comma-separated host names, `*.example.com`, or `*` for any; set it in soniccraft's environment, never in the timeline). Any other host is refused before anything is sent (`provider_unavailable` in validate, also offline); regional endpoints such as api.eu.residency.elevenlabs.io must be listed. Requests without a key may go anywhere. A value that is an unexpanded placeholder (`${VAR}`, `${VAR:-…}`, `$VAR`, `${env:VAR}`, `%VAR%`) counts as unset (`missing_credentials`, with a hint that the MCP client passed its config template through). |
| `base_url` | (HTTP providers) Override the API base. Defaults: `https://api.elevenlabs.io`, `https://api.openai.com/v1` (any OpenAI-compatible `/audio/speech` server works, e.g. Kokoro-FastAPI, Speaches, LocalAI), `https://api.cartesia.ai`, kokoro: env `KOKORO_BASE_URL` or `http://localhost:8880`. A password in the URL is `secret_in_timeline`. |
| `model` | Default model for this instance. |
| `output_format` | Provider-native output format override (e.g. ElevenLabs `pcm_44100`; default `pcm_24000`). |
| `max_concurrency` | Parallel requests for this instance. Defaults: elevenlabs 2, openai 4, cartesia 2, kokoro 1, piper 2, macos_say 4, command 2, mock 8. |
| `timeout_ms` | Time limit of one cue's synthesis (default 120000): every attempt, the waits between retries and the whole response, an SSE stream included, must fit in it. |
| `chars_per_second` | Speaking-rate assumption for dry-run duration estimates (§5.3); preferred. |
| `words_per_minute` | Alternative speaking-rate assumption (used only when `chars_per_second` is not set). |
| `settings` | Default voice settings for cues using this instance. |
| `command` | (`command`/`piper`) argv template, e.g. `["piper", "-m", "{voice_id}", "-f", "{output}"]`. |
| `stdin` | (`command`/`piper`) template written to the child's stdin, e.g. `"{text}"`. Omit for none. |
| `voices_dir` | (`piper`) directory searched first for `<voice_id>.onnx`, relative to the timeline directory. A bare voice name is looked up in `voices_dir`, then `$PIPER_VOICES_DIR` (relative to the timeline directory when relative), then the timeline directory itself, then `~/.local/share/piper/voices`. A `voice_id` that contains `/` or ends in `.onnx` is a model path (relative to the timeline directory). When no model is found, the error (`unknown_voice`, already in validate) lists every searched directory and the `python3 -m piper.download_voices <name> --data-dir <dir>` command. |

Template placeholders: `{text}`, `{text_file}` (path of a UTF-8 temp file containing the text), `{output}`
(path the command must write; any decodable audio format), `{voice_id}`, `{model}`, `{language}`.

Cue text and voice ids come from the timeline: pass text through `{text_file}` or `stdin: "{text}"`, not as an
argument of its own. SonicCraft refuses to run a command when an argument that begins with `{text}`,
`{voice_id}`, `{model}` or `{language}` would start with `-` (`synthesis_failed`), and `validate` warns
(`suspicious_value` at `providers.<name>.command[<i>]`) about templates that pass `{text}` or `{voice_id}` as an
argument of its own. Child processes (piper, macos_say, command) run in the timeline directory, in a process
group of their own (killed as a whole on timeout or cancellation), without `ELEVENLABS_API_KEY`,
`OPENAI_API_KEY`, `CARTESIA_API_KEY`, `KOKORO_API_KEY`, `SONICCRAFT_ALLOWED_HOSTS` or any `api_key_env` of the
timeline in their environment.

`command` providers and piper instances with a custom `command` run local programs. Engines that do not allow
them (the MCP server unless started with `--allow-command` / `SONICCRAFT_ALLOW_COMMAND=1`) report their cues
as `provider_unavailable` (an error; a warning when every cue is cached).

**Secrets never live in timelines.** Error `secret_in_timeline`, with a hint to use `api_key_env`, for: any
key named `api_key`, `apikey`, `token`, `secret`, `password` anywhere in the document (case, separators and an
`x-`/`x_` annotation prefix ignored: `apiKey`, `client_secret`, `x-api-key`); any key shaped like a provider
key; any string value that looks like a provider key (`sk-…`, `sk_…` with 16+ key characters); and in
provider-bound strings (a `providers` entry or a `settings` map) such a key anywhere inside the string (after
`Bearer `, in `--api-key=…`, in a URL), a bearer token, 32+ hex characters, a password in a URL userinfo
(`https://user:pass@host`) or a secret URL query parameter (`?api_key=…`). Secrets are never repeated in
diagnostics (messages, hints, paths, related ids); `soniccraft_core::parse::secret_spans` gives their source
ranges, which the CLI uses to print them as `<redacted>` in source snippets.

### 2.9 Tracks — common fields

| Field | Type | Default | Semantics |
|---|---|---|---|
| `id` | id | required | Unique across tracks, cues and clips. |
| `type` | `speech` \| `music` \| `sfx` | required | |
| `volume` | linear | `1.0` | Track fader. |
| `gain_db` | dB | `0` | Added to the fader: track gain = `volume × 10^(gain_db/20)`. |
| `pan` | [-1, 1] | `0` | Constant-power pan for mono sources, balance for stereo sources (§4.4). |
| `mute` | bool | `false` | The track is laid out (timing is stable) and synthesized, but not mixed. |
| `normalize` | bool \| `{target_lufs: n}` | speech & music `true`, sfx `false` | Source-level loudness normalization (§4.2). |
| `start_at_ms` / `start_relative` | Millis / Anchor | `0` | Start of the track's first item when that item has no explicit start. Mutually exclusive. |
| `ducking` | DuckingSpec | none | §2.12. Allowed on any track type. |

### 2.10 Speech tracks (`type: speech`)

Additional fields: `sequence` (required, non-empty), voice fields (`speaker`, `provider`, `voice_id`,
`model`, `language`, `settings`), `trim_silence` (bool | `{threshold_db: -50, padding_ms: 30}`, default `true`).

Fields only valid on audio tracks (`source`, `clips`, `loop`, fades, `trim_start_ms`, `duration_ms`,
`end_at_ms`, `end_relative`, `loop_crossfade_ms`, `fade_curve`) are `field_not_applicable` errors on speech
tracks, and vice versa (`sequence`, `trim_silence`, voice fields on music/sfx). `crossfade_ms` exists only in
`clips` entries (`unknown_field` on any track, with a hint to use a `clips` list).

`sequence` items are either a **pause** or a **cue**:

- **Pause:** `{pause_duration: Millis}` or `{pause: Millis}` and nothing else.
- **Cue:**

| Field | Semantics |
|---|---|
| `id` | Optional; default `<track_id>.cue<n>` (1-based count of cues in that track). |
| `text` | Required, non-empty after trimming. What the provider speaks. |
| `caption` | Optional subtitle text override (e.g. text spells "S Q L" phonetically, caption shows "SQL"). |
| `subtitles` | `false` excludes the cue from subtitle files. Default `true`. |
| `speaker`, `provider`, `voice_id`, `model`, `language`, `settings` | Voice overrides (§2.7). |
| `start_at_ms` / `start_relative` | Explicit start (mutually exclusive). Without them the cue is *implicit* (sequential). |
| `volume`, `gain_db` | Per-cue gain on top of the track fader. |
| `allow_overlap` | `true` exempts this cue from overlap handling (intentional crosstalk). |

A mapping containing both pause and cue fields is `conflicting_fields`. An item made only of misspelled pause
keys (`{pause_ms: 300}`, `{wait: 1s}`) is reported once as `unknown_field` (did-you-mean `pause`) and treated as
a pause, not as a cue without text.

In YAML, an unquoted ` #` inside a value starts a comment: a plain `text` or `caption` that continues with ` #`
is cut short there (`text: Tip #1: duck the bed` speaks only "Tip"), which is the warning `comment_in_text` with
the quoted line as hint.

### 2.11 Music and sfx tracks (`type: music` / `type: sfx`)

An audio track has either the **single-clip shorthand** (clip fields directly on the track) or a `clips`
list — not both (`conflicting_fields`; hint: move the fields into each clip). Clip fields:

| Field | Default | Semantics |
|---|---|---|
| `id` | `<track_id>.clip<n>` (shorthand clip: the track id itself is the anchor) | |
| `source` | required | Audio file path relative to the timeline directory (wav, mp3, flac, ogg, m4a/aac, aiff). Sources must have a sample rate of 1000–768000 Hz (`asset_unreadable` at validate, `decode_failed` at render). Sources wider than stereo are mixed down (§4). Non-finite samples (NaN, ±∞) in float files, TTS audio and cache entries are replaced with silence (warning `audio_repaired`). An MP3 without a Xing/Info/VBRI header is decoded to its real end and counted by decoding, so plan and render agree. |
| `start_at_ms` / `start_relative` | sequential | First clip: track start. Later clips: end of the previous clip minus its `crossfade_ms`. |
| `trim_start_ms` | `0` | Offset into the source where playback begins. |
| `duration_ms` / `end_at_ms` / `end_relative` | natural | At most one (`conflicting_fields`). See §3.3. |
| `loop` | `false` | Loop the source (from `trim_start_ms`) to fill the clip length. |
| `loop_crossfade_ms` | `0` | Equal-power crossfade at each loop seam (for non-seamless loops). |
| `fade_in_ms`, `fade_out_ms` | `0` | Fades at the clip's start/end. |
| `fade_curve` | `equal_power` | `linear` \| `equal_power` \| `exponential`. |
| `crossfade_ms` | `0` | (clips list only) Overlap with the previous clip, equal-power fades on both sides. Only a clip that follows the previous clip sequentially (no `start_at_ms`/`start_relative`) crossfades; on a clip with its own start it is ignored with warning `field_not_applicable`. |
| `volume`, `gain_db` | `1.0`, `0` | Per-clip gain on top of the track fader. |

### 2.12 Ducking (`ducking:` on a track)

| Field | Default | Semantics |
|---|---|---|
| `trigger_tracks` | required, non-empty | Track ids whose activity ducks this track. Unknown → `unknown_reference`; itself → `self_reference`. |
| `reduction_db` | `-15` | Depth. The sign is ignored: `-18` and `18` both mean "attenuate by 18 dB". |
| `attack_ms` | `80` | Time to ramp from 0 dB to full depth (linear in dB). |
| `release_ms` | `400` | Time to ramp from full depth back to 0 dB. |
| `hold_ms` | `250` | Activity gaps shorter than this keep the duck engaged; activity is also held this long after it stops. |
| `lookahead_ms` | = `attack_ms` | Activity is detected this much earlier, so the bed is fully ducked when speech begins. |
| `threshold_db` | `-40` | dBFS RMS threshold for `sidechain` mode. |
| `mode` | `sidechain` | `sidechain` (signal-driven) or `cue` (timeline-driven from trigger items' intervals). |

### 2.13 Anchors (`start_relative`, `end_relative`)

```yaml
start_relative: { after_cue: cue_01, offset_ms: 150 }   # start = end(cue_01) + 150ms
start_relative: { with_cue: cue_01, offset_ms: -200 }   # start = start(cue_01) - 200ms
start_relative: { before_cue: cue_02, offset_ms: 0 }    # this item ENDS at start(cue_02) + offset
end_relative:   { after_cue: cue_03, offset_ms: 2000 }  # clip ends 2 s after cue_03 ends
end_relative:   { with_cue: cue_03 }                    # clip ends when cue_03 starts
```

Exactly one of `after_cue` / `with_cue` / `before_cue` (`before_cue` is invalid in `end_relative`).
`offset_ms` is a signed Millis, default 0. The referenced id may be a cue, a clip or a **track**. A track
anchor resolves in declaration order: `with_cue`/`before_cue: <track>` use the start of the track's first
declared item and `after_cue: <track>` the end of its last declared item (pauses included). When another item
of that track starts earlier or ends later, warning `track_anchor_out_of_order` names both and suggests
anchoring to the item (`after_cue: outro`) or reordering the sequence. Referencing the end of an open-ended
item (§3.3) is `anchor_open_ended`. Self-references are `self_reference`; cycles are `dependency_cycle`
(message lists the cycle, e.g. `cue_02 → cue_03 → cue_02`; a step through a track anchor names the item it
resolves to: *outro → sting → voice (track anchor: last item `outro`)*). A clip's start and end are separate
events (§3.1), so items may anchor to the start of a bed whose `end_relative` waits for them. A resolved start
< 0 is clamped to 0 with warning `negative_start`.

---------------------------------------------------------------------------------------------------------

## 3. Timeline geometry (layout engine)

`soniccraft_layout::layout` is a pure function of the lowered program and a duration table. It is used twice: by dry
runs (estimated durations) and by renders (exact durations). Everything is in frames:
`frames(ms) = round(ms × sample_rate / 1000)`.

### 3.1 Nodes

Every cue, pause and clip is a node. Each node has a *start rule*: `Sequential`, `Absolute(ms)` or
`Relative{anchor, edge, offset}` and a duration (cues: synthesized/estimated audio length after silence
trim; pauses: their Millis; clips: §3.3).

The start and the end of a node are separate scheduling events. A clip whose `end_relative` waits for other
items is placed as soon as its start is known, and its end is resolved when those items are placed, so items
may be anchored to the start of a bed whose end follows them (`dependency_cycle` is reported only for real
cycles between starts and ends).

### 3.2 Sequential semantics

Within a track, a cursor starts at the track start (track `start_at_ms`/`start_relative`, default 0).
Items are visited in order: a pause advances the cursor; an implicit cue/clip starts at the cursor; an
explicit one starts where its rule says. After every cue/clip the cursor becomes its end (for clips: end
minus the *next* clip's `crossfade_ms`).

### 3.3 Clip length

`natural = source_frames − trim_start_frames` (≥ 0; `trim_start_ms` beyond the source end → error
`invalid_value`).

| Specified | Not looping | Looping |
|---|---|---|
| nothing | natural | **open-ended**: until the timeline end |
| `duration_ms` | min(duration, natural) (+ info `clip_shorter_than_requested` if cut short) | duration |
| `end_at_ms` / `end_relative` | min(end − start, natural) (same info) | end − start |

End before start → error `invalid_value`. Open-ended clips do not contribute to the content end and their
end cannot be referenced. If `fade_in + fade_out > length` both are scaled proportionally (info `fades_scaled`).

### 3.4 Scheduling and the speech-overlap policy

The scheduler processes nodes in a deterministic dependency order. When several nodes are ready, it picks:
(1) nodes with an explicit start (Absolute/Relative) before sequential ones, then (2) the smallest tentative
start, then (3) declaration order (track index, item index).

**Overlap:** two speech cues (any speech tracks, muted included) whose intervals intersect by more than
`overlap_tolerance_ms`, unless either has `allow_overlap: true`.

**`push` (default):** when an *implicit* cue is scheduled and its tentative interval overlaps an already
scheduled speech cue, it moves to `end(other) + gap`, where `gap` = the sum of the pauses directly preceding
it in its sequence, or `timing.min_gap_ms` if there are none. Repeat until it overlaps nothing; never move
earlier than the cursor. Each pushed cue emits one info `cue_pushed` (`from` = its tentative start, `to` = its
final start, and the cue it finally waits for); after 50 of them one summary info counts the rest. Explicit
cues are never moved. After scheduling, if an overlap remains between an implicit cue and an explicit cue
scheduled after it that does not depend on it, re-run scheduling with the previous pass's explicit-cue
intervals pre-seeded as obstacles (max 4 passes). An explicit cue never pushes an implicit cue it depends on
(directly or through other items): it would move with it. The best pass is kept (fewest remaining overlaps,
then the shortest total overlap, then the least push displacement, then the earliest). Remaining overlaps →
warning `speech_overlap`. Pushing costs a few logarithmic queries per cue, whatever the number of colliding
cues.

**`warn`:** nothing moves; overlaps → warning `speech_overlap`. **`error`:** overlaps → error (render aborts).
**`allow`:** nothing is reported.

Every `speech_overlap` diagnostic states both cues, both intervals and the overlap length, and gives a
mechanical fix, e.g. *"set `start_relative: {after_cue: cue_02, offset_ms: 350}` on `cue_03`, or
`allow_overlap: true` for intentional crosstalk"*.

Worked example (the README timeline): `cue_01` [0, d1); `cue_02` explicit `after cue_01 + 150ms`;
`cue_03` is implicit after a 350 ms pause → tentative start d1+350ms overlaps `cue_02` → pushed to
`end(cue_02) + 350ms` (the preceding pause). Result: a natural three-turn exchange.

### 3.5 Content end, timeline end

`content_end` = max end over all finite nodes (cues, pauses, non-open-ended clips).
`total = output.duration_ms` if set, else `content_end + tail`, where `tail = output.tail_ms` if set, else
the max `fade_out_ms` of open-ended clips (0 if none). An open-ended clip without a fade-out gets info
`no_fade_out_on_bed`. Open-ended clips end at `total`; their fade-out ends at `total`.

A total longer than 24 h (`soniccraft_core::MAX_TIME_MS`) is error `timeline_too_long`, naming the item that
ends last (or the tail, or `output.duration_ms`); the reported total is capped at 24 h. A total of 0 is error
`invalid_value` (with the hint to give the looping bed a `fade_out_ms`, or set `output.tail_ms` /
`output.duration_ms`, or add finite content), so validate, plan and render agree.

Every layout diagnostic records whether its condition depends on estimated durations (§5.3). Static ones
(cycles, references, conflicting fields, `anchor_open_ended`, trim and loop source errors,
`field_not_applicable`) never do.

---------------------------------------------------------------------------------------------------------

## 4. Signal chain

All processing is at the output sample rate in f32. Speech is processed mono; tracks render to the output
channel count.

Decoding (clip sources, provider audio, cache entries) accepts sample rates of 1000–768000 Hz and replaces
non-finite samples (NaN, ±∞) with silence (warning `audio_repaired`). Sources wider than stereo are mixed down
to stereo at decode with ITU-R BS.775: `Lo = L + 0.7071·C + 0.7071·Ls`, `Ro = R + 0.7071·C + 0.7071·Rs`, LFE
dropped. Front-of-centre and wide channels count as fronts; side, back and top left/right channels as
surrounds (0.7071); back/top centres go 0.5 into each side; channels without a position are dropped. The
layout comes from the file (WAV channel mask, the codec's channel map), else the WAVE order
`L R C LFE Ls Rs` is assumed. Info `channels_downmixed`. Mono output takes `(Lo + Ro)/2`.

### 4.1 Per speech cue

1. Synthesize (or load from cache) → decode → downmix to mono.
2. Resample to the output sample rate (rubato, high-quality sinc, latency-compensated, exact length
   `round(in_frames × out_sr / in_sr)`).
3. Trim silence (if enabled): 5 ms RMS frames, first/last frame above `threshold_db` (default -50 dBFS),
   keep `padding_ms` (default 30 ms) on each side. Word timings shift accordingly.
4. Source normalization to the track's target (§4.2).
5. Gain: cue `volume × 10^(gain_db/20)` × speaker `gain_db`.

### 4.2 Source-level normalization

When enabled for a track, every cue (speech) or every distinct clip source (music/sfx; measured once on the
whole decoded source) is gained so its integrated loudness (BS.1770 / EBU R128, gated) equals the target
(`normalize.target_lufs` or `mix.reference_lufs`, default -20 LUFS). Gain is capped at +24 dB. Sources too
short or too quiet to measure (integrated loudness undefined) are left unchanged with info
`normalization_skipped`. This is what makes cues from different providers sit at the same level.

### 4.3 Clip rendering

A clip plays `length` frames starting at timeline frame `start`, reading the source from `trim_start`,
wrapping to `trim_start` when `loop` is set (with optional equal-power `loop_crossfade` at seams). Fades are
applied relative to the clip's own start/end. Frames before 0 or after `total` are discarded.

Fade curves for `t ∈ [0,1]` (fade-in gain; fade-out uses `t' = 1 − t`): linear `t`; equal_power
`sin(t·π/2)`; exponential `(10^(t·60/20) − 1) / (10^3 − 1)` (a 60 dB exponential ramp).

### 4.4 Pan

Mono → stereo, constant power: `θ = (pan + 1)·π/4`, `L = cos θ`, `R = sin θ` (center = -3 dB per channel,
which preserves BS.1770 loudness). Stereo balance: `pan < 0` scales R by `1 + pan`; `pan > 0` scales L by
`1 − pan`. Mono output: stereo sources are downmixed `(L + R)/2`, pan ignored.

### 4.5 Track stems and ducking

stem = Σ placed items → pan → × track gain (0 when muted). Ducking is applied to the stem afterwards.

Ducking detector (per ducked track):
1. **sidechain:** detector = mono sum `(L+R)/2` of the trigger tracks' **pre-fader** content (after per-cue
   normalization and per-cue gain, before track volume/pan/mute; muted trigger tracks are excluded with
   info `muted_trigger`). Levels are computed on 1 ms blocks as RMS over a sliding 10 ms window, in dBFS.
   A block is *active* when its level > `threshold_db`.
   **cue:** a block is active when it intersects any trigger track item interval.
2. Hold: gaps between active runs shorter than `hold_ms` become active; each active run is extended by
   `hold_ms` at its end.
3. Lookahead: each active run starts `lookahead_ms` earlier (clamped at 0).
4. Target gain per block: active → `-|reduction_db|` dB, else 0 dB.
5. Per-sample gain moves toward the target linearly in dB: downward slope `|reduction_db| / attack_ms`,
   upward slope `|reduction_db| / release_ms`. Convert to linear and multiply the stem.

Reports expose the active regions (after hold and lookahead) as `[start_ms, end_ms)` intervals plus the
depth, per ducked track. Dry runs compute them from estimated item intervals (as `cue` mode does) whatever
the configured mode, by interval arithmetic and without per-millisecond arrays (a cost independent of the
timeline's length); validation computes none. The report's `mode` field is the configured mode.

### 4.6 Master

1. master = Σ stems in track declaration order.
2. De-click: if the last 10 ms are not silent, apply a 10 ms linear fade-out at the very end.
3. If `normalize`: measure integrated loudness `I`; apply gain `G = target − I`; run the true-peak limiter at
   `true_peak_db`; re-measure `I'`. While `|I' − target| > 0.2 LU`, try another pre-limiter gain on the
   pre-limiter signal, at most 4 passes in all. Until the target is bracketed, the gain moves by
   `target − I'` divided by the measured slope `dI'/dG` (clamped to [0.25, 1]). Once a pass below and a pass
   above the target exist, the next gain interpolates linearly between them, and bisects when the estimate
   leaves the bracket or the same end moved twice in a row. This keeps BS.1770 gating jumps from making the
   loop oscillate. The output is the pass closest to the target. A pass whose loudness is undefined (below
   the −70 LUFS gate) is never chosen over a defined one. If no pass is within 0.2 LU (and the target is
   reachable) → warning `loudness_target_missed` with the achieved value and a hint to set `target_lufs` to
   it. The target is unreachable when it needs more than 6 dB of average reduction (`(I + G) − I' > 6.05 dB`).
   Once such a pass is still below the target, the loop instead looks for the loudest master within the
   limit: it starts at the limiter onset (`true_peak_db − 0.1 − TP(mix)`) + 6 dB, follows the secant of the
   reduction, and stops within 0.2 dB of the limit. That search does not depend on the target, so a louder
   target never gives a quieter master → warning `loudness_target_unreachable` with the achieved value (the
   hint names it as the target to use). A silent master → warning `silent_output`, no gain.
4. If not `normalize`: limiter only (clip guard at `true_peak_db`).

### 4.7 True-peak limiter (offline, linked)

`ceiling = 10^((true_peak_db − 0.1)/20)`. For each frame `n`: `peak[n]` = max over channels of the 4×
oversampled magnitude around `n` (polyphase windowed-sinc interpolator, ≥ 32 taps/phase);
`req[n] = min(1, ceiling / peak[n])`. Lookahead `L` = 5 ms: `h[n] = min(req[n .. n+L))` (sliding-window
minimum); `s[n] = mean(h[n−L+1 ..= n])` (guarantees `s ≤ req` at every peak); release:
`g[n] = min(s[n], g[n−1] + (1 − g[n−1])·(1 − e^(−1/(release·sr))))` with release 80 ms. Apply `g` to all
channels. Verify with an independent true-peak measurement (ebur128 4× oversampling); if still above
`true_peak_db`, lower the ceiling by the excess + 0.05 dB and redo (max 4 times).

The limiter streams and works in place. Verification passes measure their output block by block without
storing it; only the accepted pass is written over the input, and its measurement (integrated, LRA, true
peak, sample peak) is what the report shows. Mastering therefore needs no memory proportional to the mix
beyond the mix itself. Long operations poll a cancellation callback.

### 4.8 Encoding

wav (hound: 16/24-bit int, 32-bit float), mp3 (LAME CBR via mp3lame-encoder, mono/stereo), flac
(16/24-bit), ogg/opus/m4a via `ffmpeg -f wav -i pipe:0 …` (stdin WAV, 32-bit float). Quantization to
integer PCM uses TPDF dither (fixed-seed xorshift) at 16-bit when `dither` is true; 24-bit is rounded
without dither. Samples are clamped to [-1, 1) before quantization.

Lossy formats (mp3, ogg, opus, m4a): the encoded file is decoded and measured with the same BS.1770 meter. If
its true peak exceeds `true_peak_db`, the master is limited again with the ceiling lowered by the overshoot
+ 0.1 dB; otherwise, if its integrated loudness is more than 0.2 LU off a target the master had reached
(codecs shift loudness by about 0.1-0.4 LU), the difference is applied to the master as gain, limited to the
current ceiling. Either way it is encoded again, at most 3 extra passes; the peaks win over loudness. A file
still above the ceiling gets warning `true_peak_exceeded`; one still more than 0.2 LU off a target the master
had reached gets `loudness_target_missed`. The report's loudness block describes the delivered file, and `pre_encode` the
master that went into the encoder (§8). If the encoded file cannot be decoded, warning `decode_failed` says so
and the report shows the master.

---------------------------------------------------------------------------------------------------------

## 5. TTS

### 5.1 Request → cache key

The cache key is `blake3(canonical JSON)` of
`{cache_version, provider_type, api_version (e.g. Cartesia-Version), base_url (kokoro/openai-compatible/command/piper only when not the default), model, voice_id, language, text, settings, output_format, command and stdin (command/piper only), program (command/piper: the resolved program path and a BLAKE3 hash of its contents), files (command and custom-command piper: hashes of the files that literal arguments name, relative to the timeline directory), base_dir (command, and piper with a custom command), model_file (piper: the resolved .onnx path and hashes of it and its .onnx.json)}`.
Canonical JSON means: object keys sorted recursively (the workspace enables `serde_json/preserve_order`, so sorting must be
explicit), floats normalized (`-0.0` → `0.0`, non-finite rejected, at most 6 decimals), no insignificant whitespace.
Cache directory: CLI `--cache-dir` > env `SONICCRAFT_CACHE_DIR` > `<timeline dir>/.soniccraft/cache`. Each entry
is `<key>.wav` (32-bit float WAV of the decoded provider audio at its native rate, untrimmed) and
`<key>.json` (metadata incl. word timings if the provider returned them). Writes are atomic (temp + rename):
temporary cache files get random names and are created exclusively (O_EXCL), then renamed into place, so a
planted file or symlink is never written through. A cache that cannot be written (read-only directory, full
disk) does not fail the render: warning `cache_write_failed` (once per render, naming the cues) says that later
renders will synthesize them again.

### 5.2 Providers

| Type | Transport | Word timings |
|---|---|---|
| `elevenlabs` | HTTPS `POST /v1/text-to-speech/{voice_id}/with-timestamps?output_format=pcm_24000` (`xi-api-key`); on 403 `subscription_required`/`feature_not_available` or 400 `invalid_output_format` for a configured format, fall back to `pcm_24000`. `eleven_v4*` models accept only `stability`/`similarity_boost` (style/speed are dropped with an info diagnostic). | yes (character alignment → words; `[audio tags]` dropped) |
| `openai` | HTTPS `POST {base}/audio/speech` (`Authorization: Bearer`), `response_format: pcm` (24 kHz s16le mono, headerless). Generic OpenAI-compatible client; OpenAI's own TTS models retire 2027-01-06. Never send provider-specific extra fields to api.openai.com. | no (estimated) |
| `cartesia` | HTTPS `POST /tts/sse` (`Authorization: Bearer`, `Cartesia-Version: 2026-08-14`), raw `pcm_f32le` at 48000, `add_timestamps: true`; dispatch SSE frames on the JSON `type` (`chunk`, `timestamps`, `done`, `error`). Default model `sonic-3.6`. | yes (word timestamps, batched) |
| `kokoro` | HTTP Kokoro-FastAPI: `POST /dev/captioned_speech` (`stream: false`, `response_format: pcm`, 24 kHz s16le), fallback `POST /v1/audio/speech` on 404 | yes when the captioned endpoint exists (punctuation tokens folded into the previous word) |
| `piper` | child process `piper -m <model.onnx> -f <tmp.wav> [-s N] [--length_scale X]`, text on stdin (portable across rhasspy/piper and piper1-gpl); the model is found as described under `voices_dir` (§2.8), and a missing model is `unknown_voice` already in validate | no (estimated) |
| `macos_say` | child process `say -v <voice> [-r <rate>] -o <tmp>.wav --data-format=LEI16@48000 -f <textfile>` (unknown voices silently fall back to the default voice, so the voice is validated against `say -v '?'`: `unknown_voice` with did-you-mean, already in validate) | no (estimated) |
| `command` | child process from the argv template | no (estimated) |
| `mock` | in-process deterministic synthetic voice (vowel-like tone bursts per word) | yes (exact) |

All WAV parsing must be tolerant: skip unknown chunks (`FLLR`, `LIST`, `JUNK`), accept `data` sizes of `0`/`0xFFFFFFFF`/too large
(use the remaining bytes), accept format tags 1, 3 and 0xFFFE.

HTTP: retries on network errors, 408, 429, 500, 502, 503, 504 with exponential backoff (1 s, 2 s, 4 s;
`Retry-After` in seconds or as an HTTP-date, or `retry-after-ms`, honoured up to 60 s), max 3 retries, all
within the cue's `timeout_ms`. A connection refused by a server on this machine or a private network (or a DNS
failure for such a host) is not retried, and that instance's remaining cues then fail at once ("not attempted:
<instance> unreachable at <url>"). Redirects are never followed. Responses are capped at 256 MiB of audio
(512 MiB per SSE stream, 16 MiB per SSE line), 16 MiB of JSON metadata, and the first 1 MiB of an error body.
Audio longer than 10 minutes plus 20 times the cue's estimate is refused as implausible (not for `command`).
API keys echoed by a server and URL credentials are removed from every message. Other 4xx fail fast; the
provider's error message is surfaced in the `synthesis_failed` diagnostic with a hint (bad key → check env
var; unknown voice → list voices). Concurrency: global `jobs` limit (default 4) and per-instance
`max_concurrency`. Local servers (Kokoro-FastAPI, OpenAI-compatible servers on this machine or a private
network) are probed with a 300 ms TCP connect (once per URL) by validate, plan and `soniccraft providers`; one
that does not answer is `provider_unavailable` ("no Kokoro-FastAPI server answered at http://localhost:8880").

### 5.3 Duration estimates (dry run)

Characters per second (counting letters, digits, spaces and punctuation; excluding `[audio tags]`) is more stable than
words per minute, so: `seconds = spoken_chars / cps / speed`, where `speed` = `settings.speed` (piper: multiply by
`settings.length_scale` instead; macos_say: `speed = settings.rate / 175` when `rate` is set). Default cps: elevenlabs 15.5
(`eleven_v3`/`eleven_v4*`: 14), openai 15, cartesia 15, kokoro 16, piper 17, macos_say 17, command 15. A provider instance
may set `chars_per_second`, or `words_per_minute` (then `seconds = words / (wpm/60) / speed`). Dry runs render uncached
`mock` cues in-process exactly as a render does, so their lengths are exact (`duration_source: exact`). Cached cues use
their exact cached duration (`duration_source: cache`). Estimates are expected to be within ±15 %; the overlap policy and
ducking are recomputed exactly at render time.

Layout errors whose condition depends on estimated lengths are warnings in validate and plan, their message ending in
"(based on estimated speech durations; the render checks again with exact durations)". A render lays the timeline out
with these dry-run lengths after decoding the clip sources, and stops before any TTS request on layout errors that do not
depend on estimates and on the memory budget (§9.3).

### 5.4 Word timings for subtitles

Provider timings are mapped onto the cue's display text tokens. Without provider timings, words are
estimated: detect speech regions in the trimmed cue audio (gaps ≥ 120 ms below -45 dBFS), split the text
into phrases at punctuation, assign phrase boundaries to the gaps that best match the proportional estimate
(in order), and distribute words inside each segment proportionally to a syllable-based weight. Without
audio (dry run), distribute over the whole cue. When `caption` is set, its words are distributed across the
cue using the same method.

---------------------------------------------------------------------------------------------------------

## 6. Subtitles

Per cue (never merging cues): greedily add words to the current caption until adding the next word would
exceed `max_lines × max_chars_per_line` (with line breaking), or the caption would exceed
`max_duration_ms`; prefer breaking after sentence-ending punctuation once a caption has ≥ 50 % of its
capacity. Lines are balanced (break at the space closest to the middle, preferring after punctuation).
Caption start = first word start, end = last word end, extended toward `min_duration_ms` without
overlapping the next caption from the same track. Speaker labels: WebVTT `<v Label>`; SRT `Label: ` prefix on
the first line. Timestamps: WebVTT `HH:MM:SS.mmm`, SRT `HH:MM:SS,mmm`, rounded to the nearest ms. The JSON
export contains `{version, captions: [...], words: [{text, start_ms, end_ms, cue_id, track_id, speaker}]}`.

---------------------------------------------------------------------------------------------------------

## 7. Diagnostics

```json
{ "severity": "error|warning|info", "code": "unknown_reference", "message": "...",
  "path": "tracks[2].sequence[0].start_relative.after_cue", "line": 37, "column": 21,
  "hint": "Did you mean `cue_01`? Known ids: cue_01, cue_02, cue_03", "related": ["cue_01"] }
```

Paths use `field.sub[index]` notation. Codes (stable API):

- **parse/schema:** `yaml_syntax`, `json_syntax`, `invalid_type`, `unknown_field`, `missing_field`,
  `unsupported_version`, `invalid_value`, `field_not_applicable`, `conflicting_fields`, `empty_tracks`,
  `empty_sequence`, `empty_text`, `duplicate_id`, `invalid_id`, `secret_in_timeline`, `comment_in_text`
  (warning: an unquoted ` #` cut a cue's `text` or `caption` short), `suspicious_value` (warning: a valid value
  that is very likely a mistake, e.g. seconds written as milliseconds, `duration_ms: 30`).
- **references:** `unknown_reference`, `self_reference`, `dependency_cycle`, `anchor_open_ended`,
  `unknown_provider`, `missing_provider`, `missing_voice`, `unknown_speaker` (error for a misspelled speaker,
  else warning), `default_voice`, `speakers_share_voice` (warning: two or more speakers resolve to the same
  voice).
- **output:** `unsupported_sample_rate`, `unsupported_bit_depth`, `bitrate_ignored`, `bitrate_adjusted`,
  `format_extension_mismatch`, `format_requires_ffmpeg`.
- **environment (engine):** `asset_not_found`, `asset_unreadable`, `missing_credentials`,
  `provider_unavailable`, `text_too_long`, `unknown_voice` (a macOS voice that is not installed, a piper model
  found nowhere; an error unless every cue with it is cached), `suspicious_value` (warning: a `command` element
  that is `{text}`/`{voice_id}` on its own), `timeline_too_long` (the render memory budget,
  `SONICCRAFT_MAX_RENDER_GB`, default 8 GB: an error at render, before synthesis, a warning in dry runs).
  `provider_unavailable` also covers keys the key-sending host policy refuses (§2.8), local programs an engine
  does not allow, and local servers that do not answer a 300 ms probe. The asset checks run even when the
  document has errors.
- **layout:** `speech_overlap`, `cue_pushed`, `negative_start`, `content_truncated`,
  `clip_shorter_than_requested`, `fades_scaled`, `no_fade_out_on_bed`, `muted_trigger`,
  `track_anchor_out_of_order` (warning: a track anchor resolves to the first/last declared item, but another
  item of the track starts earlier or ends later), `timeline_too_long` (error: the total exceeds 24 h). The
  layout also emits `field_not_applicable` (warning: `crossfade_ms` on a clip with its own start) and
  `invalid_value` (an empty timeline, a clip end before its start, `trim_start_ms` beyond the source).
- **render:** `synthesis_failed`, `normalization_skipped`, `loudness_target_unreachable`, `silent_output`,
  `alignment_estimated`, `encode_failed`, `decode_failed`, `loudness_target_missed` (warning: mastering stopped
  outside 0.2 LU of a reachable target, or lossy compensation cost more than 0.2 LU), `true_peak_exceeded`
  (warning: the encoded lossy file is still above the ceiling), `cache_write_failed` (warning, once per render:
  the TTS cache could not be written), `audio_repaired` (warning: non-finite samples replaced with silence),
  `channels_downmixed` (info: a source with more than two channels was mixed down, §4).
- **provider notes:** `setting_ignored` (info): a provider ignored or replaced a setting (e.g. `eleven_v4`
  drops `style`/`speed`; an ElevenLabs plan without `pcm_44100` falls back to `pcm_24000`; a settings key the
  provider does not read: "macos_say ignores `voice`; it reads `rate`, `speed`"). TTS cache-write failures are
  `cache_write_failed` (warning), not provider notes.

Missing credentials are an error only for cues that are not cached (cached cues need no API calls); a
provider whose every cue is cached gets a warning.

---------------------------------------------------------------------------------------------------------

## 8. Reports

All engine results are serde-serializable and identical between CLI `--json` and MCP.

- **ValidationReport** `{valid, diagnostics, summary:{tracks, speech_cues, clips, speakers, providers:[...],
  characters, cached_cues, output_format, sample_rate, target_lufs}}` (`summary` only when the document lowers)
- **PlanReport** `{valid, diagnostics, duration_ms, estimated, sample_rate, tracks:[{id, type, gain_db, pan, mute,
  ducked_by}], items:[{id, kind:"cue|pause|clip", track_id, start_ms, end_ms, duration_ms,
  duration_source:"exact|cache|estimate|source|fixed", speaker, provider, voice_id, text, pushed_from_ms, source,
  open_ended, looped}], ducking:[{track_id, mode, reduction_db, triggers, regions:[{start_ms,end_ms}]}],
  synthesis:{cues, cached, characters_to_synthesize, by_provider:{name:{provider_type, cues, cached, characters}}},
  diagram}`. `duration_source: exact` means measured in this render, or the mock voice rendered in-process by a
  plan; `estimated` is false when no item is an estimate. The diagram is cut after 256 KB, with a note, for very
  large timelines (`items` still lists every item).
- **RenderReport** `{output:{path, format, sample_rate, channels, bit_depth, bitrate_bps, duration_ms, bytes},
  loudness:{integrated_lufs, true_peak_dbtp, sample_peak_dbfs, loudness_range_lu, target_lufs,
  true_peak_target_dbtp, normalized, gain_db, limiter_max_reduction_db, iterations, pre_encode?:{integrated_lufs,
  true_peak_dbtp, sample_peak_dbfs, loudness_range_lu, ceiling_dbtp, compensation_passes}},
  files:{subtitles:[{format, path}], stems:[{track_id, path}], manifest, preview}, tracks:[...], items:[...as
  plan, exact], ducking:[...], synthesis:{cues, cache_hits, synthesized, providers_called}, diagnostics,
  timings_ms:{...}}`. The four measurements of `loudness` describe the **delivered file**: for mp3, ogg, opus
  and m4a the encoded file is decoded and measured (§4.8), and `pre_encode` holds the master that went into the
  encoder; for wav and flac `pre_encode` is absent and the values are the master's. `gain_db`,
  `limiter_max_reduction_db` and `iterations` are the mastering loop's.

---------------------------------------------------------------------------------------------------------

## 9. Interfaces

### 9.1 CLI

```
soniccraft validate <TIMELINE> [--json] [--check-credentials]
soniccraft plan <TIMELINE> [--json]                      (alias: dry-run)
soniccraft render <TIMELINE> [-o PATH] [--format F] [--subtitles] [--stems] [--manifest] [--preview]
                  [--no-cache] [--cache-dir DIR] [--jobs N] [--json]
soniccraft schema [-o FILE]
soniccraft init [DIR] [--template NAME] [--list] [--no-assets] [--force]
soniccraft providers [--json]
soniccraft voices <PROVIDER> [--json]
soniccraft cache <info|clear> [--cache-dir DIR] [TIMELINE]
soniccraft mcp [--base-dir DIR] [--allow-command]
```

Exit codes: `0` success, `1` timeline invalid (errors in diagnostics), `2` usage error, `3` runtime/render
failure, `130` interrupted. With `--json`, stdout contains exactly one JSON document, usage errors included
(`` {"message", "diagnostics": [], "hint": "…; see `soniccraft <command> --help`"} ``; `soniccraft mcp`
excepted: its stdout is the protocol); progress and logs go to stderr. Timelines (files, FIFOs, `-` for stdin) are read
up to the 5 MiB document limit, never more than one byte past it: larger inputs, `/dev/zero` or endless stdin
fail at once with exit 1 and the size error in `diagnostics`. Source snippets in human-readable diagnostics
show every secret as `<redacted>`, like the JSON reports. A render stopped only by its environment or options
(missing key, unavailable provider, voice not installed, memory budget, a `--format`/`-o` format that needs
ffmpeg) exits 1 and ends with `error: render stopped: N errors`. `soniccraft mcp` lets timelines run `command`
providers only when started with `--allow-command` (or `SONICCRAFT_ALLOW_COMMAND=1`, which `soniccraft-mcp`
reads too). `voices <PROVIDER>` ignores case and `-`/`_` (timelines spell provider types exactly).

### 9.2 MCP tools

| Tool | Input | Result |
|---|---|---|
| `soniccraft_validate_timeline` | `{yaml_content?, yaml_path?, base_dir?, check_credentials?, detail?}` (exactly one of content/path) | ValidationReport |
| `soniccraft_dry_run` | `{yaml_content?, yaml_path?, base_dir?, detail?}` | PlanReport (+ diagram as text content) |
| `soniccraft_render` | `{yaml_content?, yaml_path?, output_path?, format?, base_dir?, export_subtitles?, no_cache?, detail?}` | RenderReport |
| `soniccraft_get_schema` | `{detail?}` | DSL quick reference (+ the JSON Schema with `detail: "full"`) |
| `soniccraft_list_providers` | `{}` | providers, availability (local servers get a TCP probe of at most 300 ms), credential status, default voices |
| `soniccraft_list_voices` | `{provider}` | voices (may call the provider API) |

`path` is accepted as a synonym of `yaml_path` by the three timeline tools (both appear in the input schemas), so
an agent never fails on the name.

An invalid timeline is a *successful* validation/dry-run call (`isError: false`, `valid: false`). Render
failures return `isError: true` with the diagnostics, so agents can repair and retry. Argument problems (an
unknown argument name, a wrong type, a value outside an enumeration, a `yaml_path` that is missing, not a
regular file or larger than 5 MiB) are also `isError: true` results that name the argument and the valid
choices (SEP-1303); JSON-RPC `-32602` is reserved for unknown tools and malformed `tools/call` params. `format`
(wav, mp3, flac, ogg, opus, m4a) overrides `output.format`, like the CLI's `--format`. `detail` sizes results
for an agent's context: `auto` (default) returns the full report while its JSON stays within about 12,000
characters and a compact report with the same keys above that (errors and warnings up to 50 with
`diagnostic_counts`; the first and last 10 items plus every item a diagnostic names or the overlap policy
moved, with `items_total` and `items_truncated`; capped ducking regions with `regions_total`; `full_report`
says where the rest is), keeping the whole result under about 40,000 characters; `summary` always returns the
compact report; `full` returns everything that fits one message and is shortened step by step, never failing,
beyond that.

Implementation: the official Rust SDK `rmcp` (pinned `=3.5.1`), which serves both protocol eras on stdio — the
`initialize` handshake (2024-11-05 … 2025-11-25; used by Codex/Cursor) and the stateless 2026-07-28 revision with
`server/discover` (probed by Claude Code). stdout carries only protocol messages; logs go to stderr. Results put the report in
`structuredContent` (a JSON object), mirror it as JSON text, add a short human/LLM summary text block, and for renders add
`resource_link` blocks (`file://` URIs, absolute paths) for the audio and side files (sessions on revisions older than
2025-06-18, which lack `resource_link`, get a `Files written:` text block instead) — never base64 audio. In the 2026-07-28
revision every result carries `_meta["io.modelcontextprotocol/serverInfo"]`. Relative paths
resolve against `base_dir`, else `soniccraft mcp --base-dir`, else env `CLAUDE_PROJECT_DIR`, else the server's working directory. Long renders emit
`notifications/progress` when the request carries `_meta.progressToken` and honour `notifications/cancelled`. A zero-argument
launcher binary `soniccraft-mcp` (equivalent to `soniccraft mcp`) exists for clients that cannot pass arguments.

Timelines are untrusted input: the server runs `command` providers (and piper with its own `command`) only when started
with `soniccraft mcp --allow-command` or `SONICCRAFT_ALLOW_COMMAND=1`; a document's own `output.path` must resolve inside the
call's base directory (an `output_path` argument may point anywhere); API keys go only to each provider's official host,
servers on this machine (the provider's own key variable only), or hosts in `SONICCRAFT_ALLOWED_HOSTS` (§2.8). At most two renders run at once
(`SONICCRAFT_MCP_MAX_RENDERS`); later ones wait, reporting progress ("Waiting for another render to finish") and honouring
cancellation.

### 9.3 Outputs and resources

Renders publish their outputs with a two-phase commit: every file (audio, subtitles, words, transcript, stems, manifest,
preview) is written to a hidden temporary name next to its destination (`.<name>.<random>.tmp.<ext>`, created exclusively)
and renamed into place after the last stage and the last cancellation check. A commit that fails half-way puts back the
files it replaced. A failed or cancelled render deletes only its temporary files and the empty directories it created, so
earlier outputs survive untouched. Existing files in the way that are not outputs of their kind are refused before any work
(§2.3).

A render holds the master in memory, the only full-length buffer: tracks are mixed in windows of 64 Ki frames straight into
it, stems stream to their files, and mastering works in place. Measured peaks (stereo, 48 kHz): about 23 MB per minute of
output for WAV, MP3 and the ffmpeg formats when music dominates, about 40 MB per minute when speech runs throughout, and
about 85 MB per minute for 24-bit FLAC, whose encoder keeps the whole stream. The engine estimates the peak before any TTS
request (with the dry-run lengths) and again after the exact layout; a render above the budget (`SONICCRAFT_MAX_RENDER_GB`,
default 8 GB of 2^30 bytes) fails with `timeline_too_long` before synthesizing anything, and validate/plan warn.

---------------------------------------------------------------------------------------------------------

## 10. Subtitle JSON and preview payload contracts

### 10.1 `<stem>.words.json` (subtitle format `json`)

```json
{
  "version": "1.0",
  "captions": [
    { "index": 1, "cue_id": "cue_01", "track_id": "voice_primary", "speaker": "Narrator",
      "start_ms": 0.0, "end_ms": 2480.0, "text": "Autonomous software development",
      "lines": ["Autonomous software development"] }
  ],
  "words": [
    { "text": "Autonomous", "start_ms": 0.0, "end_ms": 610.0, "cue_id": "cue_01",
      "track_id": "voice_primary", "speaker": "Narrator" }
  ]
}
```

`speaker` is the display label and may be `null`. Times are milliseconds from the start of the output,
rounded to 0.1 ms. `words` are in chronological order of their cues' start times, then word order.

### 10.2 Web preview (`<stem>.preview.html`)

The preview is `packages/soniccraft-web-preview/preview.html`: one self-contained file (inline CSS + JS, no
network, works from `file://`). The engine embeds a vendored copy (`crates/soniccraft-engine/assets/preview.html`,
kept byte-identical to the package file by a test, so the crate stays publishable) with `include_str!` and
replaces the single placeholder token `__SONICCRAFT_DATA__` inside
`<script id="soniccraft-data" type="application/json">…</script>` with the payload below, escaping **every**
`<` as `\u003c` (escaping only `</` is not enough: `<!--` followed later by `<script` keeps the data script
open) and U+2028/U+2029 as `\u2028`/`\u2029`. When the placeholder is
still present (template opened directly), the page offers a file picker for a `.preview.json`/manifest and
an audio file instead.

```json
{
  "soniccraft_preview": 1,
  "title": "Tech briefing",
  "audio": "briefing.mp3",
  "report": { "...": "RenderReport (§8) as the manifest carries it: paths relative to the HTML file, no timings_ms, no synthesis counters, cached durations exact, no cache_write_failed, so fresh and warm renders write identical pages" },
  "waveform": { "points_per_second": 50, "min": [-0.12, ...], "max": [0.11, ...] },
  "envelopes": [ { "track_id": "ambient_bed", "points_per_second": 50, "gain_db": [0.0, -0.4, ...] } ],
  "captions": [ "...same objects as words.json captions..." ],
  "words": [ "...same objects as words.json words..." ]
}
```

`waveform` is computed from the mastered output mixed to mono (min/max per bucket, rounded to 3 decimals);
bucket `i` covers `[i/pps, (i+1)/pps)`. `envelopes` are the ducking gain curves (dB, 0 = no attenuation) of
every ducked track; sample `i` is the gain at `t = i/pps`. Audio is played with
an `<audio>` element whose `src` is `audio` (relative), so the page works without fetch/CORS.

### 10.3 `<stem>.transcript.json` (subtitle format `transcript`, HyperFrames-compatible)

A JSON array of words in **seconds** (rounded to milliseconds), absolute to the start of the output file, in the normalized
shape HyperFrames' `transcribe` command consumes:

```json
[ {"id": "w0", "text": "Autonomous", "start": 0.0, "end": 0.61}, {"id": "w1", "text": "software", "start": 0.61, "end": 1.02} ]
```

`npx hyperframes transcribe <stem>.transcript.json --dir <video project>` then drives captions in a HyperFrames composition that
plays the mastered audio with `data-start="0"`.

---------------------------------------------------------------------------------------------------------

## 11. Licensing of optional components

The workspace is MIT OR Apache-2.0. Native MP3 encoding uses LAME 3.100 (LGPL-2.0-or-later), built from the sources bundled in
`mp3lame-sys` and statically linked through `mp3lame-encoder` (both crates LGPL-3.0). It is behind the default-on cargo
feature `mp3` (in `soniccraft-dsp`, re-exported by engine and CLI). Building with `--no-default-features` removes LAME; mp3
output then uses ffmpeg when available (`format_requires_ffmpeg` otherwise). Binary distributions that include the `mp3`
feature ship the LGPL-3.0, GPL-3.0 and LAME license texts (`licenses/`) and allow relinking a modified LAME
(`licenses/README.md`; satisfied by the public source). LAME's GPL decoder (the `decoder` feature of mp3lame-sys) is never
enabled, and `deny.toml` bans it. symphonia and option-ext are MPL-2.0 and used unmodified. Every release archive lists the
crates compiled into it, with their license texts, in `THIRD-PARTY-LICENSES.txt`.
