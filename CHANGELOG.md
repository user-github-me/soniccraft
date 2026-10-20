# Changelog

All notable changes to SonicCraft are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/) (before 1.0, minor versions may contain breaking changes).
Diagnostic codes and report fields are treated as public API.

## [Unreleased]

### Added

- One-line installers: `install.sh` (macOS, Linux, Git Bash) and `install.ps1` (Windows PowerShell 5.1 and 7)
  download a release archive, verify its SHA-256 and install `soniccraft` and `soniccraft-mcp`. The Installers
  workflow tests both on Linux, macOS and Windows after each release.
- `AGENTS.md` (imported by `CLAUDE.md`) for AI agents that use or change SonicCraft, and a "Use it from an AI
  agent" section at the top of the README.
- Distribution: a Homebrew tap (`brew install user-github-me/soniccraft/soniccraft`), a Scoop bucket, the
  official MCP Registry entry `io.github.user-github-me/soniccraft` (the container image carries the
  `io.modelcontextprotocol.server.name` label), crates.io metadata and READMEs for every crate,
  cargo-binstall metadata, `scripts/publish-crates.py` and a trusted-publishing workflow for crates.io.

### Changed

- The CLI package is now named `soniccraft` (it was `soniccraft-cli`), so `cargo install soniccraft` installs
  it. The binaries and the `crates/soniccraft-cli` directory keep their names.

## [0.1.0] - 2026-10-20

First release: timeline DSL v1.0 and engine v0.1.

### Added

- **Timeline DSL v1.0** (YAML 1.2 or JSON): output and mastering settings, subtitle layout, timing policy,
  mix reference level, voice defaults, a speakers registry, provider instances, and speech, music and sfx
  tracks with cues, pauses and clips. Values accept unit strings (`"350ms"`, `"1.5s"`, `"1m30s"`,
  `"00:01:30.500"`, `"320k"`).
- **Validation with agent-first diagnostics:** stable codes, `field.sub[index]` paths, line/column for YAML,
  did-you-mean hints for unknown keys, enum values, references and providers; every problem reported at
  once; secrets in timelines rejected.
- **Timing:** sequential placement with pauses, absolute starts, and relative anchors (`after_cue`,
  `with_cue`, `before_cue`, signed offsets) on cues, clips and tracks; dependency-cycle detection; a
  sample-accurate layout engine with the speech-overlap policy (`push` by default, or `warn`, `error`,
  `allow`) and per-cue `allow_overlap`.
- **TTS providers:** ElevenLabs (word timestamps), OpenAI-compatible `/audio/speech` servers, Cartesia
  (word timestamps), Kokoro via Kokoro-FastAPI (word timestamps), Piper, macOS `say`, custom commands and a
  deterministic mock voice; content-addressed cache; concurrency limits; retries with backoff; credential
  checks; voice listing; dry-run duration estimates in characters per second.
- **Native DSP:** decoding, resampling, silence trimming, per-source loudness normalization, loops with
  optional seam crossfades, fades (equal-power, linear, exponential), clip crossfades, constant-power
  panning, sidechain and cue-driven ducking with attack, release, hold and lookahead.
- **Mastering:** EBU R128 / BS.1770 loudness normalization with presets (`podcast` -16, `streaming` -14,
  `broadcast` -23, `atsc` -24 LUFS) and an iterative, 4× oversampled true-peak limiter.
- **Encoders:** WAV (16/24-bit integer, 32-bit float), MP3 (LAME, CBR; cargo feature `mp3`, on by default),
  FLAC (16/24-bit); ogg, opus and m4a through ffmpeg; fixed-seed TPDF dither.
- **Subtitles:** WebVTT, SRT, JSON with per-word timestamps, and a HyperFrames-compatible transcript;
  speaker labels; caption overrides.
- **Side files:** per-track stems, a deterministic manifest, and a self-contained HTML preview (waveform,
  ducking envelopes, captions).
- **CLI** `soniccraft`: `validate`, `plan` (`dry-run`), `render`, `schema`, `init`, `providers`, `voices`,
  `cache`, `mcp`; `--json` output and exit codes 0/1/2/3.
- **MCP server** (`soniccraft mcp`, `soniccraft-mcp`) serving both protocol eras: tools
  `soniccraft_validate_timeline`, `soniccraft_dry_run`, `soniccraft_render`, `soniccraft_get_schema`,
  `soniccraft_list_providers`, `soniccraft_list_voices`.
- **Agent Skill** `skills/soniccraft` with references and five templates; Claude Code plugin and marketplace
  manifests; Codex skill metadata.
- **Examples** (`tech-briefing`, `two-person-podcast`, `offline-demo`) with procedurally generated,
  royalty-free assets and the script that generates them.
- Dual MIT OR Apache-2.0 licensing; CI on Linux and macOS.
- A container image, `ghcr.io/user-github-me/soniccraft` (linux/amd64 and linux/arm64, with ffmpeg), published
  from each release's Linux binaries; `docker build .` builds it from source.

### Added in pre-release hardening

- Release workflow: prebuilt `soniccraft` and `soniccraft-mcp` for Linux (x86_64, arm64), macOS (Apple
  silicon, Intel) and Windows (x86_64), each with and without the LGPL MP3 encoder, with `SHA256SUMS` and a
  `THIRD-PARTY-LICENSES.txt` in every archive.
- The `soniccraft-mcp` npm launcher (`npx -y soniccraft-mcp`): runs `soniccraft mcp`, downloading the
  matching release on first use and verifying it against checksums pinned in the package.
- `licenses/` with the LAME and LGPL/GPL texts and the steps to relink a modified LAME; a cargo-deny license,
  ban and source policy (`deny.toml`).
- CI: the minimum Rust version, Windows (build and smoke render), cargo-deny, rustdoc, and the npm packages
  on Node 20 and 24.
- New diagnostics: `comment_in_text` (an unquoted ` #` cut a cue's text short), `suspicious_value` (seconds
  written as milliseconds; `{text}`/`{voice_id}` as a command argument of its own), `speakers_share_voice`,
  `unknown_voice` (macOS voice not installed, piper model not found, already in validate),
  `track_anchor_out_of_order`, `timeline_too_long` (over 24 h, or over the render memory budget),
  `loudness_target_missed`, `true_peak_exceeded`, `cache_write_failed`, `audio_repaired`,
  `channels_downmixed`.
- Render memory budget: `SONICCRAFT_MAX_RENDER_GB` (default 8); a render that would need more stops before
  any TTS request, and validate/plan warn.
- `SONICCRAFT_ALLOWED_HOSTS`: hosts besides each provider's own API host and servers on this machine that may
  receive API keys.
- MCP: `detail: auto|summary|full` on validate, dry run, render and get_schema (compact reports above about
  12,000 characters, whole results under about 40,000; `full` never turns into -32603); a `format` argument on
  render; at most two concurrent renders (`SONICCRAFT_MCP_MAX_RENDERS`), queued renders report progress and
  can be cancelled; 2026-07-28 results carry `_meta` serverInfo.
- CLI: `soniccraft mcp --allow-command` / `SONICCRAFT_ALLOW_COMMAND=1`.
- `RenderReport.loudness.pre_encode`: the master that went into a lossy encoder.
- Sources with more than two channels (5.1, 7.1) are mixed down with ITU-R BS.775 instead of being
  unusable.
- Docs: a Security and a Resources section in the README, a Setup section in the skill (install, verify,
  failed MCP server), Codex approval settings for `codex exec`.
- Library APIs: `soniccraft_core::parse::secret_spans`, `parse::MAX_NODES`, `values::parse_duration`
  (`DurationError`), `values::MAX_BITRATE_BPS`, `spelling_of` on every name enum, `Compiled.assets`
  (`AssetRef`), `ir::checked_ms_to_frames`, `diagnostics::display_path` and `human_duration`; layout
  `Layout::estimate_dependent`, `depends_on_estimates`, `diagnostics_with_estimates`, `has_certain_errors`;
  dsp `downmix`, `master::master_with_cancel`, `limiter::limit_in_place`, `loudness::measure_file`,
  `measure_with_cancel`, `duck::cue_regions`, `wav::read_repaired`, new `DecodeInfo` fields; tts
  `render_mock`, `resolve_macos_voice`, `resolve_piper_model`, `probe_reachable`, `key_host_allowed`,
  `settings_keys`, `command_argument_warnings`, `error_codes`, `TtsOptions.child_env_remove`,
  `SynthesisResult.cache_write_error`; engine `MAX_TIMELINE_BYTES`, `TimelineSource::from_reader`,
  `LoudnessReport.pre_encode`, working `EngineOptions.allow_command` and `confine_outputs_to`; mcp
  `ServerOptions::max_concurrent_renders`, `ALLOW_COMMAND_ENV`, `MAX_RENDERS_ENV`.

### Behaviour settled in pre-release hardening

- enum spellings are exact (`Speech`, `MP3`, `equal-power` are `invalid_value` with a
  did-you-mean); duration units are lowercase and clock seconds must be below 60; bitrate strings take no
  sign or exponent; `version: "01.0"` is invalid; padded ids (`" v "`) are `invalid_id`.
- every time is at most 24 h (larger values, usually epoch milliseconds, are `invalid_value`),
  and a timeline longer than 24 h is `timeline_too_long`; `target_lufs` must be in [-60, -5] (was
  [-70, -5]); a total length of 0 is `invalid_value`.
- a misspelled speaker (a close match of a defined speaker) is an error; voice keys inside
  `settings` (`settings.voice`) are a `field_not_applicable` warning.
- MCP servers refuse `command` providers (and piper with its own `command`) unless started
  with `--allow-command`; a timeline's own `output.path` must stay inside the MCP base directory.
- API keys go only to the provider's own API host, to servers on this machine (only the
  provider's own key variable), or to `SONICCRAFT_ALLOWED_HOSTS`; regional, remote and other local-network
  endpoints must be listed, and so must a local server that should get a variable named by `api_key_env`.
- command and piper cache keys include the program, its content hash, script files and the
  piper model, so those cues re-synthesize once.
- `soniccraft_core::Compiled` has a new field `assets` (build it with
  `..Compiled::default()`); `DspError` has a new variant `Cancelled`; the name enums' `parse` is exact
  (case-sensitive, no trimming), so `builtin_type("Eleven-Labs")` is `None`; `Layout.total_frames` is capped
  at 24 h.
- JSON input has the YAML limits (250000 values, 64 levels) and diagnostics for comments and trailing
  commas; stdin/MCP auto-detection no longer reads such JSON as YAML.
- Hints convert values (`44.1k` → `44100`, `50%` → `0.5`, `target_lufs: 16` → `-16`, `youtube` →
  `streaming`, `start_at_ms: a2` → `start_relative: {after_cue: a2}`) and know common key synonyms (`file`,
  `voice_name`, `start_after`, `pause_ms`), with unit notes for bare numbers.
- `secret_in_timeline` also covers annotation keys (`x-api-key`), keys inside provider strings (`Bearer`,
  `--api-key=`, URLs) and URL passwords; diagnostics never repeat a secret, and CLI source snippets show it as
  `<redacted>`.
- The JSON Schema encodes conflicts, applicability by track type, time sign and length rules, and
  null-as-absent.
- Renders commit their outputs atomically: a cancelled or failed render never deletes or half-writes
  earlier outputs, and files that are not SonicCraft outputs are never overwritten.
- Lossy outputs (mp3, ogg, opus, m4a) are decoded and measured as delivered, with codec true-peak and
  loudness-drift compensation (at most 3 passes); the report's loudness describes the delivered file.
- Mastering brackets the gain and keeps the pass closest to the target; an unreachable target gives the
  loudest master within 6 dB of limiting, the same for every target out of reach.
- Dry runs render mock voices exactly; ducking regions are computed from intervals; layout errors that depend
  on estimated durations are warnings in validate and plan; a render stops before synthesis on layout errors
  that do not.
- About 4× less render memory (60-minute bed: 6.2 → 1.4 GB footprint): windowed mixing, streamed stems,
  in-place limiting.
- validate reports uninstalled macOS voices, missing piper models, unreachable local servers (300 ms probe),
  refused key hosts, placeholder keys (`${VAR}` counts as unset) and assets of documents with other errors.
- TTS: `timeout_ms` is a per-cue deadline (retries and waits included); a local server that refuses the
  connection is not retried and its other cues fail at once; redirects are never followed; responses are
  size-capped and implausibly long audio is refused; keys and URL credentials are scrubbed from messages;
  child processes lose API keys and are killed with their process group; option-like command arguments are
  refused; piper voices next to the timeline are found; unread `settings` keys are reported
  (`setting_ignored`).
- Layout: `cue_pushed` is reported once per moved cue; an explicit cue never pushes an implicit cue it
  depends on; a clip's end is scheduled separately from its start (a bed bounded by the last line can lead
  into the first one); track anchors use declaration order; pushing scales to tens of thousands of cues; plan
  diagrams are cut at 256 KB.
- MCP: `resource_link` only from protocol 2025-06-18 (older sessions get a text block); unknown or invalid
  tool arguments are `isError` results (SEP-1303); requests with a null, fractional, boolean, array or object
  id get -32600, positional params -32602; `yaml_path` must be a regular file of at most 5 MiB;
  `soniccraft_get_schema` returns the quick reference by default (`detail: "full"` adds the JSON Schema).
- CLI: with `--json`, usage errors (exit 2) also print one JSON document; timelines are read up to the 5 MiB
  limit (`/dev/zero`, FIFOs and endless stdin fail at once); a render stopped by its environment or options
  ends with `render stopped: N errors`; `soniccraft voices` accepts provider names in any case; diagnostics of
  long single-line timelines render in linear time.
- The minimum supported Rust version is 1.89 (1.88 was documented, but serde-saphyr 2.0.0 needs 1.89).
- Release binaries are stripped of symbols (about 3 MB, 18% smaller).
- `cargo run -p soniccraft-cli` runs the `soniccraft` binary.

### Fixed in pre-release hardening

- Crashes, runaway memory or run time on huge times, NaN/infinite samples, 4 MHz sources, very large timelines
  and minified single-line documents.
- Headerless VBR MP3s are decoded to their real end, so plan and render agree.
- No false `dependency_cycle` through track anchors or bed + intro patterns; no repeated or cascading
  diagnostics after a dropped value; misspelled pause items are one `unknown_field`.
- No absolute paths in `format_extension_mismatch` and other persisted messages.
- Temporary files are created exclusively with unpredictable names.
- Docs: the README's `.mcp.json` example no longer forwards an unset key as a literal `${VAR}`; the install
  command uses `--locked`; piper setup matches the voice lookup; `diagnostics.md` and `dsl-reference.md`
  list the right codes for `label`, `crossfade_ms` and unparsable times.
