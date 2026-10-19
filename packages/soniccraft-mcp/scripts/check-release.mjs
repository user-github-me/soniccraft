#!/usr/bin/env node
// `npm pack` / `npm publish` guard (the `prepack` script): a published soniccraft-mcp must carry a complete
// release.json for its version and the project's real repository URL (package.json, README.md), or it
// could not download soniccraft.
// The release workflow writes both with scripts/pin-release.mjs; see README.md, "Development".

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { LauncherError, RELEASE_FILE, loadRelease } from '../lib/resolve.mjs';

const packageJson = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
const readme = new URL('../README.md', import.meta.url);
const problems = [];
try {
  if (!loadRelease(packageJson.version)) {
    problems.push(`${fileURLToPath(RELEASE_FILE)} is missing (run scripts/pin-release.mjs)`);
  }
} catch (error) {
  if (!(error instanceof LauncherError)) throw error;
  problems.push(error.message);
}
for (const [name, text] of [['package.json', JSON.stringify(packageJson)], ['README.md', readFileSync(readme, 'utf8')]]) {
  if (text.includes('/OWNER/')) problems.push(`${name} still has the OWNER placeholder`);
}
if (problems.length) {
  process.stderr.write(`soniccraft-mcp cannot be packed for publishing:\n  - ${problems.join('\n  - ')}\n`);
  process.exit(1);
}
