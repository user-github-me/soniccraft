#!/usr/bin/env node
// Dev harness: builds sample.html from preview.html + sample.preview.json exactly the way the engine embeds a
// payload (DESIGN.md §10.2): the single placeholder token inside
// <script id="soniccraft-data" type="application/json"> is replaced by the compact payload JSON, with "</"
// escaped as "<\/" and U+2028 / U+2029 escaped as \u2028 / \u2029. If sample.mp3 is missing it is
// synthesized from the same deterministic model (make-sample.mjs, needs ffmpeg).
//
//   node build-sample.mjs            → sample.html (+ sample.mp3 when missing)
//   open sample.html                 (or: open "sample.html#t=6.2&zoom=2")

import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));

/** The placeholder the engine replaces. It must occur exactly once in preview.html. */
export const TOKEN = '__SONICCRAFT_DATA__';

/** Make JSON safe inside an HTML <script> element (the escaping the engine applies). */
export function escapeForScript(json) {
  return json.replaceAll('</', '<\\/').replaceAll('\u2028', '\\u2028').replaceAll('\u2029', '\\u2029');
}

/**
 * Substitute `payload` (an object, or already-serialized JSON text) into the template. Uses split/join rather
 * than String#replace so `$&`-style patterns inside the payload are never interpreted.
 */
export function embedPayload(template, payload) {
  const parts = template.split(TOKEN);
  if (parts.length !== 2) throw new Error(`preview.html must contain ${TOKEN} exactly once (found ${parts.length - 1})`);
  return parts[0] + escapeForScript(typeof payload === 'string' ? payload : JSON.stringify(payload)) + parts[1];
}

/** Pretty JSON that keeps flat objects and number arrays on one line (readable, diff-friendly fixtures). */
export function formatJson(value, indent = '') {
  const flat = v => v === null || typeof v !== 'object' || (Array.isArray(v) ? v.every(x => x === null || typeof x !== 'object')
    : Object.values(v).every(x => x === null || typeof x !== 'object' || (Array.isArray(x) && x.every(y => y === null || typeof y !== 'object'))));
  const inner = indent + ' ';
  if (Array.isArray(value)) {
    if (value.every(x => x === null || typeof x !== 'object')) return JSON.stringify(value);
    return '[\n' + value.map(x => inner + (flat(x) ? JSON.stringify(x) : formatJson(x, inner))).join(',\n') + '\n' + indent + ']';
  }
  if (value && typeof value === 'object') {
    if (flat(value) && JSON.stringify(value).length <= 120) return JSON.stringify(value);
    return '{\n' + Object.entries(value).map(([k, v]) => `${inner}${JSON.stringify(k)}: ${formatJson(v, inner)}`).join(',\n') + '\n' + indent + '}';
  }
  return JSON.stringify(value);
}

if (import.meta.url === pathToFileURL(process.argv[1] || '').href) {
  const template = readFileSync(join(HERE, 'preview.html'), 'utf8');
  const payload = JSON.parse(readFileSync(join(HERE, 'sample.preview.json'), 'utf8'));
  writeFileSync(join(HERE, 'sample.html'), embedPayload(template, payload));
  console.log(`wrote sample.html (${(Buffer.byteLength(template) / 1024).toFixed(1)} KiB template)`);
  if (!existsSync(join(HERE, 'sample.mp3'))) {
    const { buildSample, writeMp3 } = await import('./make-sample.mjs');
    try {
      const { left, right, sampleRate } = buildSample();
      writeMp3(join(HERE, 'sample.mp3'), left, right, sampleRate);
      console.log('wrote sample.mp3');
    } catch (err) {
      console.warn(`sample.mp3 not written: ${err.message}. The page still works; play runs silently.`);
    }
  }
}
