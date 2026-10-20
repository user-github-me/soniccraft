# SonicCraft TTS providers

Eight built-in provider types. Reference one by its built-in name (`provider: kokoro`) or configure a
named instance under `providers:` (see [dsl-reference.md](dsl-reference.md#providers)). Facts below are
current as of October 2026.

Check what works on this machine before choosing: `soniccraft providers` lists every provider with its
availability, credential status and default voice; `soniccraft voices <provider>` lists voices.

| Type (aliases) | Runs | Needs | Default voice | Word timings | Concurrency | Est. chars/s |
|---|---|---|---|---|---|---|
| `elevenlabs` (`eleven_labs`, `11labs`) | cloud | `ELEVENLABS_API_KEY` | none: `voice_id` required | yes | 2 | 15.5 (v3/v4: 14) |
| `openai` (`open_ai`) | cloud or any OpenAI-compatible server | `OPENAI_API_KEY` | `alloy` | no (estimated) | 4 | 15 |
| `cartesia` | cloud | `CARTESIA_API_KEY` | none: `voice_id` required | yes | 2 | 15 |
| `kokoro` (`local_kokoro`, `kokoro_fastapi`) | local server | Kokoro-FastAPI (`KOKORO_BASE_URL`) | `af_heart` | yes | 1 | 16 |
| `piper` | local binary | `piper` + a voice model | none: `voice_id` required | no (estimated) | 2 | 17 |
| `macos_say` (`say`, `system`) | local, macOS only | macOS | `Samantha` | no (estimated) | 4 | 17 |
| `command` | local binary | your argv template | none: `voice_id` required | no (estimated) | 2 | 15 |
| `mock` | in-process | nothing | `mock` | yes (exact) | 8 | exact |

"Word timings" means the provider returns per-word times that drive subtitles. Without them SonicCraft
estimates word times from the rendered audio (speech regions + syllable weights) and reports info
`alignment_estimated`. "Concurrency" is the default `max_concurrency`; the CLI's `--jobs N` (default 4)
caps all providers together.

## Credentials and network

- API keys come **only** from environment variables (default names above; rename with `api_key_env`).
  A key in the timeline is the error `secret_in_timeline`. A variable that holds an unexpanded placeholder
  (`${ELEVENLABS_API_KEY}`, `$VAR`, `%VAR%`; some MCP clients pass their config template through when the
  variable is unset) counts as missing.
- **Where keys go:** only to the provider's own API host (exactly the host of its default `base_url`:
  api.elevenlabs.io, api.openai.com, api.cartesia.ai), to servers on this machine (localhost, 127.0.0.0/8,
  ::1; only the provider type's own variable, e.g. `OPENAI_API_KEY` or `KOKORO_API_KEY`), or to hosts listed
  in `SONICCRAFT_ALLOWED_HOSTS` (comma-separated, `*.example.com` patterns, `*` for any). A variable chosen
  with `api_key_env` for a local server, and any key for a host elsewhere on the local network (10/8,
  172.16/12, 192.168/16, link-local, fc00::/7, `*.local`), need the host listed. Set it in the
  environment of soniccraft or the MCP server, never in the timeline. Any other `base_url` with a key is
  `provider_unavailable` before anything is sent, even offline in `validate`. Requests without a key may go
  anywhere. Redirects are never followed, and keys echoed by a server are removed from every message.
- `soniccraft validate` reports `missing_credentials` (an error for cues that are not cached yet; only a
  warning when every cue of that provider is already cached). `soniccraft validate --check-credentials`
  also verifies the key with a cheap authenticated request.
- Unusable local providers are `provider_unavailable`: macos_say off macOS, a missing piper/command binary,
  a local server (Kokoro-FastAPI, an OpenAI-compatible server on this machine) that does not answer a 300 ms
  probe, or (over MCP) a `command` provider when the server was started without `--allow-command`. Voices
  that are not installed (a macOS voice, a piper model) are `unknown_voice`, already in `validate`.
- Retries: network errors and HTTP 408, 429, 500, 502, 503, 504 are retried with exponential backoff
  (1 s, 2 s, 4 s; `Retry-After` honoured up to 60 s), at most 3 times, all within the instance's
  `timeout_ms` (default 120 s per cue, retries and waits included). A local server that refuses the
  connection is not retried, and its remaining cues fail at once ("not attempted"). Other 4xx fail fast with
  `synthesis_failed`, carrying the provider's message and a hint (bad key: check the env var; unknown voice:
  list voices).
- A cue longer than the provider's per-request limit is `text_too_long`: split it into several cues.
- `settings` keys a provider does not read (see "Settings read" below) are reported as a `setting_ignored`
  note; the voice itself is chosen by `voice_id`, never by `settings.voice` (`field_not_applicable`).

## Estimating durations (dry runs)

`soniccraft plan` never calls a provider. Each uncached cue's duration is estimated as

`seconds = spoken_chars / chars_per_second / speed`

where `spoken_chars` counts letters, digits, spaces and punctuation (ElevenLabs `[audio tags]` excluded),
and `speed` is `settings.speed` (piper: multiply by `settings.length_scale` instead; macos_say:
`speed = settings.rate / 175` when `rate` is set). Default chars/s per provider are in the table above. A
provider instance may set `chars_per_second`, or `words_per_minute` (then `seconds = words / (wpm / 60) /
speed`). Cached cues use their exact cached length (`duration_source: cache`); dry runs render the mock voice
in-process, so mock cues are exact (`duration_source: exact`). Expect estimates within about ±15 %; the
overlap policy and ducking are recomputed exactly at render time.

Rule of thumb: natural narration is about 150 words per minute ≈ 2.5 words/s ≈ 15 characters/s, so a
60-second voiceover is roughly 150 words or 900 characters.

---

## ElevenLabs (`elevenlabs`)

- **Setup:** `export ELEVENLABS_API_KEY=...` (key scopes needed: text to speech; voices read for
  `soniccraft voices elevenlabs`).
- **Request:** `POST {base_url}/v1/text-to-speech/{voice_id}/with-timestamps?output_format=pcm_24000`
  with the `xi-api-key` header. Default `base_url` `https://api.elevenlabs.io`. Regional bases such as
  `https://api.us.elevenlabs.io` work too, but the key is only sent there when the host is listed:
  `export SONICCRAFT_ALLOWED_HOSTS=api.us.elevenlabs.io`.
- **Audio:** raw `pcm_24000` (24 kHz 16-bit mono) by default. Override per instance with
  `output_format`, e.g. `pcm_44100` (Pro tier or higher) or `pcm_48000`. If the account cannot use the
  configured format (403 `subscription_required` / `feature_not_available`, or 400
  `invalid_output_format`), SonicCraft falls back to `pcm_24000`.
- **Voices:** `voice_id` is required. List yours with `soniccraft voices elevenlabs`. Premade voices used
  in the examples: `JBFqnCBsd6RMkjVDRZzb` (George), `EXAVITQu4vr4xnSDxMaL` (Sarah),
  `21m00Tcm4TlvDq8ikWAM` (Rachel).
- **Models** (`model`; default `eleven_multilingual_v2`) and per-request character limits:

  | Model | Limit | Notes |
  |---|---|---|
  | `eleven_multilingual_v2` | 10,000 | Default. Stable long-form, 29 languages. `language` is not supported by this model. |
  | `eleven_v4` | 10,000 | Flagship (GA 2026-09-28), 90+ languages. Accepts **only** `stability` and `similarity_boost`: `style`, `speed` and `use_speaker_boost` are dropped (info diagnostic). No SSML; audio tags like `[whispers]` work. |
  | `eleven_v3` | 5,000 | Expressive; audio tags, no SSML breaks. |
  | `eleven_flash_v2_5` | 40,000 | Low latency, 32 languages, cheaper per character. |
  | `eleven_flash_v2` | 30,000 | Low latency, English. |
  | `eleven_turbo_v2_5`, `eleven_turbo_v2` | | Deprecated: use the flash equivalents. |

- **Settings** (sent as `voice_settings`): `stability` (0 to 1, default 0.5), `similarity_boost` (0 to 1,
  default 0.75), `style` (default 0), `use_speaker_boost` (bool), `speed` (0.7 to 1.2); plus `seed`
  (integer). Without `seed`, SonicCraft sends a stable seed derived from the voice and text for
  best-effort reproducibility. `language` is sent as `language_code`. Settings read: `stability`,
  `similarity_boost`, `style`, `use_speaker_boost`, `speed`, `voice_settings`, `seed`, `language_code`,
  `apply_text_normalization`, `apply_language_text_normalization`, `pronunciation_dictionary_locators`,
  `previous_text`, `next_text`, `use_pvc_as_ivc`.
- **Word timings:** yes (character alignment grouped into words; `[audio tags]` are dropped). When you use
  audio tags, also set `caption:` to the clean text so tags never reach the subtitles.
- **Concurrency:** default 2. Plan limits (Multilingual v2 / Flash): Free 2/4, Starter 3/6, Creator 5/10,
  Pro 10/20, Scale and Business 15/30. Raise `max_concurrency` only up to your plan's limit.
- SonicCraft never sends ElevenLabs `audio_effects`; mixing happens in SonicCraft.

## OpenAI-compatible (`openai`)

A generic client for any server implementing OpenAI's `/audio/speech` endpoint.

- **Request:** `POST {base_url}/audio/speech` with `Authorization: Bearer $OPENAI_API_KEY` and
  `response_format: pcm` (24 kHz 16-bit mono, headerless). Default `base_url` `https://api.openai.com/v1`.
- **OpenAI's own TTS models (`tts-1`, `tts-1-hd`, `gpt-4o-mini-tts`) retire on 2027-01-06**, with no
  successor on this endpoint. Plan to point `base_url` at a compatible server instead:

  | Server | `base_url` | `model` | Voices |
  |---|---|---|---|
  | Kokoro-FastAPI | `http://localhost:8880/v1` | `kokoro` | `af_heart`, `am_adam`, ... (prefer the native `kokoro` type: it adds word timings) |
  | Speaches | `http://localhost:8000/v1` | `speaches-ai/Kokoro-82M-v1.0-ONNX` | `af_heart`, ... |
  | LocalAI | your LocalAI URL + `/v1` | per your LocalAI config | per your LocalAI config |

  A server that is not api.openai.com gets requests without a key when none is set, so a local server that
  ignores keys needs nothing. If it insists on some key, set the provider's own variable (`export
  OPENAI_API_KEY=none`; it is sent to servers on this machine), or name another variable with `api_key_env`
  and list the host (`SONICCRAFT_ALLOWED_HOSTS=localhost`). A compatible server elsewhere on your network or
  remote needs its host in `SONICCRAFT_ALLOWED_HOSTS` (e.g. `SONICCRAFT_ALLOWED_HOSTS=tts.example.com`). An
  unexpanded `${...}` text counts as unset.
- **Voices** (OpenAI): `alloy` (default), `ash`, `ballad`, `coral`, `echo`, `fable`, `nova`, `onyx`,
  `sage`, `shimmer`, `verse`, `marin`, `cedar` (all 13 on `gpt-4o-mini-tts`; `tts-1`/`tts-1-hd` support
  alloy, ash, coral, echo, fable, onyx, nova, sage, shimmer). OpenAI recommends `marin` or `cedar`.
- **Settings:** `instructions` (tone/accent direction; `gpt-4o-mini-tts` only), `speed` (0.25 to 4.0;
  reliable on `tts-1`/`tts-1-hd`, best-effort on `gpt-4o-mini-tts`), `extra_body` (extra request fields for a
  compatible server). SonicCraft never sends provider-specific extra fields to api.openai.com.
- **Limits:** 4,096 characters per request (`gpt-4o-mini-tts` is also capped at 2,000 input tokens).
- **Word timings:** none; subtitles are estimated from the audio.

## Cartesia (`cartesia`)

- **Setup:** `export CARTESIA_API_KEY=...`.
- **Request:** `POST {base_url}/tts/sse` with `Authorization: Bearer` and `Cartesia-Version: 2026-08-14`,
  raw `pcm_f32le` at 48 kHz, `add_timestamps: true`. Default `base_url` `https://api.cartesia.ai`.
- **Models:** default `sonic-3.6` (an alias tracking the latest 3.6 snapshot). For byte-reproducible
  renders across months, pin the dated snapshot `sonic-3.6-2026-08-27`. Also available: `sonic-3.5`,
  `sonic-3`. `sonic-2*`, `sonic-turbo*` and `sonic-3-2025-10-27` are sunset on 2026-10-20;
  `sonic-latest` is a deprecated alias of the beta `sonic-preview`; avoid both.
- **Voices:** `voice_id` (a voice UUID) is required: `soniccraft voices cartesia`.
- **Language:** `en`, `fr`, `de`, `es`, `pt`, `zh`, `ja`, `hi`, `it`, `ko`, `nl`, `pl`, `ru`, `sv`, `tr` and
  29 more base codes; regional locales such as `en-GB` are accepted.
- **Settings:** `speed` (0.6 to 1.5). Also read: `volume`, `emotion`, `generation_config`, `locale`, `accent`,
  `normalization`, `pronunciation_dict_id`.
- **Word timings:** yes.
- **Limits:** no documented per-request limit; keep each cue to a few sentences. Plan concurrency: Free 2,
  Pro 3, Startup 5, Scale 15 (default `max_concurrency` 2).

## Kokoro via Kokoro-FastAPI (`kokoro`)

Kokoro-82M is a small open-weight TTS model; [Kokoro-FastAPI](https://github.com/remsky/Kokoro-FastAPI)
serves it locally. Free, no API key, good quality, word timings.

- **Setup:**

  ```bash
  docker run -p 8880:8880 ghcr.io/remsky/kokoro-fastapi-cpu:latest     # or kokoro-fastapi-gpu:latest
  curl http://localhost:8880/health                                    # {"status":"healthy"}
  ```

  `base_url` defaults to env `KOKORO_BASE_URL`, else `http://localhost:8880`. An API key is optional
  (`KOKORO_API_KEY`) for servers behind a proxy that wants one; a server elsewhere on the network or remote that gets
  the key must be listed in `SONICCRAFT_ALLOWED_HOSTS`. `soniccraft providers` and `validate` probe the server (a TCP connect
  of at most 300 ms): `available: false` with `no Kokoro-FastAPI server answered at http://localhost:8880`
  (or `provider_unavailable` in validate) means start it first.
- **Request:** `POST {base_url}/dev/captioned_speech` (`stream: false`, `response_format: pcm`, 24 kHz),
  which returns word timestamps; on servers without that route (404) SonicCraft falls back to
  `POST {base_url}/v1/audio/speech` (no timings).
- **Model:** `kokoro`.
- **Voices** (default `af_heart`): names are `<language><gender>_<name>`. Language letters: `a` US
  English, `b` UK English, `e` Spanish, `f` French, `h` Hindi, `i` Italian, `j` Japanese, `p` Brazilian
  Portuguese, `z` Mandarin; gender `f`/`m`. Examples: `af_heart`, `af_bella`, `af_sky`, `af_nicole`,
  `am_adam`, `am_michael`, `bf_emma`, `bm_george`, `ff_siwis`, `jf_alpha`, `zf_xiaoxiao`. Full list:
  `soniccraft voices kokoro`.
- **Settings:** `speed` (0.25 to 4.0). Also read: `lang_code`, `volume_multiplier`, `normalization_options`,
  `extra_body`.
- **Word timings:** yes on the captioned endpoint (punctuation tokens are folded into the previous word).
  Non-English voices may return no timings; those cues are estimated.
- **Limits:** the server's `MAX_INPUT_LENGTH` (default 1,000,000 characters). Default `max_concurrency` 1
  because one local server processes requests one at a time.

## Piper (`piper`)

Fast local neural TTS. The maintained project is **OHF-Voice/piper1-gpl** (the original rhasspy/piper
repository is archived).

- **Setup:**

  ```bash
  pip install piper-tts                     # or `pipx install piper-tts`; installs the `piper` command
  cd my-project                             # the directory of the timeline
  python3 -m piper.download_voices en_US-lessac-medium --data-dir voices
  ```

  `download_voices` writes `en_US-lessac-medium.onnx` and `.onnx.json` into `--data-dir` (without it, into
  the current directory). Then either set `voices_dir: voices` on the provider instance
  (`providers: { piper: { voices_dir: voices } }`), or download straight into the timeline directory.
- **Voice:** `voice_id` is required: a voice name such as `en_US-lessac-medium`, or a model path (a
  `voice_id` that contains `/` or ends in `.onnx`, relative to the timeline directory). A name is looked up as
  `<dir>/<voice_id>.onnx` in these directories, in order: the instance's `voices_dir` (relative to the timeline), env
  `PIPER_VOICES_DIR` (relative to the timeline when relative), the timeline directory itself, and
  `~/.local/share/piper/voices` (piper's default). Each model needs its `.onnx.json` next to it. A model found
  nowhere is `unknown_voice` in `validate`, whose message lists every searched directory and whose hint is the
  exact `download_voices ... --data-dir` command. `soniccraft voices piper` lists the models it finds, or the
  searched directories and the download command. Voice names follow `<lang>_<REGION>-<name>-<quality>`:
  `en_US-lessac-medium`, `en_GB-alan-medium`, `de_DE-thorsten-high`; catalogue:
  huggingface.co/rhasspy/piper-voices.
- **Invocation:** `piper -m <model.onnx> -f <tmp.wav> [-s N] [--length_scale X]` with the text on stdin
  (works with piper1-gpl and the archived binary). Override the argv with the instance's `command` /
  `stdin` templates if your install differs (that runs a custom program: over MCP it needs
  `--allow-command`).
- **Settings:** `speaker` (speaker id of multi-speaker models; also `speaker_id`), `length_scale` (above 1
  is slower: 1.25 is about 20 % slower), `speed`, `noise_scale`, `noise_w` (`noise_w_scale`),
  `sentence_silence`.
- **Audio:** 16 kHz (x_low, low) or 22.05 kHz (medium, high); resampled to the output rate.
- **Word timings:** none (estimated).

## macOS speech (`macos_say`)

The built-in macOS `say` command: free, offline, no setup. macOS only (`provider_unavailable` elsewhere).

- **Invocation:** `say -v <voice> [-r <rate>] -o <tmp>.wav --data-format=LEI16@48000 -f <textfile>`.
- **Voices:** default `Samantha`. List installed voices with `say -v '?'` (quote the `?` in zsh) or
  `soniccraft voices macos_say`; English voices present on most Macs: `Samantha`, `Daniel` (UK), `Karen`
  (AU), `Moira` (IE), `Rishi` (IN), `Tessa` (ZA), `Fred`. Install more in System Settings > Accessibility >
  Spoken Content > System Voice > Manage Voices.
- **Caveat:** `say` itself silently falls back to the default voice when the voice name is unknown (and
  exits successfully). SonicCraft therefore checks `voice_id` against `say -v '?'` already in `validate`
  (`unknown_voice`, with the closest installed names as did-you-mean) instead of rendering the wrong voice.
- **Settings:** `rate` in words per minute (nominal default 175), or `speed`. The real rate is quantized and
  non-linear, so do not rely on it for timing; the estimate uses `speed = rate / 175`.
- **Word timings:** none (estimated).

## Custom command (`command`)

Wrap any local TTS that can write an audio file.

```yaml
providers:
  espeak:
    type: command
    command: ["espeak-ng", "-v{voice_id}", "-w", "{output}", "-f", "{text_file}"]
speakers:
  robot: { provider: espeak, voice_id: en-us }
```

- `command`: argv template (no shell). `stdin`: optional template written to the child's stdin
  (e.g. `"{text}"`).
- Placeholders: `{text}`, `{text_file}` (UTF-8 temp file), `{output}` (where the command must write audio,
  any decodable format), `{voice_id}`, `{model}`, `{language}`.
- **Pass text through `{text_file}` or `stdin: "{text}"`**, never as an argument of its own: a cue whose text
  starts with `-` would be read as an option, so SonicCraft refuses to run the command for it, and
  `validate` warns about such templates (`suspicious_value` at `providers.<name>.command[<i>]`). Attach
  `{voice_id}` to its option (`-v{voice_id}`, `--voice={voice_id}`) for the same reason.
- The program runs in the timeline directory, in its own process group (killed as a whole on timeout or
  cancel), without `ELEVENLABS_API_KEY`, `OPENAI_API_KEY`, `CARTESIA_API_KEY`, `KOKORO_API_KEY`,
  `SONICCRAFT_ALLOWED_HOSTS` or any `api_key_env` of the timeline in its environment. If it needs a key of
  its own, have it read a variable with another name (`api_key_env` does not apply to command providers).
- The MCP server runs command providers only when started with `soniccraft mcp --allow-command` (or
  `SONICCRAFT_ALLOW_COMMAND=1`); otherwise they are `provider_unavailable`.
- `voice_id` is required. A non-zero exit is `synthesis_failed` with the end of the command's stderr.
  `settings` are not passed to the program (every key is a `setting_ignored` note).
- **Word timings:** none (estimated).

## Mock (`mock`)

A deterministic synthetic voice built into SonicCraft: vowel-like tone bursts per word, exact word
timings, identical output for identical input. Use it for tests, CI, layout experiments and demos with
zero setup (`examples/offline-demo/timeline.mock.yaml` in the SonicCraft repository).

- Default voice `mock`; any `voice_id` string works and selects a different, stable pitch. Give every
  speaker its own `voice_id` (`voice_id: alex`, `voice_id: sam`): speakers left on the default sound
  identical (warning `speakers_share_voice`).
- Dry runs render the mock voice in-process exactly as a render does, so plans with mock voices are exact
  (`duration_source: exact`, `estimated: false` for a mock-only timeline).
- Settings read: `wpm` (words per minute), `speed`.

---

## Choosing a provider

- **No keys, any OS:** `mock` (placeholder voice) to prove the timeline, then swap providers.
- **No keys, real voices:** `macos_say` on a Mac; `kokoro` (Docker) or `piper` (pip) anywhere.
- **Best quality / expressive:** `elevenlabs` (`eleven_multilingual_v2` for stable long-form, `eleven_v4`
  for expressive reads); `cartesia` `sonic-3.6` for fast, natural delivery.
- **Subtitles must be word-accurate:** prefer providers with word timings (elevenlabs, cartesia, kokoro,
  mock).
- **Mixing providers in one timeline is fine:** every cue is loudness-normalized to the same reference
  (-20 LUFS by default), so voices from different engines sit at the same level.

## Caching

Each synthesized cue is cached by a content hash (blake3 of a canonical JSON of provider type, API version,
non-default base URL, model, voice, language, text, settings, output format, and for command/piper the
command and stdin templates, the resolved program and a hash of its contents, the files that literal
arguments name (e.g. a `tts.py` script) and the piper model files). Changing any of these re-synthesizes
only the affected cue; reordering or retiming cues costs nothing. Cache location: CLI `--cache-dir` > env
`SONICCRAFT_CACHE_DIR` > `<timeline dir>/.soniccraft/cache`. `soniccraft render --no-cache` ignores cached
audio (fresh results are still written); `soniccraft cache info|clear` inspects or empties the cache. A cache
that cannot be written (read-only, full) does not fail the render: warning `cache_write_failed` says later
renders will synthesize those cues again.
