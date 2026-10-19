#!/usr/bin/env node
// Write release.json (download URL + SHA-256 of every release archive) and the real repository URLs
// (package.json, README.md) into this package, so the published launcher verifies downloads against
// checksums that come from the npm registry. Run by .github/workflows/release.yml after the archives are
// built:
//
//   node scripts/pin-release.mjs --sums dist/SHA256SUMS --repository https://github.com/<owner>/<repo>
//
// SHA256SUMS is `sha256sum` output for the archives. Exits 1 (writing nothing) when an archive of this
// package's version is missing.

import { readFileSync, writeFileSync } from 'node:fs';
import { parseArgs } from 'node:util';
import { RELEASE_FILE, releaseProblems } from '../lib/resolve.mjs';

const PLACEHOLDER = 'https://github.com/OWNER/soniccraft';
const packageFile = new URL('../package.json', import.meta.url);
const readmeFile = new URL('../README.md', import.meta.url);

const { values } = parseArgs({ options: { sums: { type: 'string' }, repository: { type: 'string' } } });
if (!values.sums || !values.repository) {
  process.stderr.write('usage: pin-release.mjs --sums SHA256SUMS --repository https://github.com/<owner>/<repo>\n');
  process.exit(2);
}
const repository = values.repository.replace(/\/+$/, '');
if (!/^https:\/\/\S+$/.test(repository) || repository.includes('/OWNER/')) {
  process.stderr.write(`pin-release: --repository must be the project's https:// URL, not ${repository}\n`);
  process.exit(2);
}

const packageText = readFileSync(packageFile, 'utf8');
const { version } = JSON.parse(packageText);
const prefix = `soniccraft-v${version}-`;
const sha256 = {};
for (const line of readFileSync(values.sums, 'utf8').split('\n')) {
  const [hash, name] = line.trim().split(/\s+/);
  const file = name?.replace(/^\*/, '');
  if (file?.startsWith(prefix) && /^[0-9a-fA-F]{64}$/.test(hash ?? '')) sha256[file] = hash.toLowerCase();
}
const release = {
  version,
  downloadBase: `${repository}/releases/download`,
  sha256: Object.fromEntries(Object.entries(sha256).sort(([a], [b]) => (a < b ? -1 : 1))),
};
const problems = releaseProblems(release, version);
if (problems.length) {
  process.stderr.write(`pin-release: ${values.sums} is incomplete: ${problems.join('; ')}\n`);
  process.exit(1);
}
writeFileSync(RELEASE_FILE, `${JSON.stringify(release, null, 2)}\n`);
writeFileSync(packageFile, packageText.replaceAll(PLACEHOLDER, repository));
writeFileSync(readmeFile, readFileSync(readmeFile, 'utf8').replaceAll(PLACEHOLDER, repository));
process.stderr.write(`pin-release: wrote release.json for ${version} (${Object.keys(sha256).length} archives)\n`);
