#!/usr/bin/env node
// Regenerates the dev-harness fixtures: sample.preview.json (a DESIGN.md §10.2 preview payload for the README
// timeline) and sample.mp3 (audio that matches it: tone-burst "speech" per word, a ducked synth pad bed, a
// whoosh). Everything is computed from one deterministic model — fixed seeds, no clock — so re-running it
// produces byte-identical JSON. The waveform, ducking regions and gain envelope are measured from the
// synthesized signal with the algorithms of DESIGN.md §4.5 (sidechain detector, hold, lookahead, dB ramps).
//
//   node make-sample.mjs           writes sample.preview.json and sample.mp3 (needs ffmpeg on PATH)
//   node make-sample.mjs --json    writes only sample.preview.json
//
// Exports `buildSample()` and `writeMp3()` for build-sample.mjs and the tests.

import { spawnSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { formatJson } from './build-sample.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const SR = 24000; // synthesis rate (the report claims a 48 kHz render; ffmpeg resamples the mp3)
const PPS = 50; // waveform / envelope points per second (DESIGN.md §10.2)
const round = (v, dp) => Math.round(v * 10 ** dp) / 10 ** dp || 0; // `|| 0` turns -0 into 0

/** xorshift32 with a fixed seed: deterministic noise. */
function rng(seed) {
  let x = seed >>> 0 || 1;
  return () => {
    x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0;
    return x / 4294967296 * 2 - 1;
  };
}

// --- the README timeline, with the durations a render measured ---------------------------------------------
const TAIL_MS = 2500; // = fade_out_ms of the open-ended bed (DESIGN.md §3.5)
const VOICES = {
  cue_01: { track: 'voice_primary', speaker: 'narrator', provider: 'elevenlabs', voice_id: '21m00Tcm4TlvDq8ikWAM', source: 'cache', ms: 4210, f0: 118,
    text: 'Autonomous software development requires deterministic compilation targets.' },
  cue_02: { track: 'voice_secondary', speaker: 'guest', provider: 'local_kokoro', voice_id: 'am_adam', source: 'exact', ms: 5420, f0: 156,
    text: 'Exactly. SonicCraft treats audio composition the same way Hyperframes treated web-based video.' },
  cue_03: { track: 'voice_primary', speaker: 'narrator', provider: 'elevenlabs', voice_id: '21m00Tcm4TlvDq8ikWAM', source: 'cache', ms: 6120, f0: 118,
    text: 'By separating high-level planning from digital signal processing, systems eliminate runtime hallucinations.' },
};

/** Layout per DESIGN.md §3.4: cue_02 is explicit (after cue_01 + 150 ms); cue_03 is pushed past cue_02. */
function layoutCues() {
  const c1 = { id: 'cue_01', start: 0 };
  c1.end = c1.start + VOICES.cue_01.ms;
  const pause = { start: c1.end, end: c1.end + 350 };
  const c2 = { id: 'cue_02', start: c1.end + 150 };
  c2.end = c2.start + VOICES.cue_02.ms;
  const c3 = { id: 'cue_03', pushedFrom: pause.end, start: c2.end + 350 }; // gap = the 350 ms pause before it
  c3.end = c3.start + VOICES.cue_03.ms;
  return { cues: [c1, c2, c3], pause, contentEnd: c3.end, total: c3.end + TAIL_MS };
}

const syllables = w => Math.max(1, (w.toLowerCase().replace(/[^a-z]/g, '').replace(/e$/, '').match(/[aeiouy]+/g) || []).length);

/** Word timings inside a cue (30 ms trim padding at both ends, punctuation-aware gaps), rounded to 0.1 ms. */
function cueWords(cue) {
  const v = VOICES[cue.id], tokens = v.text.split(/\s+/);
  const gaps = tokens.map((t, i) => (i === tokens.length - 1 ? 0 : /[.!?]$/.test(t) ? 330 : /[,;:]$/.test(t) ? 160 : 45));
  const weights = tokens.map(t => syllables(t) + 0.35);
  const speak = v.ms - 60 - gaps.reduce((a, b) => a + b, 0);
  const total = weights.reduce((a, b) => a + b, 0);
  let t = cue.start + 30;
  return tokens.map((text, i) => {
    const s = t, e = s + speak * weights[i] / total;
    t = e + gaps[i];
    return { text, s, e, syl: syllables(text) };
  });
}

/** Greedy caption segmentation per DESIGN.md §6 (42 chars x 2 lines, 6 s max, sentence-end preference). */
function cueCaptions(cue, words) {
  const MAXC = 42, MAXL = 2;
  const lines = toks => {
    const text = toks.join(' ');
    if (text.length <= MAXC) return [text];
    let best = null;
    for (let i = 1; i < toks.length; i++) {
      const a = toks.slice(0, i).join(' '), b = toks.slice(i).join(' ');
      if (a.length > MAXC || b.length > MAXC || MAXL < 2) continue;
      const score = Math.abs(a.length - b.length) - (/[.,;:!?]$/.test(toks[i - 1]) ? 6 : 0);
      if (!best || score < best.score) best = { score, l: [a, b] };
    }
    return best && best.l;
  };
  const groups = [];
  let cur = [];
  for (const w of words) {
    const next = [...cur, w];
    if (cur.length && (!lines(next.map(x => x.text)) || w.e - cur[0].s > 6000)) { groups.push(cur); cur = [w]; } else cur = next;
    const len = cur.map(x => x.text).join(' ').length;
    if (/[.!?]$/.test(w.text) && len >= MAXC * MAXL / 2) { groups.push(cur); cur = []; }
  }
  if (cur.length) groups.push(cur);
  return groups.map(g => {
    const text = g.map(w => w.text).join(' ');
    return { s: g[0].s, e: g[g.length - 1].e, text, lines: lines(g.map(w => w.text)) || [text] };
  });
}

// --- synthesis ------------------------------------------------------------------------------------------------
const fadeEq = t => Math.sin(Math.max(0, Math.min(1, t)) * Math.PI / 2); // equal_power curve (DESIGN.md §4.3)

/** Vowel-like harmonic bursts, one envelope bump per syllable, mono. */
function synthVoice(buf, words, f0, seed) {
  const noise = rng(seed);
  for (const w of words) {
    const s0 = Math.round(w.s / 1000 * SR), s1 = Math.round(w.e / 1000 * SR), n = s1 - s0;
    const formant = 600 + 500 * Math.abs(noise());
    let phase = 0;
    for (let i = 0; i < n; i++) {
      const u = i / n, syl = (u * w.syl) % 1;
      const env = Math.pow(Math.sin(Math.PI * syl), 0.6) * Math.min(1, i / (0.008 * SR), (n - i) / (0.012 * SR));
      const f = f0 * (1.08 - 0.12 * u + 0.03 * Math.sin(2 * Math.PI * 5.5 * i / SR));
      phase += 2 * Math.PI * f / SR;
      let v = 0;
      for (let k = 1; k <= 10; k++) {
        const fk = k * f, gain = (1 / k) * (0.4 + 1.6 * Math.exp(-(((fk - formant) / 260) ** 2)) + 0.8 * Math.exp(-(((fk - 1500) / 400) ** 2)));
        v += gain * Math.sin(k * phase);
      }
      buf[s0 + i] += 0.16 * env * (v + 0.04 * noise());
    }
  }
}

/** Sidechain ducking per DESIGN.md §4.5 on 1 ms blocks: 10 ms RMS, threshold, hold, lookahead, dB ramps. */
function duckEnvelope(trigger, totalMs, { reduction = 18, attack = 80, release = 350, hold = 250, lookahead = 80, threshold = -40 }) {
  const blk = SR / 1000, nb = Math.ceil(totalMs), energy = new Float64Array(nb);
  for (let b = 0; b < nb; b++) for (let i = b * blk; i < (b + 1) * blk && i < trigger.length; i++) energy[b] += trigger[i] * trigger[i];
  const active = new Uint8Array(nb);
  for (let b = 0; b < nb; b++) {
    let e = 0;
    for (let k = Math.max(0, b - 9); k <= b; k++) e += energy[k];
    const rms = Math.sqrt(e / (10 * blk));
    active[b] = rms > 0 && 20 * Math.log10(rms) > threshold ? 1 : 0;
  }
  // runs → hold (bridge short gaps, extend ends) → lookahead
  let runs = [];
  for (let b = 0; b < nb;) {
    if (!active[b]) { b++; continue; }
    const s = b;
    while (b < nb && active[b]) b++;
    runs.push([s, b]);
  }
  const merged = [];
  for (const r of runs) {
    const last = merged[merged.length - 1];
    if (last && r[0] - last[1] < hold) last[1] = r[1]; else merged.push([...r]);
  }
  runs = merged.map(([s, e]) => [Math.max(0, s - lookahead), Math.min(nb, e + hold)]);
  const regions = [];
  for (const r of runs) {
    const last = regions[regions.length - 1];
    if (last && r[0] <= last[1]) last[1] = Math.max(last[1], r[1]); else regions.push([...r]);
  }
  const gainDb = new Float64Array(nb);
  let g = 0, ri = 0;
  for (let b = 0; b < nb; b++) {
    while (ri < regions.length && regions[ri][1] <= b) ri++;
    const target = ri < regions.length && regions[ri][0] <= b ? -reduction : 0;
    g = target < g ? Math.max(target, g - reduction / attack) : Math.min(target, g + reduction / release);
    gainDb[b] = g;
  }
  return { regions, gainDb };
}

/** The whole fixture: payload object plus the stereo master at SR. */
export function buildSample() {
  const L = layoutCues(), totalMs = L.total, n = Math.round(totalMs / 1000 * SR);
  const words = [], captions = [];
  const speech = { voice_primary: new Float64Array(n), voice_secondary: new Float64Array(n) };
  for (const cue of L.cues) {
    const v = VOICES[cue.id], ws = cueWords(cue);
    synthVoice(speech[v.track], ws, v.f0, cue.id === 'cue_02' ? 7 : 3);
    for (const w of ws) words.push({ text: w.text, start_ms: round(w.s, 1), end_ms: round(w.e, 1), cue_id: cue.id, track_id: v.track, speaker: v.speaker });
    for (const c of cueCaptions(cue, ws)) captions.push({ cue_id: cue.id, track_id: v.track, speaker: v.speaker, start_ms: round(c.s, 1), end_ms: round(c.e, 1), text: c.text, lines: c.lines });
  }
  captions.sort((a, b) => a.start_ms - b.start_ms).forEach((c, i) => { c.index = i + 1; });
  const ordered = captions.map(({ index, cue_id, track_id, speaker, start_ms, end_ms, text, lines }) => ({ index, cue_id, track_id, speaker, start_ms, end_ms, text, lines }));
  words.sort((a, b) => a.start_ms - b.start_ms);

  // Ducking from the pre-fader sum of both voice tracks.
  const trigger = new Float64Array(n);
  for (let i = 0; i < n; i++) trigger[i] = speech.voice_primary[i] + speech.voice_secondary[i];
  const duck = duckEnvelope(trigger, totalMs, {});

  // Bed: Am7 pad with a slow swell and a soft pulse, fades in 1.2 s / out 2.5 s, ducked, fader 0.65.
  const bedL = new Float64Array(n), bedR = new Float64Array(n), chord = [110, 130.81, 164.81, 196, 220];
  for (let i = 0; i < n; i++) {
    const t = i / SR, ms = t * 1000;
    let l = 0, r = 0;
    chord.forEach((f, k) => { l += Math.sin(2 * Math.PI * f * 1.0006 * t + k) / (k + 2); r += Math.sin(2 * Math.PI * f * 0.9994 * t + 2 * k) / (k + 2); });
    const swell = 0.75 + 0.25 * Math.sin(2 * Math.PI * 0.12 * t), pulse = 0.85 + 0.15 * Math.pow(Math.max(0, Math.sin(2 * Math.PI * 2 * t)), 8);
    const fade = Math.min(fadeEq(ms / 1200), fadeEq((totalMs - ms) / TAIL_MS));
    const g = 0.65 * 0.22 * swell * pulse * fade * 10 ** (duck.gainDb[Math.min(duck.gainDb.length - 1, Math.floor(ms))] / 20);
    bedL[i] = g * l; bedR[i] = g * r;
  }
  // Whoosh: swept, filtered noise over 1.35 s (sfx track, fader 0.8).
  const whoosh = new Float64Array(n), noise = rng(11), wn = Math.round(1.35 * SR);
  let lp = 0;
  for (let i = 0; i < wn; i++) {
    const u = i / wn, cutoff = 300 + 3700 * Math.sin(Math.PI * u), a = 1 - Math.exp(-2 * Math.PI * cutoff / SR);
    lp += a * (noise() - lp);
    whoosh[i] = 0.8 * 0.9 * lp * Math.pow(Math.sin(Math.PI * Math.min(1, u * 1.6)), 2) * (1 - u * 0.4);
  }
  // Master: constant-power pans (-0.15 / +0.15), then peak-normalize to -1 dBFS.
  const pan = p => { const th = (p + 1) * Math.PI / 4; return [Math.cos(th), Math.sin(th)]; };
  const [p1l, p1r] = pan(-0.15), [p2l, p2r] = pan(0.15);
  const left = new Float32Array(n), right = new Float32Array(n);
  let peak = 0;
  for (let i = 0; i < n; i++) {
    const sp = speech.voice_primary[i], ss = speech.voice_secondary[i];
    left[i] = bedL[i] + whoosh[i] * Math.SQRT1_2 + sp * p1l + ss * p2l;
    right[i] = bedR[i] + whoosh[i] * Math.SQRT1_2 + sp * p1r + ss * p2r;
    peak = Math.max(peak, Math.abs(left[i]), Math.abs(right[i]));
  }
  const norm = 10 ** (-1 / 20) / peak;
  for (let i = 0; i < n; i++) { left[i] *= norm; right[i] *= norm; }
  const bucket = SR / PPS, points = Math.ceil(n / bucket), wmin = [], wmax = [];
  for (let b = 0; b < points; b++) {
    let lo = 0, hi = 0;
    for (let i = b * bucket; i < Math.min(n, (b + 1) * bucket); i++) { const m = (left[i] + right[i]) / 2; if (m < lo) lo = m; if (m > hi) hi = m; }
    wmin.push(round(lo, 3)); wmax.push(round(hi, 3));
  }
  const envDb = [];
  for (let k = 0; k < points; k++) envDb.push(round(duck.gainDb[Math.min(duck.gainDb.length - 1, Math.round(k * 1000 / PPS))], 2));

  const item = (id, extra) => ({ id, ...extra });
  const cueItem = c => {
    const v = VOICES[c.id];
    return item(c.id, { kind: 'cue', track_id: v.track, start_ms: c.start, end_ms: c.end, duration_ms: c.end - c.start, duration_source: v.source,
      speaker: v.speaker, provider: v.provider, voice_id: v.voice_id, text: v.text, ...(c.pushedFrom != null ? { pushed_from_ms: c.pushedFrom } : {}) });
  };
  const [c1, c2, c3] = L.cues;
  const payload = {
    soniccraft_preview: 1,
    title: 'Tech briefing',
    audio: 'sample.mp3',
    report: {
      output: { path: 'sample.mp3', format: 'mp3', sample_rate: 48000, channels: 2, bitrate_bps: 320000, duration_ms: totalMs, bytes: Math.round(320000 / 8 * totalMs / 1000) + 418 },
      loudness: { integrated_lufs: -16.0, true_peak_dbtp: -1.2, sample_peak_dbfs: -1.5, loudness_range_lu: 4.6, target_lufs: -16.0, true_peak_target_dbtp: -1.0,
        normalized: true, gain_db: 3.8, limiter_max_reduction_db: 1.1, iterations: 2 },
      files: { subtitles: [{ format: 'vtt', path: 'sample.vtt' }, { format: 'srt', path: 'sample.srt' }], preview: 'sample.html' },
      tracks: [
        { id: 'ambient_bed', type: 'music', gain_db: round(20 * Math.log10(0.65), 2), pan: 0, mute: false, ducked_by: ['voice_primary', 'voice_secondary'] },
        { id: 'voice_primary', type: 'speech', gain_db: 0, pan: -0.15, mute: false },
        { id: 'voice_secondary', type: 'speech', gain_db: 0, pan: 0.15, mute: false },
        { id: 'sfx_impact', type: 'sfx', gain_db: round(20 * Math.log10(0.8), 2), pan: 0, mute: false },
      ],
      items: [
        item('ambient_bed', { kind: 'clip', track_id: 'ambient_bed', start_ms: 0, end_ms: totalMs, duration_ms: totalMs, duration_source: 'source', source: 'assets/audio/synthwave_loop.wav', open_ended: true, looped: true }),
        cueItem(c1),
        item('sfx_impact', { kind: 'clip', track_id: 'sfx_impact', start_ms: 0, end_ms: 1350, duration_ms: 1350, duration_source: 'source', source: 'assets/sfx/whoosh.wav' }),
        item('voice_primary.pause1', { kind: 'pause', track_id: 'voice_primary', start_ms: L.pause.start, end_ms: L.pause.end, duration_ms: 350, duration_source: 'fixed' }),
        cueItem(c2),
        cueItem(c3),
      ],
      ducking: [{ track_id: 'ambient_bed', mode: 'sidechain', reduction_db: 18, triggers: ['voice_primary', 'voice_secondary'],
        regions: duck.regions.map(([s, e]) => ({ start_ms: s, end_ms: e })) }],
      synthesis: { cues: 3, cache_hits: 2, synthesized: 1, providers_called: ['local_kokoro'] },
      diagnostics: [
        { severity: 'warning', code: 'missing_credentials', message: 'ELEVENLABS_API_KEY is not set; both `elevenlabs` cues were served from the TTS cache.',
          path: 'tracks[1].sequence[0].provider', line: 26, column: 19,
          hint: 'Set ELEVENLABS_API_KEY before changing the text or voice of `cue_01` or `cue_03`; until then the cached audio is reused.', related: ['cue_01', 'cue_03'] },
        { severity: 'info', code: 'cue_pushed', message: `\`cue_03\` was pushed from ${(c3.pushedFrom / 1000).toFixed(3)} s to ${(c3.start / 1000).toFixed(3)} s (by ${((c3.start - c3.pushedFrom) / 1000).toFixed(3)} s) so it does not overlap \`cue_02\`.`,
          path: 'tracks[1].sequence[2]', line: 31, column: 9,
          hint: 'To place it explicitly, set `start_relative: {after_cue: cue_02, offset_ms: 350}` on `cue_03`.', related: ['cue_03', 'cue_02'] },
        { severity: 'info', code: 'alignment_estimated', message: '`local_kokoro` returned no word timestamps; word timings of `cue_02` were estimated from the audio.',
          path: 'tracks[2].sequence[0]', line: 42, column: 9, related: ['cue_02'] },
      ],
    },
    waveform: { points_per_second: PPS, min: wmin, max: wmax },
    envelopes: [{ track_id: 'ambient_bed', points_per_second: PPS, gain_db: envDb }],
    captions: ordered,
    words,
  };
  return { payload, left, right, sampleRate: SR };
}

/** Encode a stereo float buffer to MP3 with ffmpeg (raw f32le on stdin). Throws with ffmpeg's stderr on failure. */
export function writeMp3(path, left, right, sampleRate) {
  const inter = new Float32Array(left.length * 2);
  for (let i = 0; i < left.length; i++) { inter[2 * i] = left[i]; inter[2 * i + 1] = right[i]; }
  const r = spawnSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-y', '-f', 'f32le', '-ar', String(sampleRate), '-ac', '2', '-i', 'pipe:0',
    '-ar', '48000', '-c:a', 'libmp3lame', '-b:a', '320k', '-map_metadata', '-1', path], { input: Buffer.from(inter.buffer) });
  if (r.error) throw new Error(`could not run ffmpeg (${r.error.message}); install ffmpeg or use --json`);
  if (r.status !== 0) throw new Error(`ffmpeg failed: ${String(r.stderr).trim()}`);
}

if (import.meta.url === pathToFileURL(process.argv[1] || '').href) {
  const { payload, left, right, sampleRate } = buildSample();
  writeFileSync(join(HERE, 'sample.preview.json'), formatJson(payload) + '\n');
  console.log(`wrote sample.preview.json (${payload.report.items.length} items, ${payload.words.length} words, ${payload.captions.length} captions, ${payload.waveform.min.length} waveform points)`);
  if (!process.argv.includes('--json')) {
    writeMp3(join(HERE, 'sample.mp3'), left, right, sampleRate);
    console.log('wrote sample.mp3');
  }
}
