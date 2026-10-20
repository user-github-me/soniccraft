# SonicCraft mixing guide

How SonicCraft turns tracks into a finished master, and how to set ducking, loudness, panning, fades and
music beds so the result sounds produced. Field details are in [dsl-reference.md](dsl-reference.md).

## Signal flow

Everything runs natively at the output sample rate in 32-bit float:

1. **Each speech cue:** synthesize (or load from cache) → decode → mono → resample → trim silence
   (default: frames above -50 dBFS, keeping 30 ms on each side) → normalize to the track target (default
   -20 LUFS) → per-cue gain (`volume`, `gain_db`, speaker `gain_db`).
2. **Each clip:** decode once per source file (sources with more than two channels, such as 5.1 or 7.1,
   are mixed down to stereo with ITU-R BS.775, LFE dropped: info `channels_downmixed`; sample rates from 1
   to 768 kHz) → normalize the source (music only, by default) → play `length` frames from `trim_start_ms`,
   looping if `loop: true`, with fades relative to the clip's own start and end.
3. **Each track stem:** sum of its placed items → pan → track gain (`volume × 10^(gain_db/20)`, 0 when
   muted) → ducking.
4. **Master:** stems summed in declaration order → 10 ms de-click fade at the very end if needed →
   loudness normalization to the target → true-peak limiter at the ceiling → encode.

Export `output.export_stems: true` (or `--stems`) to get each track after ducking and track gain, before
the master, as 32-bit float WAV for a DAW.

## Levels: source normalization first

Speech and music tracks are normalized at the source (`normalize: true` by default): every cue, and every
distinct music file, is gained so its integrated loudness is `mix.reference_lufs` (-20 LUFS). This is what
makes an ElevenLabs narrator, a Kokoro guest and a macOS voice sit at the same level, and what makes
`volume` meaningful: after normalization, `volume: 0.5` really is 6 dB below the voice.

- sfx tracks are **not** normalized by default: a whoosh should keep its own level. Set the level with
  `volume` / `gain_db`.
- Override per track: `normalize: { target_lufs: -24 }` (quieter source level) or `normalize: false`
  (use the file's own level, e.g. a pre-mixed bed whose loudness you set with `volume`).
- Gain is capped at +24 dB; very short or near-silent sources are left alone (info `normalization_skipped`).

Typical relative levels after normalization:

| Element | `volume` (or `gain_db`) | Comment |
|---|---|---|
| Main voice | 1.0 | Reference. |
| Second voice | 1.0 | Same as the main voice; balance with speaker `gain_db` (±1 to 2 dB) if one reads hotter. |
| Music bed (with ducking) | 0.6 to 0.9 | Full level in gaps, ducked under speech. |
| Music bed (no ducking) | 0.15 to 0.3 | About -12 to -16 dB under the voice. Prefer ducking. |
| Whoosh / transition | 0.5 to 0.9 | Not normalized: depends on the file. |
| Impacts, chimes | 0.5 to 0.8 | Keep them below the first word they introduce. |

## Ducking

Ducking lowers a track while trigger tracks are active. Put it on the bed and point it at the voices:

```yaml
- id: bed
  type: music
  source: assets/bed.wav
  loop: true
  volume: 0.8
  fade_in_ms: 1500
  fade_out_ms: 3000
  ducking:
    trigger_tracks: [narration, guest]
    reduction_db: -15     # default -15; sign ignored
    attack_ms: 80         # default 80: reach full depth in 80 ms
    release_ms: 400       # default 400: recover in 400 ms
    hold_ms: 250          # default 250: bridge short gaps
    # lookahead_ms defaults to attack_ms: fully ducked when the first word starts
    # threshold_db: -40   # sidechain detection threshold, dBFS RMS
    # mode: sidechain     # or `cue` (timeline-driven)
```

How it works: the detector looks at the trigger tracks' **pre-fader** audio (after per-cue normalization
and cue gain, before track volume, pan and mute) in 1 ms blocks with a 10 ms RMS window. Blocks above
`threshold_db` are active; gaps shorter than `hold_ms` are filled and each active run is held `hold_ms`
longer; each run starts `lookahead_ms` early; the gain then moves linearly in dB toward
`-|reduction_db|` (slope set by `attack_ms`) or back to 0 dB (slope set by `release_ms`).

Recipes:

| Use | `reduction_db` | `attack_ms` | `release_ms` | `hold_ms` |
|---|---|---|---|---|
| Podcast / interview bed | -18 to -20 | 80 | 500 to 700 | 300 to 500 |
| Explainer / voiceover bed | -12 to -15 | 60 to 80 | 400 to 500 | longer than your pauses |
| Trailer / ad with energetic music | -8 to -10 | 40 to 60 | 250 to 350 | 200 |
| Ducking sfx under dialog | -6 to -10 | 20 to 40 | 200 | 150 |

- **Pumping** (the bed bobbing up between sentences) means `hold_ms` is shorter than the pauses between
  lines. Set `hold_ms` a little longer than your typical `pause` (pauses + about 60 ms of trimmed padding).
- **Breaths or low noise triggering the duck:** raise `threshold_db` (e.g. -35).
- **`mode: cue`** ducks exactly during the trigger items' intervals (ignores the audio): predictable and
  identical between plan and render. Dry runs always compute ducking regions this way.
- Muted trigger tracks are excluded from sidechain detection (info `muted_trigger`).
- Ducking works on any track type, and several tracks may duck from the same triggers.

## Loudness and true peak

The master is normalized to an integrated loudness (ITU-R BS.1770 / EBU R128, gated) and limited to a
true-peak ceiling. Choose by destination:

| `loudness_preset` | Target | Ceiling | Use for |
|---|---|---|---|
| `podcast` (default) | -16 LUFS | -1.0 dBTP | Podcasts, voice apps, most web audio |
| `streaming` | -14 LUFS | -1.0 dBTP | YouTube, Spotify, social video |
| `broadcast` | -23 LUFS | -1.0 dBTP | EBU R128 broadcast deliverables |
| `atsc` | -24 LUFS | -2.0 dBTP | US broadcast (ATSC A/85) |

- `target_lufs` / `true_peak_db` override the preset values (for example `target_lufs: -19` for a mono
  podcast spec).
- The mastering loop measures, applies gain, runs the limiter, re-measures and corrects until within
  0.2 LU (at most 4 passes; the pass closest to the target is kept). The limiter is a linked, 4×
  oversampled true-peak limiter with 5 ms lookahead and 80 ms release, verified by an independent
  true-peak measurement.
- `loudness_target_unreachable` means reaching the target would need more than 6 dB of average limiting.
  The master is then the loudest the mix allows within that limit, and the message gives the level reached:
  set `output.target_lufs` to that value (or lower), reduce dense music and loud transients under speech, or
  raise the ceiling slightly.
- `loudness_target_missed` means the loop ended outside 0.2 LU of a reachable target (the loudness jumps
  where quiet passages cross the BS.1770 gate): accept the level in the message (`target_lufs` set to it), or
  even out the mix (raise quiet passages, lower loud transients).
- `output.normalize: false` keeps the mix level as is (the limiter still guards the ceiling).
- The render report shows `integrated_lufs`, `true_peak_dbtp`, `sample_peak_dbfs` and `loudness_range_lu`
  **of the delivered file**: mp3, ogg, opus and m4a are decoded and measured after encoding
  (`loudness.pre_encode` has the master's values), plus the applied `gain_db` and `limiter_max_reduction_db`.
  Tell the user these numbers. When a codec pushes the peaks over the ceiling, the master is limited again;
  when it moves the loudness more than 0.2 LU off the target, the difference goes back into the master; either
  way the file is re-encoded (at most 3 passes). `true_peak_exceeded` means it still overshoots, and
  `loudness_target_missed` that it still misses the target: lower `true_peak_db` to -2 for lossy delivery,
  raise the bitrate, or deliver wav/flac.

## Panning

- Mono sources (all speech, mono sfx) use constant-power panning: center is -3 dB per channel, which keeps
  BS.1770 loudness constant across the field. Stereo sources use balance (`pan < 0` turns the right side
  down, `pan > 0` the left).
- Keep the main narrator centered (or within ±0.15). Separate two-person dialogue gently: ±0.15 to ±0.3.
  Hard panning voices sounds unnatural on headphones.
- `output.channels: 1` downmixes stereo sources `(L + R) / 2` and ignores pan.

## Fades and crossfades

| `fade_curve` | Shape (fade-in gain at t ∈ [0, 1]) | Use |
|---|---|---|
| `equal_power` (default) | `sin(t·π/2)` | Natural-sounding fades and crossfades |
| `linear` | `t` | Short anti-click fades |
| `exponential` | 60 dB exponential ramp | Long, smooth fade-outs of music |

- Beds: `fade_in_ms` 800 to 2000, `fade_out_ms` 2000 to 4000.
- SFX: usually no fade (the file's own envelope), or 5 to 20 ms against clicks when trimming into a file.
- If `fade_in_ms + fade_out_ms` exceeds the clip length, both are scaled down (info `fades_scaled`).
- Consecutive clips in a `clips` list can overlap with `crossfade_ms` (equal-power on both sides); the
  next clip then starts `crossfade_ms` before the previous one ends. Only a clip that follows the previous
  one sequentially crossfades: on a clip with its own `start_at_ms`/`start_relative`, `crossfade_ms` is
  ignored (warning `field_not_applicable`); fade it in with `fade_in_ms` instead.
- `loop_crossfade_ms` smooths the seam of sources that do not loop cleanly; seamless loops need 0.

## Music beds

**Open-ended bed** (the usual case): `loop: true` and no length. The clip runs until the end of the
timeline; the timeline ends `tail_ms` after the last finite item, and `tail_ms` defaults to the bed's
`fade_out_ms`, so the bed fades out right after the last word. Without a fade-out you get info
`no_fade_out_on_bed` (and a hard stop).

```yaml
- id: bed
  type: music
  source: assets/bed.wav
  loop: true
  fade_in_ms: 1200
  fade_out_ms: 2500
```

**Bounded bed:** give it an end. `end_relative: { after_cue: last_line, offset_ms: 3000 }` ends 3 s after the
last line (the fade-out sits inside that time). Bounded clips count toward the content end and can be
anchored to.

**Intro before the voice:** start the speech track later (`start_at_ms: 2000`), or anchor the first cue
to the bed: `start_relative: { with_cue: bed, offset_ms: 2000 }`.

The two combine: a bed bounded by the last line (`end_relative: {after_cue: last_line, offset_ms: 3000}`)
can lead into the first line (`start_relative: {with_cue: bed, offset_ms: 2000}`); a clip's start and end are
scheduled separately, so this is not a dependency cycle.

**Intro and outro beds:** two clips on one music track, each with explicit start and end (see
`templates/two-person-podcast.yaml`).

Other tools: `trim_start_ms` skips into a file (e.g. past a slow intro); `output.duration_ms` forces an
exact total length (content beyond it is cut, warning `content_truncated`).

## Sound effects placement

Anchor effects to the speech they belong to, never to guessed absolute times:

- Land a whoosh on a cue: `start_relative: { before_cue: cue_02 }` (the whoosh ends when the cue starts).
- Hit with the first word: `start_relative: { with_cue: cue_01, offset_ms: -400 }`.
- Sign-off chime: `start_relative: { after_cue: outro, offset_ms: 400 }`.
- Anchoring to a whole track (`after_cue: host`) uses the end of its last *declared* item; if an item with
  its own start ends later, you get `track_anchor_out_of_order`: anchor to that cue instead.

## Output formats

- `wav` (16/24-bit integer or 32-bit float), `mp3` (CBR, default 192 kbps) and `flac` (16/24-bit) are
  encoded natively. `ogg` (Vorbis), `opus` (always 48 kHz) and `m4a` (AAC) need ffmpeg on PATH. Format names
  are exact lowercase (`format: mp3`, not `MP3`).
- The `output.format` of the timeline beats the extension of `-o`: to render a `format: wav` timeline to
  MP3, use `-o out.mp3 --format mp3` (MCP: `output_path` plus `format: "mp3"`).
- 16-bit output gets fixed-seed TPDF dither (`dither: true`); 24-bit is rounded.
- For further editing or video: `wav` 24-bit at 48 kHz. For distribution: `mp3` 192 to 320 kbps.
