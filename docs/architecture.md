# SonicCraft architecture

A contributor's tour of the pipeline: what each crate owns, how data flows from a timeline file to a
mastered audio file, and the invariants (determinism, diagnostics, caching) every change must keep.
[DESIGN.md](DESIGN.md) is the normative specification; this document explains how the pieces fit and
points to the relevant DESIGN sections. When the two disagree, DESIGN.md wins and this file is a bug.

## Principles (DESIGN §0)

1. **Declarative in, deterministic out.** The same timeline, TTS cache and binary produce byte-identical
   audio, subtitles and manifests. No wall-clock time, unseeded randomness, host names or absolute paths in
   rendered artifacts; iteration order is stable (`IndexMap`, sorted vectors).
2. **Agent-first diagnostics.** Never stop at the first problem. Every diagnostic has a stable `code`, a
   precise `path` (plus line/column for YAML) and, wherever possible, a mechanical `hint`.
3. **Native, dependency-light DSP.** Decode, resample, mix, duck, measure and limit in Rust on `f32`
   buffers. ffmpeg is never required for wav/mp3/flac; it is an optional encoder for ogg/opus/m4a and an
   optional decode fallback.
4. **Sample-accurate geometry.** Layout is integer sample frames at the output rate; milliseconds exist only
   at the edges (the DSL and the reports).
5. **Cheap iteration.** TTS results are cached by content hash; dry runs never call paid APIs.

## Workspace map

```
crates/
  soniccraft-core       DSL: raw AST, parsing + spans, diagnostics, lowering to IR, JSON Schema, provider metadata
  soniccraft-layout     geometry: scheduling, overlap policy, clip lengths, plan diagram
  soniccraft-captions   word alignment, caption segmentation, VTT/SRT/JSON/transcript writers
  soniccraft-dsp        AudioBuffer, decode, resample, analysis, clips, pan, ducking, loudness, limiter, encoders
  soniccraft-tts        providers, cache, duration estimates, retries/concurrency, credentials, voices
  soniccraft-engine     validate / plan / render orchestration, environment checks, report types
  soniccraft-mcp        MCP stdio server over the engine
  soniccraft-cli        `soniccraft` binary (+ `soniccraft-mcp` launcher)
packages/soniccraft-web-preview   self-contained preview.html (the engine embeds a vendored copy)
packages/soniccraft-mcp           npm launcher: finds or downloads the release binary, runs `soniccraft mcp`
skills/soniccraft                 Agent Skill (SKILL.md, reference/, templates)
examples/                         runnable timelines + generated assets
licenses/                         LGPL/GPL/LAME texts shipped with mp3 builds, relinking notes
scripts/                          asset generator, template sync, license notices, CI checks
```

Dependency graph (no cycles):

```
core <-- layout
core <-- tts --> dsp
captions (standalone)
engine --> core, layout, captions, dsp, tts
mcp    --> engine
cli    --> engine, mcp
```

`soniccraft-layout` and `soniccraft-captions` are pure (no I/O), like `soniccraft-core`.

The `mp3` cargo feature (native LAME encoding: LAME is LGPL-2.0-or-later, the mp3lame crates LGPL-3.0; DESIGN
§11) is defined in `soniccraft-dsp` and re-exported by `soniccraft-engine` and `soniccraft-cli`, where it is
on by default. Internal crates depend on `soniccraft-dsp` and `soniccraft-engine` with default features off,
so the top-level binary decides.

## Data flow

```
text --parse--> serde_json::Value --decode per element--> model::Timeline (raw AST) + SpanIndex
     --lower--> ir::Program (resolved, validated) + Compiled.assets          [core]
     --engine env checks--> ValidationReport                              [engine]

plan:   Program + duration table (cache | exact mock | estimate | source | fixed)
        --layout--> placements + overlap diagnostics --interval ducking--> PlanReport + diagram

render: Program --decode clip sources, dry-run layout + memory budget (preflight)--> go / stop  [engine]
        --synthesize all cues (cache first)--> provider audio + word timings      [tts]
        --per cue: mono, resample, trim, normalize, gain-->                               [dsp]
        clips: decode once per source, resample, normalize --> exact duration table
        --layout (exact)--> placements                                                    [layout]
        --windowed mix: place, pan, track gain, ducking--> master --R128 + true peak--> encode   [dsp]
        --measure the delivered file (lossy), re-limit if needed                          [engine]
        --captions from placed word timings--> vtt/srt/json/transcript                    [captions]
        --stems, manifest, preview--> two-phase commit --> RenderReport                   [engine]
```

### 1. Parse (core, DESIGN §2)

`parse::parse_timeline(text, format)` turns YAML (serde-saphyr, strict booleans, duplicate keys rejected)
or JSON into a generic value, with the same budgets for both formats (5 MiB, `MAX_NODES` = 250000 values,
64 levels of nesting); JSON comments and trailing commas get their own diagnostics. It then deserializes the
value **element by element** into the raw AST (`model::Timeline`): each top-level field, track, sequence item
and clip separately, so one bad element is reported (`invalid_type`) and dropped without hiding problems
elsewhere. Unknown keys are collected everywhere (`unknown_field` with did-you-mean against the fields valid
at that location and common synonyms such as `file` → `source`; `x-` and `x_` keys are ignored). The raw
AST is deliberately permissive: every field is optional and enum-like values are strings, so typos become
diagnostics instead of parse failures. A mechanical-fix pass then reports values with one obvious
correction (an enum with the wrong case, `"350MS"`, `target_lufs: 16`, `loudness_preset: youtube`) as
`invalid_value` and keeps the corrected value for the remaining checks, so one mistake does not cascade.
Secrets (`secret_in_timeline`) and unquoted ` #` in cue text (`comment_in_text`) are found here too;
`parse::secret_spans` gives the byte ranges the CLI masks in snippets. `SpanIndex` maps diagnostic paths
(`tracks[2].sequence[0].text`) to line/column; `compile()` attaches them to every diagnostic and sorts the
list deterministically.

### 2. Lower (core, DESIGN §2, §3.3 static parts)

`lower::lower(timeline, options)` applies every semantic rule and produces `ir::Program` only when there are
no errors:

- resolves defaults (output presets, subtitle layout, timing policy, mix reference, per-track normalize
  and trim), parses every `Millis`/`Bitrate`/enum (one duration grammar, shared with the JSON Schema
  pattern; every time at most 24 h, `soniccraft_core::MAX_TIME_MS`), and applies CLI/MCP overrides
  (`LowerOptions`);
- validates ids (shared namespace, auto ids `<track>.cue<n>` / `<track>.clip<n>`, never trimmed), field
  applicability per track and provider type, conflicting fields and ranges, and warns about bare numbers that
  look like seconds (`suspicious_value`);
- resolves provider instances (configured, built-in names and aliases) and merges each cue's voice in the
  documented order (DESIGN §2.7), filling provider defaults from `providers::meta`; misspelled speakers are
  errors resolved with the did-you-mean speaker, voice keys inside `settings` are warnings, and speakers that
  end up with an identical voice get `speakers_share_voice`;
- resolves anchors to `AnchorRule`s and builds a static dependency graph to report `self_reference`,
  `dependency_cycle` and `anchor_open_ended` before any audio exists. Track anchors use declaration order
  (first declared item for `with_cue`/`before_cue`, last for `after_cue`), the same model the layout uses.

`Compiled.assets` lists every clip `source` (path, id, line/column) even when the program is `None`, so the
engine checks assets in documents that have other errors.

The IR (`ir.rs`) is what every downstream crate consumes: `Program { output, subtitles, timing, mix,
tracks, nodes, providers, ids }`, where `nodes` holds every cue, pause and clip with its `StartRule`
(`Sequential` | `Absolute` | `Relative`) and, for clips, a `ClipLength` rule. Every time in it is finite and
within ±24 h, so frame conversions cannot overflow.

### 3. Validate (engine, DESIGN §7 environment codes)

`Engine::validate` runs `core::compile` and adds environment diagnostics: assets exist and decode
(`asset_not_found`, `asset_unreadable`; also for documents with errors, from `Compiled.assets`), credentials
are present and not unexpanded placeholders (`missing_credentials`: an error only for uncached cues), keys
would only go to allowed hosts (the tts key-host policy and `SONICCRAFT_ALLOWED_HOSTS`), providers can run
here (`provider_unavailable`: a missing binary, macOS only, a local server that does not answer a 300 ms
probe, a `command` provider the engine does not allow), voices are installed (`unknown_voice` for macOS
voices and piper models), command templates do not pass `{text}`/`{voice_id}` as an argument of their own
(`suspicious_value`), output paths stay inside `confine_outputs_to` when set (the MCP server), ffmpeg exists
when a format needs it (`format_requires_ffmpeg`), cues fit the provider's request limit (`text_too_long`),
and the render fits the memory budget (`timeline_too_long` warning). Layout errors that depend on estimated
durations are reported as warnings. It never fails: every problem is a diagnostic in the `ValidationReport`.

### 4. Durations and layout (tts + layout, DESIGN §3, §5.3)

Layout is a pure function of the `Program` and a **duration table**. A dry run (`Engine::plan`) fills the
table without any API call: cached cues their exact cached length (`cache`), mock cues an exact in-process
rendering (`exact`), others the provider's characters-per-second estimate (`estimate`), clips their probed
source length (`source`), pauses and explicit lengths their value (`fixed`). A render fills it with exact
lengths measured after silence trimming.

The scheduler (DESIGN §3.4) visits node starts and ends as separate events in dependency order (explicit
starts first, then the smallest tentative start, then declaration order), places sequential items at the
track cursor, resolves anchors in frames, and applies the speech-overlap policy: under `push`, an implicit cue
that collides with scheduled speech moves to `end(other) + gap` and emits `cue_pushed` (one per moved cue);
explicit cues never move, and never push an implicit cue they are anchored to; a bounded re-run (at most 4
passes, keeping the best) handles implicit cues scheduled before an explicit cue they collide with. Speech
obstacles live in an ordered staircase structure, so a push costs a few logarithmic queries. A clip's end is
resolved separately from its start, so a bed bounded by the last line can lead into the first one. It then
computes clip lengths (DESIGN §3.3), `content_end`, the tail and the total length (DESIGN §3.5; above 24 h
`timeline_too_long`, 0 `invalid_value`). Every layout diagnostic records whether it depends on estimated
durations (`Layout::estimate_dependent`), which is how dry runs downgrade such errors and how a render
decides to stop before synthesis. The same function serves plans and renders, which is why a plan predicts
the render up to estimation error.

### 5. Synthesis (tts, DESIGN §5)

Every cue becomes a synthesis request (provider instance, merged voice, text). The client checks the cache
first, then calls the provider under a global concurrency limit (`--jobs`, default 4) and a per-instance
`max_concurrency`, retrying network errors and 408/429/5xx with backoff, all within the cue's `timeout_ms`.
A local server that refuses the connection is not retried, and its other cues fail at once. Redirects are
never followed, responses are size-capped, audio far longer than the text can produce is refused, and keys
or URL credentials are scrubbed from every message. Child processes (piper, `say`, `command`) run in their
own process group without provider API keys in their environment. Providers prefer raw PCM and tag it with
its rate. Each result is the decoded audio at the provider's native rate (untrimmed, non-finite samples
repaired) plus optional word timings (seconds from the audio start). Failures become `synthesis_failed`
diagnostics with the provider's message and a hint; a cache that cannot be written becomes one
`cache_write_failed` warning per render.

### 6. DSP (dsp, DESIGN §4)

Decoding accepts 1–768 kHz sources, replaces non-finite samples (`audio_repaired`) and mixes sources wider
than stereo down with ITU-R BS.775 (`channels_downmixed`). Per cue: downmix to mono, resample
(high-quality sinc, latency compensated, exact output length), trim silence (5 ms RMS frames, threshold and
padding from `trim_silence`; word timings shift accordingly), normalize to the track's source target
(BS.1770 integrated loudness, capped at +24 dB), apply cue gain. Clips are decoded once per distinct source,
resampled, channel-adapted and normalized once per source.

The master is the only full-length buffer. Tracks are mixed one after another in windows of 64 Ki frames
straight into it; each window places the track's items (loops, loop crossfades, fades and crossfades relative
to each clip), pans them (constant power for mono, balance for stereo), scales them by the track gain (0 when
muted) and applies the track's ducking gain. Stems stream to their files; sidechain detectors are built one
at a time before the master is allocated, from the trigger tracks' pre-fader content (or their item intervals
in cue mode), with hold and lookahead on 1 ms blocks and gain ramps linear in dB at the attack/release slopes
(DESIGN §4.5). Peak memory is therefore about one master (23 MB per minute of stereo 48 kHz) plus the
processed speech and decoded sources, plus the stream a FLAC encode holds (DESIGN §9.3); `budget.rs` models it
and stops renders above `SONICCRAFT_MAX_RENDER_GB` before any TTS request.

The mastering loop (DESIGN §4.6) works in place: measure integrated loudness, apply gain, true-peak limit
(DESIGN §4.7: 4× oversampled peak detection, 5 ms lookahead, sliding minimum, 80 ms release, verified by an
independent measurement while streaming), re-measure, then bracket the gain (secant, then interpolation or
bisection) for at most 4 passes, keeping the closest; unreachable targets get the loudest master within 6 dB
of average limiting. Encoders: hound for WAV, LAME for MP3, flacenc for FLAC, ffmpeg via stdin for
ogg/opus/m4a; 16-bit output gets fixed-seed TPDF dither. Lossy files are then decoded and measured; a true
peak over the ceiling re-limits the master with a lower ceiling, and a loudness the codec moved more than 0.2 LU
off the target goes back into the master as gain; either encodes again (at most 3 passes). The report
describes the delivered file (`loudness.pre_encode` keeps the master's numbers).

### 7. Captions and side files (captions + engine, DESIGN §5.4, §6, §10)

Provider word timings are mapped onto each cue's display text (`caption` or `text`); without them, words
are estimated from speech regions in the trimmed audio and syllable weights (`alignment_estimated`). Times
are offset by the cue's placed start. Captions are segmented per cue (never merging cues) by characters per
line, lines, maximum and minimum duration, and written as WebVTT, SRT, `<stem>.words.json` and the
HyperFrames `<stem>.transcript.json`.

The engine also writes stems (`<stem>.stems/<track>.wav`, 32-bit float), the manifest and the preview, both
from one normalized report (paths relative to the file, no `timings_ms`, no `synthesis` cache counters, cached
durations `exact`, no `cache_write_failed`), so a warm-cache render writes the same bytes as a fresh one.
The preview template is a vendored copy of `packages/soniccraft-web-preview/preview.html` at
`crates/soniccraft-engine/assets/preview.html`, embedded with `include_str!`; a test fails when the two
differ, so after editing the package run `cp packages/soniccraft-web-preview/preview.html
crates/soniccraft-engine/assets/preview.html`. The payload (report, waveform, ducking envelopes, captions,
words) replaces the `__SONICCRAFT_DATA__` placeholder, with every `<` escaped as `<`.

Every output (audio, subtitles, words, transcript, stems, manifest, preview) is first written to a hidden
`.<name>.<random>.tmp.<ext>` next to its destination, created exclusively, and renamed into place only after
the last stage and the last cancellation check (`files.rs`, `Staging`). A commit that fails half-way restores
the files it replaced; a failed or cancelled render deletes only its temporary files. Files in the way that
are not outputs of their kind are refused before any work.

### 8. Interfaces (engine, mcp, cli, DESIGN §8, §9)

The engine API (`crates/soniccraft-engine/src/lib.rs`) is what both front-ends call:
`Engine::validate`, `Engine::plan`, `Engine::render`, `Engine::providers`, `Engine::voices`, and the cache
helpers, with `TimelineSource` (text + format + base directory; bounded reads of regular files),
`EngineOptions` (cache dir, read-cache switch, jobs, ffmpeg path, progress callback, `allow_command`,
`confine_outputs_to`) and `RenderOverrides`. Reports (`report.rs`) are serde types shared verbatim by
`--json`, MCP `structuredContent`, the manifest and the preview: add fields freely, never rename or remove
them.

- **CLI:** human output by default (source snippets with secrets shown as `<redacted>`), exactly one JSON
  document on stdout with `--json` (usage errors included), logs on stderr, exit codes 0/1/2/3 and 130;
  inputs are read up to the 5 MiB limit.
- **MCP:** rmcp-based stdio server serving both protocol eras; reports in `structuredContent` (compact above
  about 12,000 characters unless `detail: "full"`), file outputs as `resource_link`s (text before
  2025-06-18), progress, cancellation and at most two concurrent renders; an invalid timeline is a
  successful validate/dry-run result, a failed render or a bad argument an `isError` result; timelines run
  `command` providers only with `--allow-command` and write their own outputs only inside the base
  directory.

## Determinism checklist

- Frames are integers; `frames(ms) = round(ms × sample_rate / 1000)`. Convert to milliseconds only for
  reports, rounded consistently.
- Iterate maps in insertion or sorted order (`IndexMap`, sorted `Vec`); never `HashMap` iteration into
  output.
- Pseudo-randomness (dither, the mock voice) uses fixed-seed generators. The random names of temporary
  files never reach an artifact.
- No wall-clock values in artifacts: `timings_ms` lives only in the live `RenderReport`.
- The manifest and preview use relative paths; reports returned to callers use absolute paths. Paths printed
  in diagnostics that can reach a manifest go through `diagnostics::display_path`.
- Cache keys hash a **canonical** JSON: keys sorted recursively (the workspace enables
  `serde_json/preserve_order`, so sorting must be explicit), floats normalized (`-0.0` → `0.0`, at most 6
  decimals, non-finite rejected).
- Determinism is tested by rendering the same timeline twice and comparing bytes (audio, subtitles,
  manifest, preview).

## Caching (DESIGN §5.1)

Key: `blake3(canonical JSON)` of `{cache_version, provider_type, api_version, base_url (kokoro,
OpenAI-compatible, command and piper, only when not the default), model, voice_id, language, text,
settings, output_format, command and stdin (command/piper), program and its content hash, files named by
literal arguments, base_dir (command), model_file (piper)}`. Directory: CLI `--cache-dir` > env
`SONICCRAFT_CACHE_DIR` > `<timeline dir>/.soniccraft/cache`. Entries: `<key>.wav` (32-bit float, native
rate, untrimmed, so trim settings can change without re-synthesis) and `<key>.json` (metadata, word
timings). Writes are atomic (exclusively created temp file + rename). `--no-cache` skips reads but still
writes. Bump `cache_version` when the meaning of cached audio changes.

## Diagnostics (DESIGN §7)

`core::diagnostics::Diagnostic { severity, code, message, path, line, column, hint, related }`. Codes live
in `diagnostics::codes` and are a public API: add new codes there **and** to DESIGN §7 and
`skills/soniccraft/reference/diagnostics.md`; never rename one. Paths use `field.sub[index]` notation built
with `diagnostics::path`. Use `diagnostics::did_you_mean` for suggestions (tuned on agent typos) and make
hints mechanical: name the exact field and value to write. `with_related` never adds an id twice.

## Testing strategy

- Unit tests next to the code; integration tests under each crate's `tests/`.
- No test touches a paid API. Providers are tested against recorded or synthetic responses and an in-process
  HTTP test server; end-to-end renders use the `mock` provider (and `macos_say` behind
  `#[cfg(target_os = "macos")]`).
- `soniccraft-core` keeps the JSON Schema in `skills/soniccraft/reference/timeline-schema.json` under a
  golden test, checks that the schema and the compiler agree on accepted and rejected documents, and checks
  that every example and template timeline validates and compiles.
- CI renders `examples/offline-demo/timeline.mock.yaml` twice and compares the outputs byte for byte, and
  validates every example and template.

## Common changes

**Add a DSL field.** Add it to `model.rs` (with a doc comment: it becomes the schema description), to the
IR if downstream crates need it, implement validation in `lower.rs` with diagnostics and hints, regenerate the
JSON Schema golden file, update DESIGN §2 and `skills/soniccraft/reference/dsl-reference.md`, and use it in a
template or example if it is user-facing.

**Add a diagnostic code.** Add the constant to `diagnostics::codes`, document it in DESIGN §7 and in
`skills/soniccraft/reference/diagnostics.md` (meaning and mechanical fix), and test code, path and hint.

**Add a provider.** Add the type and aliases to `ProviderTypeName` in `model.rs`, its metadata to
`core::providers` (env var, base URL, default model and voice, concurrency, chars per second, word-timing
support, request limit), implement it in `soniccraft-tts` with recorded-response tests (and its
`settings_keys`), document it in DESIGN §5.2 and `skills/soniccraft/reference/providers.md`, and add it to
the provider tables in the README and `SKILL.md`.

**Change an example.** Edit `examples/<name>/timeline.yaml`, then run `scripts/sync-templates.sh` (the
`tech-briefing`, `two-person-podcast` and `offline-demo` skill templates are generated from the examples)
and copy the templates into `crates/soniccraft-cli/templates/` (byte-identical, enforced by a test;
`offline-demo-mock.yaml` there is `examples/offline-demo/timeline.mock.yaml`). Regenerate audio assets only
with `scripts/generate-example-assets.sh`.

**Change the web preview.** Edit `packages/soniccraft-web-preview/preview.html`, run its tests, then copy it
to `crates/soniccraft-engine/assets/preview.html` (see §7).
