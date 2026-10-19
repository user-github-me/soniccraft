# SonicCraft MCP tools

`soniccraft mcp` runs a Model Context Protocol server on stdio (newline-delimited JSON-RPC 2.0).
`soniccraft-mcp` is the same server as a zero-argument binary, for clients that cannot pass arguments.
Timelines run local programs (`command` providers, a piper `command`) only when the server is started with
`soniccraft mcp --allow-command` or with `SONICCRAFT_ALLOW_COMMAND=1` in its environment; otherwise such
providers are `provider_unavailable` errors. It serves both protocol eras: the `initialize` handshake
(revisions 2024-11-05 through 2025-11-25, used by Codex and Cursor) and the stateless 2026-07-28 revision with
`server/discover` (which Claude Code probes first). stdout carries only protocol messages; logs go to stderr.

The tools wrap the same engine as the CLI and return the same reports as `soniccraft ... --json` (large
reports come back compact unless you ask for `detail: "full"`, see [Results](#results)).

| Tool | Arguments | Result |
|---|---|---|
| `soniccraft_validate_timeline` | `{yaml_content?, yaml_path?, base_dir?, check_credentials?, detail?}`: exactly one of `yaml_content` / `yaml_path` | ValidationReport |
| `soniccraft_dry_run` | `{yaml_content?, yaml_path?, base_dir?, detail?}` | PlanReport (+ the ASCII diagram as text) |
| `soniccraft_render` | `{yaml_content?, yaml_path?, output_path?, format?, base_dir?, export_subtitles?, no_cache?, detail?}` | RenderReport |
| `soniccraft_get_schema` | `{detail?}` | A DSL quick reference (+ the JSON Schema with `detail: "full"`) |
| `soniccraft_list_providers` | `{}` | Providers, availability, credential status, default voices |
| `soniccraft_list_voices` | `{provider}` | Voices of one provider (may call the provider API) |

`path` is accepted everywhere as a synonym of `yaml_path`.

In clients the callable name is prefixed with the server name you registered, e.g. Claude Code
`mcp__soniccraft__soniccraft_render`, or `mcp__plugin_soniccraft_soniccraft__soniccraft_render` when installed
as the SonicCraft plugin.

## Arguments

| Argument | Type | Used by | Meaning |
|---|---|---|---|
| `yaml_content` | string | validate, dry_run, render | The timeline document itself (YAML or JSON). Relative asset paths resolve against `base_dir`. |
| `yaml_path` | string | validate, dry_run, render | Path of a timeline file (synonym: `path`): a regular file of at most 5 MiB. Relative paths resolve against `base_dir`. |
| `base_dir` | string | validate, dry_run, render | Directory that relative paths resolve against. Default: the server's `--base-dir`, else env `CLAUDE_PROJECT_DIR`, else the server's working directory. A leading `~` is expanded. |
| `check_credentials` | bool | validate | Also verify API keys with a cheap authenticated request (network). Keys go only where the key policy allows (see [Security](#security)). |
| `output_path` | string | render | Output file; overrides `output.path`. Its extension selects the format unless `output.format` is set (then pass `format` too). May point anywhere; a timeline's own `output.path` must stay inside `base_dir`. |
| `format` | string | render | Output format overriding `output.format`: wav, mp3, flac (built in), ogg, opus, m4a (need ffmpeg); case-insensitive. Pass it when `output_path` names another format than the timeline, otherwise e.g. WAV data lands in an `.mp3` file (the summary then says so). |
| `export_subtitles` | bool | render | `true`: write the subtitle formats listed in `output.export_subtitles` (vtt + srt when it lists none); `false`: write none. Omit to follow the timeline. |
| `no_cache` | bool | render | Ignore cached TTS audio and re-synthesize (results are still written to the cache). |
| `detail` | string | validate, dry_run, render, get_schema | `auto` (default): the whole report while it is small, else a compact one; `summary`: always compact; `full`: everything (get_schema: adds the JSON Schema). |
| `provider` | string | list_voices | Built-in provider name or alias, e.g. `elevenlabs`, `kokoro`, `macos_say` (case and `-`/`_` do not matter here). |

Prefer absolute paths, or pass `base_dir` explicitly: the server's working directory depends on the client.
When rendering inline `yaml_content`, also pass `output_path` (or set `output.path`) so you know where the
audio lands.

## Results

Every result carries the report as `structuredContent` (a JSON object), mirrors it as JSON text, and adds
a short human-readable summary. Render results also include `resource_link` blocks with `file://` URIs
(absolute paths) for the audio and every side file. Audio is never returned inline as base64.

- An **invalid timeline is a successful call** for `soniccraft_validate_timeline` and `soniccraft_dry_run`:
  `isError: false` with `valid: false` and the diagnostics. Fix and call again.
- A **failed render** returns `isError: true` with `{message, diagnostics}` so you can repair and retry.
- **Argument mistakes** (unknown names, wrong types, bad values, a `yaml_path` that is not a regular file of at
  most 5 MiB) are `isError: true` results naming the valid arguments, e.g. "Unknown argument `timeline_path`
  ... Did you mean `yaml_path`?": fix the call and retry.
- **Large reports are compact** (`detail: "auto"` above about 12,000 characters, or `detail: "summary"`): the
  same keys, plus `detail: "summary"`, `items_total`, `items_truncated`, `diagnostics_total`,
  `diagnostic_counts`, `diagnostics_truncated` and `full_report`, which says where everything is (the manifest
  path, or call again with `detail: "full"`). Errors and warnings are kept (up to 50), and so are the first
  and last 10 items plus every item a diagnostic names or the overlap policy moved. The whole result stays
  under about 40,000 characters. `detail: "full"` returns everything; a result too large for one message is
  shortened step by step, never turned into a protocol error.
- Sessions on protocol revisions older than 2025-06-18 get a `Files written:` text block instead of
  `resource_link` blocks.
- Long renders send `notifications/progress` when the request carries `_meta.progressToken`, and stop on
  `notifications/cancelled`. Files are put in place only when the whole render succeeds: a failed or
  cancelled render removes its partial files and leaves earlier outputs untouched.
- At most two renders run at a time (`SONICCRAFT_MCP_MAX_RENDERS`); a queued render reports "Waiting for
  another render to finish" and can be cancelled while it waits.

### ValidationReport

```json
{
  "valid": true,
  "diagnostics": [],
  "summary": {
    "tracks": 4, "speech_cues": 3, "clips": 2, "speakers": ["narrator", "guest"],
    "providers": ["elevenlabs", "local_kokoro"], "characters": 276, "cached_cues": 0,
    "output_format": "mp3", "sample_rate": 48000, "target_lufs": -16.0
  }
}
```

`summary` is present when the timeline could be lowered far enough to summarize. Each diagnostic is
`{severity, code, message, path?, line?, column?, hint?, related?}` (see [diagnostics.md](diagnostics.md)).

### PlanReport (dry run)

```json
{
  "valid": true,
  "diagnostics": [ { "severity": "info", "code": "cue_pushed", "message": "...", "path": "tracks[1].sequence[2]" } ],
  "duration_ms": 20616.9,
  "estimated": true,
  "sample_rate": 48000,
  "tracks": [ { "id": "ambient_bed", "type": "music", "gain_db": -3.74, "pan": 0.0, "mute": false,
                "ducked_by": ["voice_primary", "voice_secondary"] } ],
  "items": [
    { "id": "cue_01", "kind": "cue", "track_id": "voice_primary", "start_ms": 0.0, "end_ms": 4838.7,
      "duration_ms": 4838.7, "duration_source": "estimate", "speaker": "narrator",
      "provider": "elevenlabs", "voice_id": "21m00Tcm4TlvDq8ikWAM", "text": "Autonomous software ..." },
    { "id": "cue_03", "kind": "cue", "track_id": "voice_primary", "start_ms": 11213.7, "end_ms": 18116.9,
      "duration_ms": 6903.2, "duration_source": "estimate", "pushed_from_ms": 5188.7, "...": "..." },
    { "id": "ambient_bed", "kind": "clip", "track_id": "ambient_bed", "start_ms": 0.0, "end_ms": 20616.9,
      "duration_ms": 20616.9, "duration_source": "source", "source": "assets/audio/synthwave_loop.wav",
      "open_ended": true, "looped": true }
  ],
  "ducking": [ { "track_id": "ambient_bed", "mode": "sidechain", "reduction_db": 18.0,
                 "triggers": ["voice_primary", "voice_secondary"],
                 "regions": [ { "start_ms": 0.0, "end_ms": 11113.7 }, { "start_ms": 11133.7, "end_ms": 18366.9 } ] } ],
  "synthesis": { "cues": 3, "cached": 0, "characters_to_synthesize": 276,
                 "by_provider": { "elevenlabs": { "provider_type": "elevenlabs", "cues": 2, "cached": 0, "characters": 182 },
                                  "local_kokoro": { "provider_type": "kokoro", "cues": 1, "cached": 0, "characters": 94 } } },
  "diagram": "..."
}
```

(Numbers illustrative.) Items are sorted by start time. `kind` is `cue` | `pause` | `clip`;
`duration_source` is `estimate` (heuristic), `cache` (exact, from cached audio), `exact` (measured in this
render, or the mock voice in a plan: dry runs render mock cues in-process), `source` (clip source file) or
`fixed` (pauses, explicit lengths). `estimated` is false when nothing is an estimate (e.g. a mock-only
timeline). `pushed_from_ms` is the original tentative start of a cue moved by the overlap policy.
`ducking[].mode` is the configured mode, but dry runs always compute the regions from item intervals (as `cue`
mode does); renders use the configured mode. `synthesis.characters_to_synthesize` is what a render would bill.
Layout errors that depend on estimated durations are warnings here, ending in "(based on estimated speech
durations; the render checks again with exact durations)".

### RenderReport

```json
{
  "output": { "path": "/abs/project/timeline.mp3", "format": "mp3", "sample_rate": 48000, "channels": 2,
              "bitrate_bps": 320000, "duration_ms": 21043.5, "bytes": 842112 },
  "loudness": { "integrated_lufs": -16.3, "true_peak_dbtp": -1.4, "sample_peak_dbfs": -1.8,
                "loudness_range_lu": 4.1, "target_lufs": -16.0, "true_peak_target_dbtp": -1.0,
                "normalized": true, "gain_db": 3.2, "limiter_max_reduction_db": 1.8, "iterations": 2,
                "pre_encode": { "integrated_lufs": -16.0, "true_peak_dbtp": -1.3, "sample_peak_dbfs": -1.7,
                                "loudness_range_lu": 4.1, "ceiling_dbtp": -1.0, "compensation_passes": 0 } },
  "files": { "subtitles": [ { "format": "vtt", "path": "/abs/project/timeline.vtt" },
                            { "format": "srt", "path": "/abs/project/timeline.srt" } ] },
  "tracks": [ "...as in the plan..." ],
  "items": [ "...as in the plan, with exact durations..." ],
  "ducking": [ "...as in the plan, from the configured mode..." ],
  "synthesis": { "cues": 3, "cache_hits": 0, "synthesized": 3, "providers_called": ["elevenlabs", "local_kokoro"] },
  "diagnostics": [],
  "timings_ms": { "...": "wall-clock per stage (not deterministic; omitted from the manifest)" }
}
```

(Numbers illustrative.) `loudness` describes the **delivered file**: for mp3, ogg, opus and m4a the encoded
file is decoded and measured, and `loudness.pre_encode` holds the master that went into the encoder (absent
for wav and flac). Codecs shift loudness slightly; a drift above 0.2 LU is corrected by re-encoding (at most 3 passes,
`pre_encode.compensation_passes`), so report `integrated_lufs` and `true_peak_dbtp` as what the file
measures. `gain_db`,
`limiter_max_reduction_db` and `iterations` are the mastering loop's. If a lossy file still overshoots the
ceiling after compensation you get `true_peak_exceeded`; `loudness_target_missed` gives the level reached when
mastering, or the delivered file, could not hit the target. `files.stems`, `files.manifest` and `files.preview` appear when written.
`bit_depth` appears for wav/flac, `bitrate_bps` for lossy formats. `integrated_lufs` is `null` for silent
output.

### Providers and voices

`soniccraft_list_providers` returns `{providers: [{name, aliases, display_name, local, word_timestamps,
available, status, api_key_env?, default_voice?, default_model?, notes}]}`; `available` says whether the
provider can be used right now (a local server such as Kokoro gets a quick probe of at most 300 ms) and
`status` why not (e.g. the missing env var, or `no Kokoro-FastAPI server answered at <url>`).

`soniccraft_list_voices` returns `{provider, voices: [{id, name, language?, gender?, description?}]}`;
put `id` into `voice_id`.

## Security

A timeline is data an agent may have found anywhere, so the server treats it as untrusted:

- **Programs:** `command` providers and a piper instance with its own `command` run only when the server was
  started with `--allow-command` (or `SONICCRAFT_ALLOW_COMMAND=1`). Enable it only for timelines you trust.
- **Files:** a timeline's own `output.path` must stay inside the call's base directory (`base_dir`,
  `--base-dir`, `CLAUDE_PROJECT_DIR`); an `output_path` argument is the caller's explicit choice and may point
  anywhere. Existing files that are not SonicCraft outputs are never overwritten.
- **API keys** are read from the server's environment, never from timelines, and sent only to each provider's
  official API host, to servers on this machine (the provider's own key variable only), or to hosts listed in
  `SONICCRAFT_ALLOWED_HOSTS` (set it in the server's environment; e.g. `SONICCRAFT_ALLOWED_HOSTS=tts.example.com`
  for a remote OpenAI-compatible server). `check_credentials` follows the same rule; redirects are never
  followed.
- **Reads:** `yaml_path` must be a regular file of at most 5 MiB; `/dev/zero` or a FIFO fails at once.

## Example: validate inline YAML, then render

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"soniccraft_validate_timeline",
 "arguments":{"yaml_content":"version: \"1.0\"\ntracks:\n  - id: vo\n    type: speech\n    provider: mock\n    sequence:\n      - text: Hello from SonicCraft.\n","base_dir":"/abs/project"}}}
```

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"soniccraft_render",
 "arguments":{"yaml_path":"/abs/project/timeline.yaml","output_path":"/abs/project/out/episode.mp3","format":"mp3","export_subtitles":true}}}
```

## Client timeouts and approval

Renders that synthesize many cues can take minutes. Claude Code's per-call default is very long (set a
per-server `"timeout"` in milliseconds if you want a bound); Codex defaults to 60 s, so set
`tool_timeout_sec = 900` in its `config.toml` entry. Validation and dry runs are fast and never call
paid APIs, so call them freely before rendering.

`soniccraft_render` writes files (`readOnlyHint: false`, `destructiveHint: true`), so clients ask before
running it. Interactive Codex prompts for approval; non-interactive `codex exec` (approval policy `never`)
fails the call with "MCP tool call requires approval" unless the server entry allows it:

```toml
[mcp_servers.soniccraft]
default_tools_approval_mode = "approve"          # every tool of this server
# or only the render tool:
# [mcp_servers.soniccraft.tools.soniccraft_render]
# approval_mode = "approve"
```

or for one run: `codex exec -c 'mcp_servers.soniccraft.default_tools_approval_mode="approve"' ...`.
