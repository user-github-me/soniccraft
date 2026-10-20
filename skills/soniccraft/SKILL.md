---
name: soniccraft
description: Turn scripts into produced, loudness-mastered audio with the SonicCraft CLI or MCP server by writing a declarative YAML timeline - TTS speech (ElevenLabs, OpenAI-compatible, Cartesia, Kokoro, Piper, macOS say, mock), music beds and sound effects, compiled with sample-accurate timing, automatic overlap resolution, sidechain ducking, EBU R128 loudness normalization, true-peak limiting and WebVTT/SRT subtitles. Use when the user wants a voiceover, narration, podcast or two-person dialogue, explainer or product video audio, audio ad, audiobook passage or any script turned into mixed audio; when they mention text-to-speech with background music, ducking, LUFS, true peak, loudness targets, captions or subtitles for generated speech, or a SonicCraft timeline; or when a HyperFrames or other video project needs a mastered voiceover with word timings. Prefer it over hand-written ffmpeg filter graphs for multi-track audio.
license: MIT OR Apache-2.0
compatibility: Requires the soniccraft binary on PATH (or its MCP server); see Setup for installing it. Cloud TTS providers need network access and API keys in environment variables; kokoro and piper need a local server or binary; macos_say needs macOS; the mock provider works offline everywhere.
metadata:
  author: user-github-me
  version: "0.1.0"
  dsl-version: "1.0"
---

# SonicCraft

SonicCraft is a compiler for audio. You describe intent in a timeline (who says what, in which order,
over which music, at which loudness); it synthesizes the speech, measures the real durations, lays every
cue out sample-accurately, ducks the music under the voices, masters to a loudness target and writes
subtitles that match the rendered audio. Never assemble speech and music with hand-written ffmpeg filter
graphs or guessed `adelay` offsets: write a timeline and let SonicCraft do the geometry.

## Setup

Check the binary first: `soniccraft --version`. If the command is not found, tell the user how to install
it, then continue:

- Homebrew (macOS, Linux): `brew install user-github-me/soniccraft/soniccraft`. Windows: the project's Scoop
  bucket (`scoop install soniccraft`, see the README).
- With Rust 1.89+ and a C toolchain: `cargo install --locked soniccraft` (installs `soniccraft` and
  `soniccraft-mcp` into `~/.cargo/bin`).
- A prebuilt archive from the project's GitHub releases (`soniccraft-v<version>-<target>.tar.gz`, `.zip` on
  Windows; check it against `SHA256SUMS`), with `soniccraft` and `soniccraft-mcp` put on PATH. The README's
  one-line installers do exactly this.
- For the MCP server only, without Rust: `npx -y soniccraft-mcp` (Node.js 20+) downloads the release binary
  on first use.

A SonicCraft MCP server (or the Claude Code plugin's server) listed as failed or missing usually means the
binary is not on PATH: install it as above and restart the client. The CLI works without the MCP server.

## Workflow

Run the CLI (it works in every agent); the same operations exist as MCP tools (below).

1. **Check the environment:** `soniccraft providers` shows which providers are usable here (API keys
   present, macOS, local servers answering) and their default voices. Pick providers the user can run.
2. **Start from a template:** `soniccraft init <dir> --template <name>` writes `timeline.yaml` plus the audio
   assets it uses (`soniccraft init --list`: offline-demo, offline-demo-mock, tech-briefing,
   two-person-podcast, voiceover-with-music, dialogue-crosstalk), or copy one of `reference/templates/*.yaml`
   (then provide the `assets/...` files its `source:` fields name).
3. **Write `timeline.yaml`** (rules below). Give every cue you will refer to an `id`.
4. **Validate:** `soniccraft validate timeline.yaml --json`. Fix every error using its `code`, `path`
   and `hint` (see the table below and [reference/diagnostics.md](reference/diagnostics.md)); repeat until
   `"valid": true`.
5. **Dry run (free):** `soniccraft plan timeline.yaml --json` lays out the timeline with estimated
   durations (mock voices are exact) and never calls a TTS API. Check total `duration_ms`, cue order
   (`pushed_from_ms` marks cues the overlap policy moved), ducking regions and
   `synthesis.characters_to_synthesize` (what the render will bill). Show the user the ASCII `diagram` when
   the layout matters.
6. **Render:** `soniccraft render timeline.yaml --json` (add `--subtitles`, `--stems`, `--manifest`,
   `--preview` as needed). `-o out.mp3` sets the output path but does not change the timeline's
   `output.format`: write `-o out.mp3 --format mp3` (the `-o` extension picks the format only when
   `output.format` is unset; otherwise the file holds WAV data and you get `format_extension_mismatch`).
   Synthesized cues are cached, so re-renders after edits only pay for changed cues. Outputs are put in
   place only when the render succeeds; files that are not SonicCraft outputs are never overwritten.
7. **Report:** output path, duration, `loudness.integrated_lufs` and `true_peak_dbtp` (measured on the
   delivered file; for mp3/ogg/opus/m4a `loudness.pre_encode` has the master's values) against their
   targets, any `true_peak_exceeded` / `loudness_target_missed` warnings, the side files, and any other
   warnings.

Exit codes: `0` ok, `1` timeline invalid, `2` usage error, `3` runtime/render failure. With `--json`,
stdout holds exactly one JSON report (`{message, diagnostics, hint}` when the command fails, usage errors
included); logs go to stderr.

### MCP alternative

If the SonicCraft MCP server is connected, use its tools instead of the CLI (same reports; details in
[reference/mcp-tools.md](reference/mcp-tools.md)):

| Tool | Arguments |
|---|---|
| `soniccraft_validate_timeline` | `yaml_content` or `yaml_path`, optional `base_dir`, `check_credentials`, `detail` |
| `soniccraft_dry_run` | `yaml_content` or `yaml_path`, optional `base_dir`, `detail` |
| `soniccraft_render` | `yaml_content` or `yaml_path`, optional `output_path`, `format`, `base_dir`, `export_subtitles`, `no_cache`, `detail` |
| `soniccraft_get_schema` | optional `detail` (`full` adds the JSON Schema) |
| `soniccraft_list_providers` | none |
| `soniccraft_list_voices` | `provider` |

An invalid timeline is a successful validate/dry-run call with `valid: false`; a failed render or a bad
argument returns `isError: true` with what to fix. Use absolute paths or `base_dir`; give `format` too when an
`output_path` has an extension other than the timeline's format. Long timelines return a compact report
(`items_truncated: true`); `detail: "full"` lists every item. Timelines with `command` providers need the
server started with `--allow-command`.

## Minimal timeline

```yaml
version: "1.0"
output:
  path: voiceover.mp3
  loudness_preset: podcast          # -16 LUFS, -1 dBTP
  export_subtitles: [vtt, srt]
speakers:
  narrator: { provider: elevenlabs, voice_id: JBFqnCBsd6RMkjVDRZzb, label: Narrator }
tracks:
  - id: bed
    type: music
    source: assets/bed.wav          # relative to this file
    loop: true                      # open-ended: runs to the end of the timeline
    volume: 0.8
    fade_in_ms: 1500
    fade_out_ms: 3000               # also the tail after the last word
    ducking: { trigger_tracks: [narration], reduction_db: -15 }
  - id: narration
    type: speech
    speaker: narrator
    start_at_ms: 1500
    sequence:
      - id: intro
        text: "Welcome to the show."
      - pause: 400ms
      - id: point_1
        text: "Here is the first point."
```

No API key? Use `provider: mock` (any OS) or `provider: macos_say` with `voice_id: Samantha` (macOS) to
prove the timeline, then switch providers. Give each speaker its own `voice_id` (for mock any string works,
e.g. `voice_id: alex` / `voice_id: sam`); otherwise both speakers sound the same (warning
`speakers_share_voice`).

## DSL essentials

Full field reference: [reference/dsl-reference.md](reference/dsl-reference.md). The rules that matter most:

- **Strict keys.** Unknown keys are errors with did-you-mean hints (`fade_in` → `fade_in_ms`, `voice` →
  `voice_id`, `file` → `source`). Use `x-` prefixed keys for your own notes. Booleans are `true`/`false`;
  `null` (a key without a value) means the key is absent.
- **Exact names.** Enum values are exact lowercase names (`type: speech`, `format: mp3`,
  `fade_curve: equal_power`); `Speech` or `MP3` is `invalid_value` (the hint has the spelling).
- **Quote text with ` #` or `: `.** `text: "Tip #1: always duck the bed."`; unquoted, YAML cuts the text at
  ` #` (warning `comment_in_text`).
- **Times** are milliseconds (`350`) or strings with lowercase units (`"350ms"`, `"1.5s"`, `"1m30s"`,
  `"00:01:30.500"`), at most 24 h. A bare `duration_ms: 30` is 30 ms (warning `suspicious_value`): write
  `"30s"` when you mean seconds.
- **Ids** (`^[A-Za-z0-9_][A-Za-z0-9_.:-]*$`) share one namespace across tracks, cues and clips.
  Unnamed cues get `<track_id>.cue<n>`; name anything you anchor to.
- **Tracks:** `type: speech` has a `sequence` of cues and pauses; `type: music` / `type: sfx` has either
  clip fields directly on the track (one clip) or a `clips` list. Clip fields on a speech track (or
  `sequence` on a music track) are `field_not_applicable`.
- **Sequential timing (default):** cues and pauses in a sequence play back to back from the track start
  (`start_at_ms`, default 0). A pause is its own item: `- pause: 300ms` (or `pause_duration`).
- **Explicit timing:** `start_at_ms: 2000`, or anchors:
  `start_relative: {after_cue: X, offset_ms: 150}` (start 150 ms after X ends), `{with_cue: X}` (start
  with X), `{before_cue: X}` (end exactly when X starts); `end_relative` on clips (`after_cue`/`with_cue`).
  X may be a cue, clip or track (a track anchor uses its first/last *declared* item); `offset_ms` may be
  negative. `start_at_ms` takes a time, never an id.
- **Overlaps resolve themselves.** With the default `timing.speech_overlap: push`, an implicit
  (sequential) cue that would collide with other speech moves to after it, keeping its preceding pause as
  the gap (or `min_gap_ms`, 250); you get info `cue_pushed`. Explicit cues never move, and a cue anchored to
  an implicit cue (an interjection with `with_cue`, a reply with a negative `after_cue` offset) does not push
  it: those overlaps stay `speech_overlap` warnings. Mark intentional crosstalk with `allow_overlap: true`.
- **Dialogue:** simplest is ONE speech track with a `speaker` per cue (turns can never collide). Use one
  track per person when you need panning or interruptions (anchor each turn).
- **Speakers registry:** `speakers: {host: {provider, voice_id, model, settings, label, gain_db}}`, then
  `speaker: host` on a track or cue. Voice fields resolve: provider defaults → provider instance →
  `defaults` → track speaker → track fields → cue speaker → cue fields. `label` is the subtitle name.
  Voice keys go next to `settings`, never inside it (`settings.voice` is ignored: write `voice_id`).
- **Providers:** built-in names work directly (`elevenlabs`, `openai`, `cartesia`, `kokoro`, `piper`,
  `macos_say`, `command`, `mock`). Configure instances under `providers:` (`model`, `settings`,
  `max_concurrency`, `base_url`, `api_key_env`); a custom instance name needs `type`. elevenlabs, cartesia,
  piper and command need a `voice_id`.
- **Secrets never go in the timeline** (`secret_in_timeline`). Keys live in env vars:
  `ELEVENLABS_API_KEY`, `OPENAI_API_KEY`, `CARTESIA_API_KEY`. They are sent only to the provider's own API
  host, to servers on this machine (the provider's own variable only), or to hosts in
  `SONICCRAFT_ALLOWED_HOSTS` (set in the environment of soniccraft or the MCP server, never in the timeline).
- **Music beds:** `loop: true` without a length runs to the end; the timeline ends `fade_out_ms` after the
  last finite item, so the bed fades out after the last word. Bound a bed with
  `end_relative: {after_cue: last_line, offset_ms: 3000}` or `duration_ms`.
- **Ducking defaults:** `reduction_db -15`, `attack_ms 80`, `release_ms 400`, `hold_ms 250`,
  `lookahead_ms` = attack, `threshold_db -40`, `mode: sidechain` (signal-driven; `cue` = timeline-driven).
- **Loudness presets** (`output.loudness_preset`): `podcast` -16 LUFS / -1 dBTP (default), `streaming`
  -14 / -1, `broadcast` -23 / -1 (EBU R128), `atsc` -24 / -2. Override with `target_lufs` (-60 to -5) /
  `true_peak_db`. Speech and music sources are normalized to -20 LUFS (`mix.reference_lufs`) before mixing,
  so voices from different providers match; sfx keep their own level.
- **Output:** `format` wav | mp3 | flac (native) | ogg | opus | m4a (need ffmpeg); default from the
  `path` extension, else wav; 48 kHz stereo; mp3 192 kbps CBR.
- **Subtitles:** `output.export_subtitles: true` (= vtt + srt) or a list of `vtt`, `srt`, `json` (captions +
  word times), `transcript` (HyperFrames). `caption:` overrides the displayed text of a cue
  (`text: "minus sixteen LUFS"`, `caption: "-16 LUFS"`); `subtitles: false` hides a cue. Layout knobs
  under `subtitles:` (42 chars per line, 2 lines, 800 to 6000 ms per caption); speaker labels are on
  automatically with 2+ speakers.

## Estimating durations

Plan scripts before writing them: natural narration is about **150 words per minute ≈ 2.5 words/s ≈ 15
characters/s**. A 30 s spot is about 75 words; a 60 s voiceover about 150 words (900 characters).
SonicCraft's dry-run estimates use characters per second, counting spaces and punctuation:
elevenlabs 15.5 (`eleven_v3`/`eleven_v4`: 14), openai 15, cartesia 15, kokoro 16, piper 17, macos_say 17,
command 15; mock is exact. `settings.speed` divides the duration (piper: `length_scale` multiplies it).
Estimates are within about ±15 %; renders measure exactly and re-run layout, ducking and the overlap
policy. If the user needs an exact length, set `output.duration_ms` (content beyond it is cut) or adjust
the script after a render.

## Choosing voices and providers

- Run `soniccraft providers`; use only providers that report available. Never ask the user to paste an
  API key into the timeline or the chat; ask them to export it.
- Quality and expressiveness: `elevenlabs` (`eleven_multilingual_v2` default; `eleven_v4` is expressive
  but accepts only `stability` and `similarity_boost`), `cartesia` (`sonic-3.6`).
- Free and local: `kokoro` (Kokoro-FastAPI in Docker, word timings), `piper` (pip; download a voice with
  `python3 -m piper.download_voices <name> --data-dir voices` and set `voices_dir: voices`), `macos_say`
  (Mac). `openai` is an OpenAI-compatible client; OpenAI's own TTS models retire on 2027-01-06, so point
  `base_url` at a compatible server for long-lived timelines.
- Word-accurate subtitles need word timings: elevenlabs, cartesia, kokoro, mock. Others are estimated
  from the audio (info `alignment_estimated`).
- Find voice ids with `soniccraft voices <provider>`. Provider setup, voices, settings and limits:
  [reference/providers.md](reference/providers.md).
- Long text: split into several cues at sentence or paragraph boundaries (per-request limits, e.g.
  OpenAI 4,096 characters, `eleven_v3` 5,000; violations are `text_too_long`). Shorter cues also cache and
  retime better.

## Mixing best practices

Details and recipes: [reference/mixing-guide.md](reference/mixing-guide.md).

- Always duck beds under speech instead of setting them uniformly quiet. Podcast beds -18 to -20 dB,
  explainers -12 to -15 dB, energetic ads -8 to -10 dB.
- Set `hold_ms` a bit longer than the pauses between lines (pause + ~60 ms) so the bed stays down instead
  of pumping between sentences.
- Give beds `fade_in_ms` 800 to 2000 and `fade_out_ms` 2000 to 4000. Start speech 1 to 3 s into the bed for
  an intro (`start_at_ms` on the speech track).
- Anchor sound effects to speech (`before_cue`, `with_cue` with a negative offset, `after_cue`), never to
  guessed absolute times.
- Keep the main voice centered; separate dialogue gently (`pan` ±0.15 to ±0.3).
- Pick the loudness preset by destination: podcast -16, YouTube/streaming -14, EBU broadcast -23,
  US broadcast -24. If you see `loudness_target_unreachable` or `loudness_target_missed`, set `target_lufs`
  to the level the message reports, or lower the music.

## Subtitles and HyperFrames

For a HyperFrames video, export `transcript` (a word array in seconds, absolute to the start of the
audio) and feed it to HyperFrames:

```bash
soniccraft render voiceover.yaml --json      # with output.export_subtitles: [vtt, transcript]
npx hyperframes transcribe voiceover.transcript.json --dir my-video
```

Then play the mastered audio in the composition from time zero, so the transcript times line up:
`<audio id="voiceover" src="assets/voiceover.wav" data-start="0" data-duration="21.04"></audio>` (the `id`
is required; `data-duration` = the report's `output.duration_ms` / 1000). `json` subtitles
(`<stem>.words.json`) carry captions plus per-word timestamps for other players.

## Fixing diagnostics

| Code | Typical cause | Mechanical fix |
|---|---|---|
| `unknown_field` | typo or synonym (`fade_in`, `voice`, `triggers`, `start`, `file`) | apply the hint (`fade_in_ms`, `voice_id`, `trigger_tracks`, `start_at_ms`, `source`); `label` goes in a `speakers` entry |
| `field_not_applicable` | clip fields on a speech track, `sequence` on music, `settings.voice` | move the field to the right place (`voice_id` next to `settings`) |
| `conflicting_fields` | `start_at_ms` + `start_relative`; pause and text in one item; `source` + `clips` | keep one; make the pause its own `- pause: 300ms` item |
| `invalid_value` | bad or miscased enum (`MP3`), bad time string, out-of-range number, `target_lufs: 16` | use the hinted value; times like `"1.5s"` |
| `suspicious_value` | seconds written as milliseconds (`duration_ms: 30`) | write the unit (`"30s"`) or keep it if you meant ms |
| `comment_in_text` | unquoted text with ` #` | quote the text (the hint has the line) |
| `unknown_reference` | anchor or trigger id that does not exist | use a hinted id; give the target cue an explicit `id` |
| `dependency_cycle` | anchors that loop | make one item sequential or absolute |
| `anchor_open_ended` | anchoring to the end of a looping bed with no length | anchor to speech, use `with_cue`, or give the bed a length |
| `track_anchor_out_of_order` | track anchor while an item with its own start ends later | anchor to that cue (`after_cue: outro`) or reorder the items |
| `missing_voice` | elevenlabs/cartesia/piper/command without `voice_id` | set `voice_id` (find with `soniccraft voices <provider>`) |
| `unknown_voice` | macOS voice not installed, piper model not downloaded | use the hinted voice, or run the download command in the hint |
| `unknown_speaker` | misspelled speaker (error) or unregistered one (warning) | use the hinted name or add it under `speakers:` |
| `speakers_share_voice` | two speakers without distinct `voice_id`s | give each speaker its own `voice_id` |
| `unknown_provider` | misspelled provider or instance | use the hinted name or add it under `providers:` |
| `missing_credentials` | env var not set (or an unexpanded `${VAR}`) | ask the user to export the key, or switch to a local provider |
| `provider_unavailable` | macos_say off macOS, missing piper, local server down, key host not allowed, MCP without `--allow-command` | follow the hint, or switch provider (`mock` always works) |
| `asset_not_found` | wrong `source` path (relative to the timeline) | fix the path or copy the file |
| `secret_in_timeline` | API key pasted into YAML | delete it; use env vars / `api_key_env` |
| `text_too_long` | cue over the provider's limit | split into several cues |
| `speech_overlap` | explicit cues collide | apply the hint: `start_relative: {after_cue: ..., offset_ms: 350}` or `allow_overlap: true` |
| `cue_pushed` (info) | push policy moved a cue | fine if the order is intended; otherwise anchor it |
| `timeline_too_long` | longer than 24 h, or over the render memory budget | look for seconds written as ms; split the timeline |
| `loudness_target_unreachable`, `loudness_target_missed` | target too hot or uneven material | set `target_lufs` to the reported level, or lower the music |
| `true_peak_exceeded` | lossy codec overshoots the ceiling | `true_peak_db: -2`, a higher bitrate, or wav/flac |
| `format_extension_mismatch` | `-o out.mp3` on a `format: wav` timeline | add `--format mp3` (MCP: `format`) |
| `synthesis_failed` | provider rejected the request | read the provider message; check key, voice id, quota, server |

## Reference files

- [reference/dsl-reference.md](reference/dsl-reference.md): every field, type, default and the layout
  rules. Read when writing anything beyond the minimal template.
- [reference/providers.md](reference/providers.md): setup, env vars, models, voices, settings, limits and
  word timings per provider. Read before choosing or configuring a provider.
- [reference/mixing-guide.md](reference/mixing-guide.md): ducking, loudness, panning, fades, beds, sfx.
  Read when the user cares about the mix or gives a loudness spec.
- [reference/diagnostics.md](reference/diagnostics.md): every diagnostic code with its fix. Read when a
  code is not in the table above.
- [reference/mcp-tools.md](reference/mcp-tools.md): MCP tool arguments, report shapes, client timeouts and
  approval.
- [reference/timeline-schema.json](reference/timeline-schema.json): the JSON Schema (also
  `soniccraft schema`).
- Templates in `reference/templates/`: `tech-briefing.yaml` (two voices, pushed cue, ducked bed),
  `two-person-podcast.yaml` (one conversation track, intro/outro beds), `voiceover-with-music.yaml`
  (narrator, bounded bed, cold-open hit), `offline-demo.yaml` (macOS say + mock, no keys),
  `dialogue-crosstalk.yaml` (panned voices, interruptions, `allow_overlap`).
