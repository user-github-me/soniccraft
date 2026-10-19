#!/usr/bin/env node
// Zero-dependency tests for the web preview: template contract, inline-script syntax, pure data functions,
// the engine-equivalent payload embedding and the sample fixture. Run: node --test  (or: node test.mjs)

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
import { embedPayload, escapeForScript, formatJson, TOKEN } from './build-sample.mjs';
import { buildSample } from './make-sample.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const template = readFileSync(join(HERE, 'preview.html'), 'utf8');
const sampleText = readFileSync(join(HERE, 'sample.preview.json'), 'utf8');
const sample = JSON.parse(sampleText);
const MINUS = '\u2212';
const DATA_OPEN = '<script id="soniccraft-data" type="application/json">';

/** The main inline script exactly as an HTML parser delimits it: from `<script>` to the FIRST `</script>`. */
function mainScript(html) {
  const open = html.indexOf('<script>\n');
  assert.ok(open > 0, 'main <script> not found');
  const start = open + '<script>\n'.length;
  return html.slice(start, html.indexOf('</script>', start));
}
const script = mainScript(template);
new vm.Script(script, { filename: 'preview.html#inline' }).runInThisContext(); // no `document`: exports the API only
const P = globalThis.SonicCraftPreview;

// ---------------------------------------------------------------------------------------------- template contract
test('placeholder occurs exactly once, as the whole content of the data script', () => {
  assert.equal(template.split(TOKEN).length, 2);
  assert.ok(template.includes(`${DATA_OPEN}${TOKEN}</script>`));
  assert.equal(P.TOKEN, TOKEN);
});

test('a guard element follows the data script (see README: tokenizer double-escape)', () => {
  assert.ok(template.includes(`${TOKEN}</script>\n<div hidden>--></script></div>\n`));
});

test('template is self-contained: no network references', () => {
  const urls = (template.match(/\b(?:https?|wss?):\/\/[^\s"'`)]+/g) || []).filter(u => u !== 'http://www.w3.org/2000/svg');
  assert.deepEqual(urls, []);
  const css = template.slice(template.indexOf('<style>'), template.indexOf('</style>'));
  const forbidden = [[template, /<link\b|<iframe|<img\b|<object\b|<embed\b/i], [template, /<[a-z][^<>]*\s(?:src|srcset|poster)\s*=/i], [css, /@import|url\(/i],
    [script, /\bfetch\(|XMLHttpRequest|\bimport\(|\beval\(|new Function|sendBeacon|WebSocket|EventSource|Worker\(/]];
  for (const [text, re] of forbidden) assert.ok(!re.test(text), `forbidden pattern ${re}: ${(text.match(re) || [])[0]}`);
});

test('template stays within the 60 KiB budget', () => {
  const bytes = Buffer.byteLength(template);
  assert.ok(bytes <= 60 * 1024, `preview.html is ${bytes} bytes`);
});

test('inline script passes `node --check` and cannot end or escape its own element', () => {
  assert.doesNotMatch(script, /<\/script|<!--|<script/i);
  const dir = mkdtempSync(join(tmpdir(), 'scp-'));
  try {
    const file = join(dir, 'inline.js');
    writeFileSync(file, script);
    const r = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
    assert.equal(r.status, 0, r.stderr);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('payload data never reaches innerHTML (only the constant icon markup does)', () => {
  const uses = script.match(/innerHTML\s*=[^;]*/g) || [];
  assert.deepEqual(uses, ['innerHTML = ICON[name]']);
  assert.doesNotMatch(script, /insertAdjacentHTML|outerHTML|document\.write/);
});

// ---------------------------------------------------------------------------------------------- formatting
test('fmtTime', () => {
  assert.equal(P.fmtTime(0), '0:00.00');
  assert.equal(P.fmtTime(6.2), '0:06.20');
  assert.equal(P.fmtTime(59.996), '1:00.00');
  assert.equal(P.fmtTime(18.75, 0), '0:19');
  assert.equal(P.fmtTime(4.21, 3), '0:04.210');
  assert.equal(P.fmtTime(3723.5, 1), '1:02:03.5');
  assert.equal(P.fmtTime(-1.5, 1), `${MINUS}0:01.5`);
  assert.equal(P.fmtTime(-0.001, 2), '0:00.00');
  assert.equal(P.fmtTime(NaN), '0:00.00');
});

test('fmtNum / fmtSigned / fmtLen / fmtMs', () => {
  assert.equal(P.fmtNum(-16), `${MINUS}16.0`);
  assert.equal(P.fmtNum(-0.04), '0.0');
  assert.equal(P.fmtNum(4.56, 2), '4.56');
  assert.equal(P.fmtNum(null), '—');
  assert.equal(P.fmtSigned(3.8), '+3.8');
  assert.equal(P.fmtSigned(-3.74, 1), `${MINUS}3.7`);
  assert.equal(P.fmtSigned(0.01, 1), '0.0');
  assert.equal(P.fmtLen(4210), '4.210 s');
  assert.equal(P.fmtLen(65300), '1:05.3');
  assert.equal(P.fmtLen(undefined), '—');
  assert.equal(P.fmtMs(350), '350 ms');
  assert.equal(P.fmtMs(1500), '1.500 s');
});

test('tickStep picks the smallest nice step at least minPx wide', () => {
  assert.deepEqual(P.tickStep(64), { major: 2, div: 4, dp: 0 }); // 18.75 s over 1200 px
  assert.deepEqual(P.tickStep(4000), { major: 0.02, div: 4, dp: 2 });
  assert.deepEqual(P.tickStep(500), { major: 0.2, div: 4, dp: 1 });
  assert.deepEqual(P.tickStep(0.3), { major: 300, div: 5, dp: 0 }); // 1 h over ~1100 px
  assert.equal(P.tickStep(1e-9).major, 7200); // clamps at the largest step
  for (let pps = 0.01; pps < 1e5; pps *= 1.37) {
    const { major } = P.tickStep(pps);
    assert.ok(major * pps >= 72 || major === 7200, `pps=${pps}`);
  }
});

test('classify recognizes every accepted JSON shape', () => {
  assert.equal(P.classify(sample), 'payload');
  assert.equal(P.classify({ report: {} }), 'payload');
  assert.equal(P.classify(sample.report), 'report');
  assert.equal(P.classify({ valid: true, items: [], diagram: '', duration_ms: 1 }), 'plan');
  assert.equal(P.classify({ version: '1.0', captions: [], words: [] }), 'words');
  for (const junk of [null, 1, 'x', [], [{ id: 'w0', text: 'a', start: 0, end: 1 }], { hello: 1 }]) assert.equal(P.classify(junk), null);
});

test('safeHref keeps relative paths (encoded) and safe schemes only', () => {
  assert.equal(P.safeHref('briefing.mp3'), 'briefing.mp3');
  assert.equal(P.safeHref('out dir/my #1?.mp3'), 'out%20dir/my%20%231%3F.mp3');
  assert.equal(P.safeHref('../a/b.vtt'), '../a/b.vtt');
  assert.equal(P.safeHref('https://example.com/a.mp3'), 'https://example.com/a.mp3');
  assert.equal(P.safeHref('blob:null/123'), 'blob:null/123');
  for (const bad of ['javascript:alert(1)', ' JaVaScRiPt:x', 'data:text/html,x', 'vbscript:x', 'C:\\out\\a.mp3', '', '  ', null, {}]) assert.equal(P.safeHref(bad), null, String(bad));
  assert.equal(P.safeHref(7), '7'); // a numeric file name is still a relative path
});

// ---------------------------------------------------------------------------------------------- normalize
test('normalize: the sample payload', () => {
  const m = P.normalize(sample, { embedded: true });
  assert.equal(m.kind, 'payload');
  assert.equal(m.title, 'Tech briefing');
  assert.equal(m.audio, 'sample.mp3');
  assert.equal(m.dur, 18750);
  assert.deepEqual(m.tracks.map(t => [t.id, t.cls]), [['ambient_bed', 'music'], ['voice_primary', 'speech'], ['voice_secondary', 'speech'], ['sfx_impact', 'sfx']]);
  assert.deepEqual(m.items.map(i => i.id), ['ambient_bed', 'cue_01', 'sfx_impact', 'voice_primary.pause1', 'cue_02', 'cue_03']);
  const c3 = m.ids.get('cue_03');
  assert.deepEqual([c3.s, c3.e, c3.pushed, c3.speaker, c3.src], [10130, 16250, 4560, 'narrator', 'cache']);
  assert.ok(m.ids.get('ambient_bed').open && m.ids.get('ambient_bed').looped);
  assert.equal(m.byTrack.get('voice_primary').length, 3);
  assert.equal(m.duck.get('ambient_bed').depth, 18);
  assert.equal(m.env.get('ambient_bed').db.length, sample.waveform.min.length);
  assert.equal(m.wave.n, 938);
  assert.equal(m.levels[m.levels.length - 1].n, 1);
  assert.equal(m.diags.length, 3);
  assert.deepEqual(m.diags.map(d => d.sev), ['warning', 'info', 'info']);
  assert.equal(m.caps.length, 5);
  for (const c of m.caps) { // every caption token is linked to a word of the same cue, in order
    const toks = c.rows.flat();
    assert.equal(toks.length, c.lines.join(' ').split(/\s+/).length);
    assert.ok(toks.every(t => t.w && t.w.cue === c.cue && t.w.text === t.text), c.text);
  }
});

test('normalize: a bare render report (manifest) falls back to cue text captions', () => {
  const m = P.normalize(sample.report);
  assert.equal(m.kind, 'report');
  assert.equal(m.title, 'sample'); // from the output file stem
  assert.equal(m.audio, ''); // relative paths are not resolvable for opened files
  assert.deepEqual(m.caps.map(c => c.cue), ['cue_01', 'cue_02', 'cue_03']);
  assert.equal(m.caps[1].text, sample.report.items[4].text);
  assert.ok(m.caps.every(c => c.rows.flat().every(t => t.w === null)));
});

test('normalize: plan reports are flagged as estimates', () => {
  const plan = { valid: true, diagnostics: [], duration_ms: 5000, estimated: true, sample_rate: 48000, tracks: [], items: [{ id: 'a', kind: 'cue', track_id: 'v', start_ms: 0, end_ms: 900, duration_ms: 900, duration_source: 'estimate', text: 'Hi.' }], ducking: [], synthesis: { cues: 1, cached: 0, characters_to_synthesize: 3, by_provider: {} }, diagram: '' };
  const m = P.normalize(plan);
  assert.equal(m.kind, 'plan');
  assert.equal(m.estimated, true);
  assert.equal(m.dur, 5000);
  assert.deepEqual(m.tracks.map(t => [t.id, t.type]), [['v', 'speech']]);
});

test('normalize: words.json merged onto a report links words by cue id', () => {
  const m = P.normalize({ report: sample.report, captions: sample.captions, words: sample.words });
  assert.equal(m.words.length, sample.words.length);
  assert.ok(m.caps.every(c => c.words.length > 0));
});

test('normalize: robust to missing and malformed fields', () => {
  const inputs = [
    undefined, null, 0, 'text', [], {}, { soniccraft_preview: 1 }, { report: null }, { report: { items: null, tracks: 'x' } },
    { report: { items: [null, 1, 'x', {}, { kind: 'cue', start_ms: '100', end_ms: 'abc', text: 5 }, { start_ms: 900, end_ms: 100, source: 's.wav' }] } },
    { report: { output: { duration_ms: -5 }, loudness: 'loud', ducking: [{ track_id: 'x', regions: [{ start_ms: 2, end_ms: 1 }, 'r', null] }, null], diagnostics: [null, 1, { severity: 'fatal' }] } },
    { waveform: { min: 'a', max: [1] }, envelopes: [{ track_id: 'q', points_per_second: 0, gain_db: [0] }, { track_id: 'e', gain_db: [0, -6, 'x', 3] }] },
    { captions: [{}, { start_ms: 0, end_ms: 50, text: 'hi' }, { start_ms: 5, end_ms: 1, text: 'bad' }], words: [null, { text: 'hi', start_ms: 0 }, { text: '', start_ms: 1 }] },
    { report: { items: [{ id: 'c', kind: 'cue', track_id: 't', start_ms: 0, end_ms: 1000, text: 'one two' }] }, words: [{ text: 'one', start_ms: 0, end_ms: 400 }, { text: 'two', start_ms: 500, end_ms: 900 }] },
  ];
  for (const input of inputs) {
    const m = P.normalize(input);
    const label = JSON.stringify(input);
    assert.ok(m.dur > 0 && Number.isFinite(m.dur), label);
    assert.ok(m.items.every((it, i) => it.i === i && it.s <= it.e && it.id && it.track), label);
    assert.equal(new Set(m.tracks.map(t => t.id)).size, m.tracks.length, label);
    assert.ok(m.tracks.every(t => ['speech', 'music', 'sfx', 'other'].includes(t.cls)), label);
    assert.ok(m.caps.every((c, i) => c.rows && (i === 0 || m.caps[i - 1].s <= c.s)), label);
    assert.ok(m.diags.every(d => ['error', 'warning', 'info'].includes(d.sev)), label);
    for (const d of m.duck.values()) assert.ok(d.regions.every(([a, b]) => b > a), label);
    for (const e of m.env.values()) assert.ok(e.db.every(v => v <= 0 && Number.isFinite(v)), label);
  }
  const words = P.normalize(inputs[inputs.length - 1]);
  assert.deepEqual(words.caps[0].rows.flat().map(t => t.w && t.w.text), ['one', 'two']); // linked by time without cue ids
});

test('activeCaps returns all captions covering t, ascending', () => {
  const caps = [{ s: 0, e: 1000 }, { s: 500, e: 3000 }, { s: 1000, e: 2000 }, { s: 4000, e: 5000 }];
  assert.deepEqual(P.activeCaps(caps, 2500, -1), []);
  assert.deepEqual(P.activeCaps(caps, 2500, 0), [0]);
  assert.deepEqual(P.activeCaps(caps, 2500, 999.9), [0, 1]);
  assert.deepEqual(P.activeCaps(caps, 2500, 1000), [1, 2]);
  assert.deepEqual(P.activeCaps(caps, 2500, 3500), []);
  assert.deepEqual(P.activeCaps(caps, 2500, 4999), [3]);
  assert.deepEqual(P.activeCaps([], 0, 10), []);
});

// ---------------------------------------------------------------------------------------------- waveform & envelope
function seeded(seed) { let x = seed; return () => ((x = (x * 1103515245 + 12345) % 2147483648) / 2147483648); }

test('pyramid levels are the exact min/max of aligned blocks', () => {
  const rnd = seeded(7), n = 1001, lo = new Float32Array(n), hi = new Float32Array(n);
  for (let i = 0; i < n; i++) { lo[i] = -rnd(); hi[i] = rnd(); }
  const levels = P.pyramid({ pps: 50, n, lo, hi });
  assert.deepEqual(levels.map(l => l.n), [1001, 501, 251, 126, 63, 32, 16, 8, 4, 2, 1]);
  levels.forEach((L, k) => {
    assert.equal(L.pps, 50 / 2 ** k);
    for (let i = 0; i < L.n; i++) {
      const block = Array.from({ length: 2 ** k }, (_, j) => i * 2 ** k + j).filter(j => j < n);
      assert.equal(L.lo[i], Math.min(...block.map(j => lo[j])));
      assert.equal(L.hi[i], Math.max(...block.map(j => hi[j])));
    }
  });
});

test('waveBars stay within one pyramid block of their exact span', () => {
  const rnd = seeded(42), n = 937, lo = new Float32Array(n), hi = new Float32Array(n);
  for (let i = 0; i < n; i++) { lo[i] = -rnd(); hi[i] = rnd(); }
  const levels = P.pyramid({ pps: 50, n, lo, hi });
  const range = (a, b, arr, f) => { let v = f === Math.min ? 1 : -1; for (let i = Math.max(0, a); i < Math.min(n, b); i++) v = f(v, arr[i]); return v; };
  for (const pxPerSec of [5, 20, 64, 130]) {
    const viewW = 1000, viewX = pxPerSec * 3, bars = P.waveBars(levels, pxPerSec, viewX, viewW);
    const B = 2 ** Math.floor(Math.log2(3 * 50 / pxPerSec)); // points per block at the level the bars use
    assert.ok(bars.length > 0 && bars.length % 4 === 0);
    for (let k = 0; k < bars.length; k += 4) {
      const x = bars[k] + viewX, a0 = Math.ceil(x / pxPerSec * 50), a1 = Math.floor((x + 3) / pxPerSec * 50);
      assert.ok(bars[k + 2] >= range(a0 - B, a1 + B, lo, Math.min) - 1e-6 && bars[k + 3] <= range(a0 - B, a1 + B, hi, Math.max) + 1e-6, `pps=${pxPerSec} bar ${k / 4} too wide`);
      if (a1 - B > a0 + B) assert.ok(bars[k + 2] <= range(a0 + B, a1 - B, lo, Math.min) + 1e-6 && bars[k + 3] >= range(a0 + B, a1 - B, hi, Math.max) - 1e-6, `pps=${pxPerSec} bar ${k / 4} misses points`);
      assert.ok(bars[k] >= -3 && bars[k] <= viewW && bars[k + 1] === 2, 'bar inside the view, 2 px wide');
    }
  }
  const zoomed = P.waveBars(levels, 600, 0, 120); // 12 px per point: one bar per point
  assert.equal(zoomed.length / 4, 10);
  assert.deepEqual([zoomed[0], zoomed[2], zoomed[3]], [0, lo[0], hi[0]]);
  assert.ok(zoomed[1] >= 8 && zoomed[1] <= 11);
  assert.deepEqual(P.waveBars(null, 50, 0, 100), []);
});

test('simplifyEnv keeps only slope changes and reconstructs the curve', () => {
  const db = [0, 0, 0, -4.5, -9, -13.5, -18, -18, -18, -16, -14, -12, -12, 0];
  const pts = P.simplifyEnv(db);
  assert.deepEqual(pts.map(p => p[0]), [0, 2, 6, 8, 11, 12, 13]);
  for (let i = 0; i < db.length; i++) { // linear interpolation between kept points reproduces every sample
    const k = pts.findIndex(p => p[0] >= i);
    const [i1, v1] = pts[k], [i0, v0] = pts[Math.max(0, k - 1)];
    const v = i1 === i0 ? v1 : v0 + (v1 - v0) * (i - i0) / (i1 - i0);
    assert.ok(Math.abs(v - db[i]) < 1e-9, `i=${i}`);
  }
  assert.deepEqual(P.simplifyEnv([]), []);
  assert.deepEqual(P.simplifyEnv([-3]), [[0, -3]]);
});

// ---------------------------------------------------------------------------------------------- embedding
test('embedPayload escapes exactly like the engine and round-trips hostile strings', () => {
  const hostile = { soniccraft_preview: 1, title: 'a </script><script>alert(1)</script> $& $1 $$ \u2028\u2029 <!-- x', report: { items: [] } };
  const html = embedPayload(template, hostile);
  const start = html.indexOf(DATA_OPEN) + DATA_OPEN.length, end = html.indexOf('</script>', start);
  const data = html.slice(start, end);
  assert.doesNotMatch(data, /<\/|\u2028|\u2029/);
  assert.deepEqual(JSON.parse(data), hostile);
  assert.ok(html.includes('$& $1 $$'), 'replacement patterns are not interpreted');
  assert.equal(escapeForScript('</a>\u2028'), '<\\/a>\\u2028');
  assert.equal(embedPayload(template, '{"a":1}').split('{"a":1}').length, 2);
  assert.throws(() => embedPayload('no token here', {}), /exactly once/);
  assert.throws(() => embedPayload(TOKEN + TOKEN, {}), /exactly once/);
});

test('the embedded page keeps its script structure', () => {
  const html = embedPayload(template, sample);
  assert.equal((html.match(/<script\b/g) || []).length, 2);
  assert.equal(mainScript(html), script);
});

test('formatJson is lossless', () => {
  assert.deepEqual(JSON.parse(formatJson(sample)), sample);
  assert.equal(formatJson([1, 2]), '[1,2]');
  assert.equal(formatJson({ a: { b: [1, { c: 2 }] } }), '{\n "a": {\n  "b": [\n   1,\n   {"c":2}\n  ]\n }\n}');
});

// ---------------------------------------------------------------------------------------------- fixture
test('sample.preview.json is up to date with make-sample.mjs (deterministic)', () => {
  const { payload } = buildSample();
  assert.equal(formatJson(payload) + '\n', sampleText);
  assert.equal(formatJson(buildSample().payload), formatJson(payload));
});

test('sample.preview.json is a consistent DESIGN.md §10.2 payload', () => {
  const r = sample.report, dur = r.output.duration_ms;
  assert.equal(sample.soniccraft_preview, 1);
  assert.equal(sample.waveform.points_per_second, 50);
  assert.equal(sample.waveform.min.length, Math.ceil(dur / 20));
  assert.equal(sample.waveform.max.length, sample.waveform.min.length);
  assert.ok(sample.waveform.min.every((v, i) => v <= sample.waveform.max[i] && v >= -1 && sample.waveform.max[i] <= 1));
  assert.ok(sample.waveform.max.every(v => Math.round(v * 1000) / 1000 === v), 'rounded to 3 decimals');
  assert.equal(sample.envelopes[0].gain_db.length, sample.waveform.min.length);
  assert.equal(r.tracks.length, 4);
  assert.ok(r.items.every((it, i) => i === 0 || r.items[i - 1].start_ms <= it.start_ms), 'items sorted by start');
  assert.ok(r.items.every(it => it.end_ms - it.start_ms === it.duration_ms));
  const cues = new Map(r.items.filter(i => i.kind === 'cue').map(i => [i.id, i]));
  for (const c of sample.captions) {
    const cue = cues.get(c.cue_id);
    assert.ok(c.start_ms >= cue.start_ms && c.end_ms <= cue.end_ms && c.end_ms - c.start_ms <= 6000, `caption ${c.index}`);
    assert.ok(c.lines.length <= 2 && c.lines.every(l => l.length <= 42) && c.lines.join(' ') === c.text);
  }
  assert.ok(sample.words.every((w, i) => i === 0 || sample.words[i - 1].start_ms <= w.start_ms), 'words chronological');
  for (const w of sample.words) {
    const cue = cues.get(w.cue_id);
    assert.ok(w.start_ms >= cue.start_ms && w.end_ms <= cue.end_ms && w.start_ms < w.end_ms);
  }
  const regions = r.ducking[0].regions;
  assert.ok(regions.every(g => g.start_ms < g.end_ms && g.end_ms <= dur));
  for (const w of sample.words) assert.ok(regions.some(g => g.start_ms <= w.start_ms && w.end_ms <= g.end_ms), `word ${w.text} is ducked`);
  assert.equal(cues.get('cue_03').pushed_from_ms, 4560);
  assert.equal(cues.get('cue_03').start_ms, cues.get('cue_02').end_ms + 350);
});
