#!/usr/bin/env node
// Opt-in end-to-end test of preview.html in headless Chrome/Chromium, driven through the DevTools protocol with
// Node's built-in WebSocket (Node 22+, no dependencies). Covers playback, click/keyboard seeking, zoom, captions,
// table and diagnostics interaction, loader mode with real file inputs, degraded and hostile payloads, phone
// widths and console errors. Skips when no browser is found.
//
//   node --test browser-test.mjs          (CHROME_PATH=/path/to/chrome to choose the browser)

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { after, before, describe, it } from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { embedPayload } from './build-sample.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));

function findChrome() {
  const candidates = [process.env.CHROME_PATH, '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium', 'google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser'];
  for (const c of candidates.filter(Boolean)) {
    if (c.includes('/') ? existsSync(c) : spawnSync('which', [c]).status === 0) return c;
  }
  return null;
}
const CHROME = typeof WebSocket === 'function' ? findChrome() : null;

/** Minimal CDP session on the first page target. */
async function launchChrome(profile) {
  const proc = spawn(CHROME, ['--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=0',
    `--user-data-dir=${profile}`, '--autoplay-policy=no-user-gesture-required', '--hide-scrollbars', 'about:blank'], { stdio: ['ignore', 'ignore', 'pipe'] });
  const wsUrl = await new Promise((resolve, reject) => {
    let buf = '';
    const timer = setTimeout(() => reject(new Error('browser did not start')), 20000);
    proc.stderr.on('data', d => {
      buf += d;
      const m = /DevTools listening on (ws:\/\/\S+)/.exec(buf);
      if (m) { clearTimeout(timer); resolve(m[1]); }
    });
  });
  const port = new URL(wsUrl).port;
  let page;
  for (let i = 0; i < 50 && !page; i++) {
    page = (await (await fetch(`http://127.0.0.1:${port}/json/list`)).json()).find(t => t.type === 'page');
    if (!page) await new Promise(r => setTimeout(r, 100));
  }
  const ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map(), listeners = new Set(), logs = [];
  ws.onmessage = ev => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) {
      const { res, rej } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) rej(new Error(msg.error.message)); else res(msg.result);
    } else if (msg.method) for (const l of listeners) l(msg);
  };
  const send = (method, params = {}) => new Promise((res, rej) => { const i = ++id; pending.set(i, { res, rej }); ws.send(JSON.stringify({ id: i, method, params })); });
  listeners.add(m => {
    if (m.method === 'Runtime.consoleAPICalled') logs.push(`${m.params.type}: ${m.params.args.map(a => a.value ?? a.description).join(' ')}`);
    if (m.method === 'Runtime.exceptionThrown') logs.push(`exception: ${m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text}`);
    if (m.method === 'Log.entryAdded' && /error|warning/.test(m.params.entry.level)) logs.push(`${m.params.entry.level}: ${m.params.entry.text} ${m.params.entry.url || ''}`);
  });
  for (const domain of ['Runtime', 'Log', 'Page', 'DOM']) await send(`${domain}.enable`);
  const b = {
    logs,
    send,
    async eval(expression) {
      const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true });
      if (r.exceptionDetails) throw new Error(`page eval failed: ${r.exceptionDetails.exception?.description || r.exceptionDetails.text}`);
      return r.result.value;
    },
    async waitFor(expression, timeout = 5000) {
      const end = Date.now() + timeout;
      for (;;) {
        if (await b.eval(`(() => { try { return !!(${expression}); } catch { return false; } })()`)) return;
        if (Date.now() > end) throw new Error(`timed out waiting for: ${expression}`);
        await new Promise(r => setTimeout(r, 50));
      }
    },
    async goto(url) {
      await send('Page.navigate', { url: 'about:blank' });
      const loaded = new Promise((res, rej) => {
        const timer = setTimeout(() => rej(new Error(`load timeout: ${url}`)), 20000);
        const l = m => { if (m.method === 'Page.loadEventFired') { listeners.delete(l); clearTimeout(timer); res(); } };
        listeners.add(l);
      });
      await send('Page.navigate', { url });
      await loaded;
      await b.eval('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
    },
    viewport: (width, height, { mobile = false, dark = false } = {}) => Promise.all([
      send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: mobile ? 2 : 1, mobile }),
      send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: dark ? 'dark' : 'light' }] }),
    ]),
    async mouse(type, x, y, extra = {}) { await send('Input.dispatchMouseEvent', { type, x, y, button: 'left', clickCount: 1, ...extra }); },
    async click(x, y) { await b.mouse('mouseMoved', x, y); await b.mouse('mousePressed', x, y); await b.mouse('mouseReleased', x, y); },
    async clickSel(sel) { const r = await b.rect(sel); await b.click(r.x + r.w / 2, r.y + r.h / 2); },
    async key(key, code, keyCode, text) {
      await send('Input.dispatchKeyEvent', { type: 'keyDown', key, code, windowsVirtualKeyCode: keyCode, ...(text ? { text } : {}) });
      await send('Input.dispatchKeyEvent', { type: 'keyUp', key, code, windowsVirtualKeyCode: keyCode });
    },
    rect: sel => b.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); e.scrollIntoView({ block: 'nearest' }); const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height }; })()`),
    async setFiles(selector, files) {
      const { root } = await send('DOM.getDocument', { depth: -1 });
      const { nodeId } = await send('DOM.querySelector', { nodeId: root.nodeId, selector });
      await send('DOM.setFileInputFiles', { nodeId, files });
    },
    close() {
      try { ws.close(); } catch { /* already closed */ }
      if (proc.exitCode !== null || proc.signalCode !== null) return Promise.resolve();
      const exited = new Promise(r => proc.once('exit', r));
      proc.kill('SIGKILL');
      return exited;
    },
  };
  return b;
}

/** Playback position from the on-screen clock (works with real audio and with the silent clock). */
async function time(b) {
  await b.eval('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
  const [, m, s] = /^(\d+):(\d\d\.\d\d)$/.exec(await b.eval(`document.getElementById('now').textContent`));
  return +m * 60 + +s;
}
const text = (b, sel) => b.eval(`(document.querySelector(${JSON.stringify(sel)}) || {}).textContent || ''`);
const near = (a, x, tol = 0.06) => assert.ok(Math.abs(a - x) <= tol, `${a} is not within ${tol} of ${x}`);

describe('preview.html in headless Chrome', { skip: CHROME ? false : 'no Chrome/Chromium found (set CHROME_PATH) or Node lacks WebSocket' }, () => {
  let b, dir, sampleUrl, hasAudio;
  before(async () => {
    dir = mkdtempSync(join(tmpdir(), 'soniccraft-preview-'));
    const template = readFileSync(join(HERE, 'preview.html'), 'utf8');
    const sample = JSON.parse(readFileSync(join(HERE, 'sample.preview.json'), 'utf8'));
    hasAudio = existsSync(join(HERE, 'sample.mp3'));
    if (!hasAudio) {
      try {
        const { buildSample, writeMp3 } = await import('./make-sample.mjs');
        const { left, right, sampleRate } = buildSample();
        writeMp3(join(dir, 'sample.mp3'), left, right, sampleRate);
        hasAudio = true;
      } catch { /* no ffmpeg: playback checks are skipped */ }
    } else copyFileSync(join(HERE, 'sample.mp3'), join(dir, 'sample.mp3'));
    const page = (name, payload) => writeFileSync(join(dir, name), typeof payload === 'string' ? payload : embedPayload(template, payload));
    const clone = () => JSON.parse(JSON.stringify(sample));
    page('sample.html', sample);
    page('template.html', template);
    page('minimal.html', { soniccraft_preview: 1, report: { items: [] } });
    const bare = clone();
    for (const k of ['waveform', 'envelopes', 'words', 'captions', 'audio']) delete bare[k];
    page('bare.html', bare);
    const missing = clone();
    missing.audio = 'missing-file.mp3';
    page('missing.html', missing);
    const hostile = clone();
    hostile.title = 'Hostile </script><b>x</b>';
    hostile.report.items[1].text = 'Lesson: <!-- then <script>window.__pwned = 1</script> and $& \u2028';
    hostile.report.diagnostics[0].message = '<img src=x onerror="window.__pwned=2"> `code`';
    page('hostile.html', hostile);
    page('broken.html', template.replace('__SONICCRAFT_DATA__', '{"soniccraft_preview": 1,'));
    writeFileSync(join(dir, 'manifest.json'), JSON.stringify(sample.report));
    writeFileSync(join(dir, 'words.json'), JSON.stringify({ version: '1.0', captions: sample.captions, words: sample.words }));
    writeFileSync(join(dir, 'other.json'), '{"hello": 1}');
    sampleUrl = pathToFileURL(join(dir, 'sample.html')).href;
    b = await launchChrome(join(dir, 'profile'));
    await b.viewport(1440, 900);
  });
  after(async () => {
    await b?.close();
    if (dir) rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
  });
  const url = name => pathToFileURL(join(dir, name)).href;
  const errorsSince = n => b.logs.slice(n).filter(l => !/missing-file\.mp3/.test(l));

  it('renders the sample without console output', async () => {
    const n = b.logs.length;
    await b.goto(sampleUrl);
    assert.equal(await b.eval('document.title'), 'Tech briefing — SonicCraft preview');
    assert.deepEqual(await b.eval(`[document.querySelectorAll('#rows tr').length, document.querySelectorAll('#diags li').length, document.querySelectorAll('.lane').length]`), [6, 3, 4]);
    assert.equal(await text(b, '.cap.next .cap-who'), 'narrator'); // the first caption is previewed at t=0
    assert.deepEqual(errorsSince(n), []);
  });

  it('plays and pauses the audio', async t => {
    if (!hasAudio) return t.skip('no sample audio (needs ffmpeg)');
    await b.waitFor(`document.getElementById('audio').readyState >= 1`);
    near(await b.eval(`document.getElementById('audio').duration`), 18.75, 0.1);
    await b.clickSel('#play');
    await b.waitFor(`document.getElementById('audio').currentTime > 0.3`);
    assert.equal(await b.eval(`document.getElementById('play').getAttribute('aria-label')`), 'Pause');
    await b.clickSel('#play');
    await b.waitFor(`document.getElementById('audio').paused`);
  });

  it('seeks by clicking the timeline and with the keyboard', async () => {
    const sc = await b.rect('#scroll'), lanes = await b.rect('#lanes');
    await b.click(sc.x + sc.w / 2, lanes.y + 10);
    near(await time(b), 18.75 / 2, 0.05);
    assert.equal(await b.eval('document.activeElement.id'), 'scroll');
    await b.key('ArrowRight', 'ArrowRight', 39);
    near(await time(b), 18.75 / 2 + 5, 0.05);
    await b.key('ArrowLeft', 'ArrowLeft', 37);
    await b.key('ArrowLeft', 'ArrowLeft', 37);
    near(await time(b), 18.75 / 2 - 5, 0.05);
    await b.key('End', 'End', 35);
    near(await time(b), 18.75, 0.06);
    await b.key('Home', 'Home', 36);
    near(await time(b), 0, 0.01);
    assert.match(await b.eval(`document.getElementById('scroll').getAttribute('aria-valuetext')`), /^0:00 of 0:19$/);
  });

  it('zooms with the buttons, ctrl+wheel and keys', async () => {
    await b.clickSel('#zin');
    const [w, v] = await b.eval(`[document.getElementById('content').offsetWidth, document.getElementById('scroll').clientWidth]`);
    assert.ok(Math.abs(w - 2 * v) <= 1, `${w} vs ${v}`);
    assert.equal(await text(b, '#zlvl'), '2.0×');
    const sc = await b.rect('#scroll'), lanes = await b.rect('#lanes');
    await b.mouse('mouseWheel', sc.x + 100, lanes.y + 10, { deltaX: 0, deltaY: -300, modifiers: 2 });
    await b.waitFor(`parseFloat(document.getElementById('zlvl').textContent) > 2`);
    await b.key('0', 'Digit0', 48, '0');
    assert.equal(await text(b, '#zlvl'), '1.0×');
  });

  it('shows the caption with karaoke highlighting and highlights playing rows', async () => {
    await b.eval(`location.hash = 't=6.25'`);
    await b.waitFor(`document.querySelector('.cap:not(.next) .w.now')`);
    assert.equal(await text(b, '.cap-who'), 'guest');
    assert.match(await text(b, '.cap-text'), /^Exactly\. SonicCraft/);
    assert.deepEqual(await b.eval(`[...document.querySelectorAll('#rows tr.on td:first-child code')].map(c => c.textContent)`), ['ambient_bed', 'cue_02']);
  });

  it('seeks from table rows and diagnostic chips', async () => {
    const row = await b.eval(`(() => { const r = [...document.querySelectorAll('#rows tr')].find(r => r.querySelector('code').textContent === 'cue_03'); r.scrollIntoView({ block: 'center' }); const q = r.getBoundingClientRect(); return { x: q.x + 30, y: q.y + q.height / 2 }; })()`);
    await b.click(row.x, row.y);
    near(await time(b), 10.13, 0.01);
    const chip = await b.eval(`(() => { const c = [...document.querySelectorAll('.chip')].find(c => c.textContent === 'cue_02'); c.scrollIntoView({ block: 'center' }); const q = c.getBoundingClientRect(); return { x: q.x + q.width / 2, y: q.y + q.height / 2 }; })()`);
    await b.click(chip.x, chip.y);
    near(await time(b), 4.36, 0.01);
    assert.ok(await b.eval(`document.querySelectorAll('.dg-msg code').length >= 3`), 'backticks render as code');
  });

  it('shows tooltips for blocks and the ducking gain', async () => {
    await b.eval('window.scrollTo(0, 0)');
    const blk = await b.rect('.lane[data-track=voice_secondary] .blk');
    await b.mouse('mouseMoved', blk.x + 30, blk.y + blk.h / 2);
    assert.match(await text(b, '#tip'), /^cue_02 cue on voice_secondaryguest: Exactly\./);
    const bed = await b.rect('.lane[data-track=ambient_bed]');
    await b.mouse('mouseMoved', bed.x + bed.w * 0.3, bed.y + bed.h / 2);
    assert.match(await text(b, '#tip'), /Ducking gain at \d:\d\d\.\d\d: −18\.0 dB$/);
    assert.doesNotMatch(await text(b, '#tip'), /false|undefined|null/);
    await b.mouse('mouseMoved', 5, 5);
    assert.ok(await b.eval(`document.getElementById('tip').hidden`));
  });

  it('fits phone widths without horizontal overflow', async () => {
    for (const width of [390, 375]) {
      await b.viewport(width, 844, { mobile: true });
      await b.goto(`${sampleUrl}?w=${width}#t=6.2`);
      assert.deepEqual(await b.eval(`[document.documentElement.scrollWidth, document.documentElement.clientWidth]`), [width, width]);
    }
    await b.viewport(1440, 900, { dark: true });
    await b.goto(sampleUrl);
    assert.equal(await b.eval('getComputedStyle(document.body).backgroundColor'), 'rgb(15, 18, 22)');
    await b.viewport(1440, 900);
  });

  it('opens files in loader mode', async () => {
    const n = b.logs.length;
    await b.goto(url('template.html'));
    assert.deepEqual(await b.eval(`[document.getElementById('loader').hidden, document.getElementById('app').hidden]`), [false, true]);
    await b.setFiles('#pickJson', [join(dir, 'other.json')]);
    await b.waitFor(`/not a SonicCraft/.test(document.getElementById('lstatus').textContent)`);
    if (hasAudio) {
      await b.setFiles('#pickAudio', [join(dir, 'sample.mp3')]);
      await b.waitFor(`/Audio ready/.test(document.getElementById('lstatus').textContent)`);
    }
    await b.setFiles('#pickJson', [join(dir, 'manifest.json')]);
    await b.waitFor(`!document.getElementById('app').hidden`);
    assert.equal(await b.eval(`document.querySelectorAll('#rows tr[data-i]').length`), 6);
    if (hasAudio) assert.match(await b.eval(`document.getElementById('audio').getAttribute('src')`), /^blob:/);
    await b.eval(`location.hash = 't=6.3'`);
    await b.waitFor(`/web-based video\\.$/.test(document.querySelector('.cap-text').textContent)`); // cue text fallback
    await b.setFiles('#pickAudio2', [join(dir, 'words.json')]); // JSON is classified by content, whatever the picker
    await b.waitFor(`document.querySelectorAll('.w').length > 3`);
    assert.deepEqual(errorsSince(n), []);
  });

  it('degrades gracefully on minimal, bare, missing-audio, hostile and broken payloads', async () => {
    const n = b.logs.length;
    const page = async name => { await b.goto(url(name) + '#t=6.3'); return b.eval(`({ app: !document.getElementById('app').hidden, rows: document.querySelectorAll('#rows tr[data-i]').length, note: document.getElementById('anote').hidden ? '' : document.getElementById('anoteText').textContent, status: document.getElementById('lstatus').textContent, pwned: window.__pwned || 0, title: document.title })`); };
    let s = await page('minimal.html');
    assert.ok(s.app && s.rows === 0 && /no audio file/.test(s.note), JSON.stringify(s));
    s = await page('bare.html');
    assert.ok(s.app && s.rows === 6, JSON.stringify(s));
    assert.match(await text(b, '.cap-text'), /web-based video\.$/);
    s = await page('missing.html');
    await b.waitFor(`/Could not load “missing-file\\.mp3”/.test(document.getElementById('anoteText').textContent)`);
    s = await page('hostile.html');
    assert.ok(s.app && s.rows === 6 && s.pwned === 0 && s.title.startsWith('Hostile </script><b>x</b>'), JSON.stringify(s));
    s = await page('broken.html');
    assert.ok(!s.app && /could not be read/.test(s.status), JSON.stringify(s));
    assert.deepEqual(errorsSince(n), []);
  });
});
