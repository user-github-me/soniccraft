# SonicCraft timeline DSL v1.0: quick reference

A timeline is one YAML or JSON document describing tracks of speech, music and sfx. SonicCraft
synthesizes the speech, lays every item out sample-accurately, ducks music under speech, masters to a
loudness target and writes subtitles. Workflow: write → `soniccraft_validate_timeline` (fix every
diagnostic by its `code`, `path` and `hint`) → `soniccraft_dry_run` (free) → `soniccraft_render`.

## Document rules
- `version: "1.0"` and a non-empty `tracks` list are required.
- Keys are snake_case and strict: an unknown key is `unknown_field` with a did-you-mean hint. Keys
  starting with `x-` or `x_` are your annotations and are ignored. Enum values are exact lowercase names
  (`speech`, `mp3`, `equal_power`): `MP3` or `equal-power` is `invalid_value` with the fix.
- Times (Millis): a number of **milliseconds** (`350`) or a string: `"350ms"`, `"1.5s"`, `"2m"`,
  `"1m30s"`, `"01:30.5"`, `"00:01:30.500"`. At most 24 h; only `offset_ms` may be negative. A bare
  number that looks like seconds (`duration_ms: 30`) warns `suspicious_value`.
- Ids match `^[A-Za-z0-9_][A-Za-z0-9_.:-]*$` and share ONE namespace across tracks, cues and clips.
  Unnamed cues get `<track_id>.cue<n>`, clips `<track_id>.clip<n>`; name anything you anchor to.
- Linear `volume` ≥ 0 (1.0 = unity, > 2 warns, > 4 errors); `gain_db` in [-60, 24]; `pan` in [-1, 1].
- Relative paths (`source`, `output.path`) resolve against the timeline file's directory (inline
  `yaml_content`: against `base_dir`).
- Never put API keys in a timeline (`secret_in_timeline`); providers read env vars.

## Top level
`version`, `title`, `description`, `output`, `subtitles`, `timing`, `mix`, `defaults` (voice for every
cue), `speakers` (name → voice), `providers` (name → instance), `tracks` (mixed in declaration order).

## output
| field | default | notes |
|---|---|---|
| `format` | path extension, else `wav` | wav, mp3, flac native; ogg, opus, m4a need ffmpeg; wins over the extension; MCP `format` overrides |
| `path` | `<timeline stem>.<ext>` | relative to the timeline; over MCP it must stay inside `base_dir` unless `output_path` overrides it |
| `sample_rate` | 48000 | 8000–192000 (mp3: 8000…48000 standard rates only) |
| `channels` | 2 | 1 or 2 |
| `bitrate` | mp3 192k, ogg 160k, opus 96k, m4a 160k | `"320k"`, `320`; ignored for wav/flac |
| `bit_depth` | 24 | wav 16/24/32(float), flac 16/24 |
| `loudness_preset` | `podcast` | podcast -16 LUFS/-1 dBTP, streaming -14/-1, broadcast -23/-1, atsc -24/-2 |
| `target_lufs` / `true_peak_db` | preset | [-60, -5] / [-9, 0]; loudness values are negative |
| `normalize` | true | false = limiter only |
| `export_subtitles` | false | true = [vtt, srt]; or one name or a list of vtt, srt, json, transcript |
| `export_stems` / `export_manifest` | false | `<stem>.stems/<track>.wav`, `<stem>.manifest.json` |
| `tail_ms` | auto | time after the last finite item; auto = longest fade_out of open-ended beds |
| `duration_ms` | none | forces the total length; content beyond is cut |
| `dither` | true | TPDF dither for 16-bit output |

## Tracks (all types)
`id` (required), `type` (`speech` \| `music` \| `sfx`, required), `volume` (1.0), `gain_db` (0),
`pan` (0), `mute` (false: laid out but not mixed), `normalize` (speech/music true, sfx false; or
`{target_lufs: -20}`), `start_at_ms` or `start_relative` (track start, default 0), `ducking`.

## Speech tracks
Fields: `sequence` (required), voice fields (`speaker`, `provider`, `voice_id`, `model`, `language`,
`settings`), `trim_silence` (true, or `{threshold_db: -50, padding_ms: 30}`). Clip fields are
`field_not_applicable` here. Each `sequence` item is a pause or a cue, never both:
- Pause: `- pause: 300ms` (or `pause_duration`). It advances the track cursor.
- Cue: `text` (required), `id`, `caption` (subtitle text override), `subtitles: false` (hide),
  voice fields, `start_at_ms` / `start_relative` (explicit start; otherwise the cue is implicit and
  starts at the cursor), `volume`, `gain_db`, `allow_overlap: true` (intentional crosstalk).

## Music and sfx tracks
Either clip fields directly on the track (one clip whose id is the track id) or a `clips` list, not
both. Clip fields:
| field | default | notes |
|---|---|---|
| `source` | required | wav, mp3, flac, ogg, m4a/aac, aiff (`asset_not_found` if missing) |
| `start_at_ms` / `start_relative` | sequential | later clips follow the previous clip |
| `trim_start_ms` | 0 | offset into the source |
| `duration_ms` / `end_at_ms` / `end_relative` | natural length | at most one |
| `loop` | false | repeat the source to fill the length |
| `loop_crossfade_ms` | 0 | equal-power crossfade at loop seams |
| `fade_in_ms` / `fade_out_ms` | 0 | scaled down if they exceed the clip (info `fades_scaled`) |
| `fade_curve` | `equal_power` | `linear`, `equal_power`, `exponential` |
| `crossfade_ms` | 0 | clips list only: overlap with the previous clip |
| `volume`, `gain_db` | 1.0, 0 | on top of the track fader |
Clip length: natural = source − trim. A looping clip with no length is **open-ended**: it runs to the
timeline end and its end cannot be anchored to (`anchor_open_ended`).

## Anchors (start_relative, end_relative)
```yaml
start_relative: { after_cue: cue_01, offset_ms: 150 }   # start 150 ms after cue_01 ends
start_relative: { with_cue: cue_01, offset_ms: -200 }   # start 200 ms before cue_01 starts
start_relative: { before_cue: cue_02 }                  # END exactly when cue_02 starts
end_relative:   { after_cue: cue_03, offset_ms: 2000 }  # clip ends 2 s after cue_03 ends
end_relative:   { with_cue: cue_03 }                    # clip ends when cue_03 starts
```
Exactly one of after_cue / with_cue / before_cue (`before_cue` is not allowed in end_relative). The id may
be a cue, a clip or a track (span: first item start → last item end). Cycles are `dependency_cycle`;
a negative resolved start is clamped to 0 (`negative_start`).

## Timing and overlaps
- Items in a sequence play back to back from the track start; pauses add gaps.
- `timing.speech_overlap`: `push` (default) moves an implicit cue that would collide with other speech to
  after it, keeping its preceding pause as the gap (or `timing.min_gap_ms`, 250); info `cue_pushed`.
  Explicit cues never move; leftover overlaps are warnings `speech_overlap` with a fix hint. `warn` reports
  only, `error` makes overlaps errors, `allow` ignores them. `overlap_tolerance_ms`: 20.
- Total length = `output.duration_ms`, else content end + tail. Beds (open-ended clips) end at the total
  and fade out with their `fade_out_ms`; that fade also becomes the tail after the last word.

## Ducking (on the track to be lowered)
```yaml
ducking: { trigger_tracks: [voice], reduction_db: -15, attack_ms: 80, release_ms: 400,
           hold_ms: 250, lookahead_ms: 80, threshold_db: -40, mode: sidechain }
```
Defaults as shown (`lookahead_ms` = attack). `mode: sidechain` follows the trigger tracks' audio; `cue`
follows their item intervals (dry runs always use cue mode). Set `hold_ms` a little longer than the
pauses between lines so the bed does not pump. Typical depth: podcast -18 to -20, explainer -12 to -15.

## Voices, speakers and providers
- Voice fields: `provider`, `voice_id`, `model`, `language`, `settings` (merged key by key), `gain_db`
  (speakers/defaults), `label` (speakers: subtitle name).
- Resolution (low → high): provider defaults → provider instance → `defaults` → track's speaker → track
  fields → cue's speaker → cue fields.
- `speakers: {host: {provider: elevenlabs, voice_id: JBFqnCBsd6RMkjVDRZzb, label: Host}}`, then
  `speaker: host` on a track or cue. A misspelling of a defined speaker is an `unknown_speaker` error;
  another name missing from a non-empty registry warns (it is only a subtitle label). Two speakers with
  the same voice warn `speakers_share_voice`: give each its own `voice_id` (any string for `mock`).
- Built-in providers (usable by name without a `providers:` entry): elevenlabs, openai, cartesia, kokoro,
  piper, macos_say, command, mock. elevenlabs, cartesia, piper and command need `voice_id`
  (`missing_voice`); openai defaults to `alloy`, kokoro `af_heart`, macos_say `Samantha`, mock `mock`.
- Instances: `providers: {narration: {type: elevenlabs, model: eleven_multilingual_v2, settings: {...},
  max_concurrency: 2}}` (`type` is required when the name is not a built-in). Other fields: `api_key_env`,
  `base_url`, `output_format`, `timeout_ms`, `chars_per_second`, `words_per_minute`, `command`, `stdin`,
  `voices_dir`.
- Keys come from env vars: ELEVENLABS_API_KEY, OPENAI_API_KEY, CARTESIA_API_KEY, and go only to the
  provider's own API host, servers on this machine (own variable only) or hosts in SONICCRAFT_ALLOWED_HOSTS. `mock` (any OS) and
  `macos_say` (macOS) need none: use them to prove a timeline before paying for synthesis.
- `command` providers (and a piper `command`) run local programs: the MCP server allows them only when
  started with `--allow-command` (else `provider_unavailable`).

## Subtitles
`output.export_subtitles: true` writes `<stem>.vtt` + `<stem>.srt`; `json` adds `<stem>.words.json`
(captions + word times), `transcript` adds `<stem>.transcript.json` (HyperFrames word array). Layout
under `subtitles:`: `max_chars_per_line` 42, `max_lines` 2, `max_duration_ms` 6000, `min_duration_ms`
800, `speaker_labels` (auto: on with 2+ speakers). Captions never merge cues.

## Estimating length
About 15 characters (2.5 words) per second: a 30 s spot ≈ 75 words, 60 s ≈ 150 words. Dry runs use
per-provider characters/second (elevenlabs 15.5, openai 15, cartesia 15, kokoro 16, piper 17,
macos_say 17; mock is exact) and are within about ±15 %; renders measure exactly.

## Example
```yaml
version: "1.0"
title: "Two-voice briefing"
output: { path: briefing.mp3, loudness_preset: podcast, export_subtitles: [vtt, srt] }
speakers:
  narrator: { provider: mock, voice_id: narrator, label: Narrator }
  guest: { provider: mock, voice_id: guest, label: Guest }
tracks:
  - id: bed
    type: music
    source: assets/bed.wav        # relative to the timeline file
    loop: true                    # open-ended: runs to the end
    volume: 0.7
    fade_in_ms: 1200
    fade_out_ms: 2500             # also the tail after the last word
    ducking: { trigger_tracks: [voice_a, voice_b], reduction_db: -18, hold_ms: 400 }
  - id: voice_a
    type: speech
    speaker: narrator
    start_at_ms: 1500
    sequence:
      - id: intro
        text: "Welcome to the briefing."
      - pause: 350ms
      - id: wrap_up               # implicit: pushed after the guest's line if they collide
        text: "That's all for today."
  - id: voice_b
    type: speech
    speaker: guest
    pan: 0.15
    sequence:
      - id: reply
        start_relative: { after_cue: intro, offset_ms: 150 }
        text: "Thanks for having me."
  - id: stingers
    type: sfx
    clips:
      - id: whoosh
        source: assets/whoosh.wav
        start_relative: { before_cue: intro }   # ends exactly when the intro starts
```

## Fixing common diagnostics
| code | fix |
|---|---|
| `unknown_field` | rename to the hinted field (`fade_in` → `fade_in_ms`, `voice` → `voice_id`) |
| `field_not_applicable` | move the field to the right track type (clip fields belong on music/sfx) |
| `conflicting_fields` | keep one of the fields; make a pause its own `- pause: 300ms` item |
| `invalid_value` / `invalid_type` | use the hinted value or form (times like `"1.5s"`) |
| `suspicious_value` | a bare number is milliseconds: write `"30s"` if you meant seconds |
| `unknown_reference` | use a hinted id; give the target cue an explicit `id` |
| `dependency_cycle` / `anchor_open_ended` | make one item sequential or absolute; give the bed a length |
| `missing_voice` / `unknown_provider` | set `voice_id`, or use the hinted provider name |
| `missing_credentials` / `provider_unavailable` | export the key, or switch to `mock` / a local provider |
| `asset_not_found` | fix the `source` path (relative to the timeline file) |
| `speech_overlap` | apply the hint: `start_relative: {after_cue: X, offset_ms: 350}` or `allow_overlap: true` |
| `text_too_long` | split the cue into several cues |
| `timeline_too_long` | keep everything within 24 h; look for times written in seconds as ms |
| `loudness_target_unreachable` | lower `target_lufs` or the music level |
| `synthesis_failed` | read the provider message: key, voice id, quota |
