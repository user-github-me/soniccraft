// Tests for the soniccraft-mcp launcher: binary resolution, downloads (checksums, limits), the PATH rules
// on each platform, argument and signal pass-through, and the MCP handshake end to end with a real
// soniccraft binary when one is built (SONICCRAFT_TEST_BIN=<path>, else target/{release,debug}/soniccraft).
// Run: npm test   (Node 20 or newer, no dependencies)

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync,
} from 'node:fs';
import { createServer } from 'node:http';
import { constants, tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { after, describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  LauncherError, RELEASE_TARGETS, archiveFile, cacheRoot, compatible, findOnPath, loadRelease, proxyHint,
  releaseProblems, resolveBinary, targetTriple,
} from '../lib/resolve.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const launcher = join(here, '..', 'bin', 'soniccraft-mcp.mjs');
const { version } = JSON.parse(readFileSync(join(here, '..', 'package.json'), 'utf8'));
const unix = process.platform !== 'win32';
const scratch = mkdtempSync(join(tmpdir(), 'soniccraft-mcp-test-'));
after(() => rmSync(scratch, { recursive: true, force: true }));
let counter = 0;
const fresh = (name) => join(scratch, `${name}-${(counter += 1)}`);

/**
 * A fake `soniccraft` (shell script): `--version` prints `reports` (default: this package's version);
 * anything else prints its arguments, or runs `body` when given.
 */
function fakeBinary(dir, body = 'echo "args: $*"\nexit "${FAKE_EXIT:-0}"', reports = version) {
  mkdirSync(dir, { recursive: true });
  const path = join(dir, 'soniccraft');
  writeFileSync(path, `#!/bin/sh\nif [ "$1" = --version ]; then echo "soniccraft ${reports}"; exit 0; fi\n${body}\n`);
  chmodSync(path, 0o755);
  return path;
}

/** Serve `routes` ({urlPath: Buffer | (req, res) => void}) on 127.0.0.1; returns {base, hits, close}. */
async function serve(routes) {
  const hits = [];
  const server = createServer((req, res) => {
    hits.push(req.url);
    const route = routes[req.url];
    if (typeof route === 'function') {
      route(req, res);
    } else if (route) {
      res.writeHead(200, { 'content-length': route.length }).end(route);
    } else {
      res.writeHead(404).end();
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const close = () => {
    server.closeAllConnections();
    return new Promise((resolve) => server.close(resolve));
  };
  return { base: `http://127.0.0.1:${server.address().port}`, hits, close };
}

/** The tar that reads and writes .zip on Windows (bsdtar), or the system tar elsewhere. */
const tarBin = unix ? 'tar' : join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'tar.exe');

/**
 * A release archive like the release workflow builds, for this platform's target. `files` maps names
 * inside the archive's top directory to contents (default: a fake soniccraft and soniccraft-mcp).
 */
function releaseArchive({ flavor = 'mp3', files } = {}) {
  const target = targetTriple();
  const file = archiveFile(version, target, flavor);
  const name = file.replace(/\.(zip|tar\.gz)$/, '');
  const root = mkdtempSync(join(scratch, 'archive-'));
  const dir = join(root, name);
  mkdirSync(dir);
  if (files) {
    for (const [entry, content] of Object.entries(files)) writeFileSync(join(dir, entry), content);
  } else if (unix) {
    fakeBinary(dir);
    writeFileSync(join(dir, 'soniccraft-mcp'), '#!/bin/sh\n');
  } else {
    writeFileSync(join(dir, 'soniccraft.exe'), 'not a real program');
    writeFileSync(join(dir, 'soniccraft-mcp.exe'), 'not a real program');
  }
  const out = join(root, file);
  const args = file.endsWith('.zip') ? ['-a', '-cf', out, '-C', root, name] : ['-czf', out, '-C', root, name];
  const tar = spawnSync(tarBin, args, { env: { ...process.env, COPYFILE_DISABLE: '1' } });
  assert.equal(tar.status, 0, String(tar.stderr));
  const bytes = readFileSync(out);
  const sha256 = createHash('sha256').update(bytes).digest('hex');
  return { file, name, target, bytes, sha256, sumLine: Buffer.from(`${sha256}  ${file}\n`) };
}

/** A complete release manifest that pins `archive` (and placeholder sums for the other archives). */
function manifestFor(archive) {
  const sha256 = {};
  for (const target of RELEASE_TARGETS) {
    for (const flavor of ['mp3', 'no-mp3']) sha256[archiveFile(version, target, flavor)] = 'f'.repeat(64);
  }
  sha256[archive.file] = archive.sha256;
  return { version, downloadBase: 'https://example.invalid/releases/download', sha256 };
}

const NO_PATH = { PATH: '' };

describe('names and platforms', () => {
  test('known platforms map to release targets', () => {
    assert.equal(targetTriple('darwin', 'arm64'), 'aarch64-apple-darwin');
    assert.equal(targetTriple('darwin', 'x64'), 'x86_64-apple-darwin');
    assert.equal(targetTriple('linux', 'x64'), 'x86_64-unknown-linux-gnu');
    assert.equal(targetTriple('linux', 'arm64'), 'aarch64-unknown-linux-gnu');
    assert.equal(targetTriple('win32', 'x64'), 'x86_64-pc-windows-msvc');
    assert.equal(targetTriple('win32', 'arm64'), 'x86_64-pc-windows-msvc', 'x64 emulation on Windows on Arm');
    assert.equal(targetTriple('freebsd', 'x64'), null);
    assert.equal(targetTriple('linux', 'ia32'), null);
  });

  test('archive names follow the release workflow', () => {
    assert.equal(archiveFile('0.1.0', 'aarch64-apple-darwin'), 'soniccraft-v0.1.0-aarch64-apple-darwin.tar.gz');
    assert.equal(archiveFile('0.1.0', 'x86_64-pc-windows-msvc', 'no-mp3'), 'soniccraft-v0.1.0-x86_64-pc-windows-msvc-no-mp3.zip');
    const workflow = join(here, '..', '..', '..', '.github', 'workflows', 'release.yml');
    if (!existsSync(workflow)) return; // outside the repository (e.g. an unpacked npm package)
    const text = readFileSync(workflow, 'utf8');
    const built = [...text.matchAll(/target: ([a-z0-9_-]+)/g)].map((m) => m[1]);
    assert.deepEqual([...new Set(built)].sort(), RELEASE_TARGETS, 'release.yml builds exactly the targets the launcher downloads');
    // TAG is `v<version>`; the archive is <name>.zip on Windows, <name>.tar.gz elsewhere, with <name>/ inside.
    for (const line of ['name="soniccraft-$TAG-$TARGET"', 'name="$name-no-mp3"', 'asset="$name.zip"', 'asset="$name.tar.gz"']) {
      assert.ok(text.includes(line), `release.yml names archives with ${line}`);
    }
  });

  test('cache root follows the platform conventions', () => {
    assert.equal(cacheRoot({ SONICCRAFT_BIN_CACHE: '/c' }, 'linux', '/h'), '/c');
    assert.equal(cacheRoot({}, 'linux', '/h'), join('/h', '.cache', 'soniccraft', 'bin'));
    assert.equal(cacheRoot({ XDG_CACHE_HOME: '/x' }, 'linux', '/h'), join('/x', 'soniccraft', 'bin'));
    assert.equal(cacheRoot({}, 'darwin', '/h'), join('/h', 'Library', 'Caches', 'soniccraft', 'bin'));
    assert.equal(cacheRoot({ LOCALAPPDATA: 'L' }, 'win32', '/h'), join('L', 'soniccraft', 'bin'));
  });

  test('version compatibility (minor versions may break before 1.0)', () => {
    assert.ok(compatible('0.1.3', '0.1.0'));
    assert.ok(!compatible('0.2.0', '0.1.0'));
    assert.ok(compatible('1.4.0', '1.0.2'));
    assert.ok(!compatible('2.0.0', '1.0.0'));
  });

  test('a release manifest must pin every archive of its version', () => {
    const archive = { file: archiveFile(version, RELEASE_TARGETS[0]), sha256: 'a'.repeat(64) };
    const complete = manifestFor(archive);
    assert.deepEqual(releaseProblems(complete, version), []);
    assert.match(releaseProblems(complete, '9.9.9').join(), /version is/);
    const missing = structuredClone(complete);
    delete missing.sha256[archive.file];
    assert.match(releaseProblems(missing, version).join(), new RegExp(`no SHA-256 for ${archive.file.replaceAll('.', '\\.')}`));
    assert.match(releaseProblems({ ...complete, downloadBase: 'http://example.com/x' }, version).join(), /downloadBase/);
    assert.match(releaseProblems({ ...complete, downloadBase: 'https://github.com/OWNER/soniccraft/releases/download' }, version).join(), /downloadBase/);
  });

  test('the proxy hint appears only when a proxy is set and Node is not told to use it', () => {
    assert.equal(proxyHint({}, []), '');
    assert.match(proxyHint({ https_proxy: 'http://p:1' }, []), /^ https_proxy is set, .*NODE_USE_ENV_PROXY=1/);
    assert.equal(proxyHint({ HTTPS_PROXY: 'http://p:1', NODE_USE_ENV_PROXY: '1' }, []), '');
    assert.equal(proxyHint({ HTTPS_PROXY: 'http://p:1', NODE_OPTIONS: '--max-old-space-size=99 --use-env-proxy' }, []), '');
    assert.equal(proxyHint({ HTTP_PROXY: 'http://p:1' }, ['--use-env-proxy']), '');
  });

  test('a broken release.json is reported, a missing one means "not pinned"', () => {
    assert.equal(loadRelease(version, join(scratch, 'no-such-release.json')), null);
    const broken = join(scratch, 'broken-release.json');
    writeFileSync(broken, '{"version": "0.0.0"');
    assert.throws(() => loadRelease(version, broken), (e) => e instanceof LauncherError && /not valid JSON/.test(e.message));
    writeFileSync(broken, JSON.stringify({ version, downloadBase: 'https://example.com/d', sha256: {} }));
    assert.throws(() => loadRelease(version, broken), (e) => e instanceof LauncherError && /no SHA-256/.test(e.message));
  });
});

describe('PATH lookup', () => {
  test('Windows: only soniccraft.exe counts, never a batch file or script', () => {
    const dir = fresh('win-path');
    mkdirSync(dir);
    writeFileSync(join(dir, 'soniccraft.cmd'), '@echo off\n');
    writeFileSync(join(dir, 'soniccraft.js'), '');
    const env = { PATH: `${join(scratch, 'nothing-here')};"${dir}"`, PATHEXT: '.COM;.EXE;.BAT;.CMD;.JS' };
    assert.equal(findOnPath('soniccraft', env, 'win32'), null);
    writeFileSync(join(dir, 'soniccraft.exe'), '');
    assert.equal(findOnPath('soniccraft', env, 'win32'), join(dir, 'soniccraft.exe'));
  });

  test('Unix: a file without the executable bit is skipped; the result is absolute', { skip: !unix && 'POSIX modes' }, () => {
    const plain = fresh('plain');
    mkdirSync(plain);
    writeFileSync(join(plain, 'soniccraft'), '#!/bin/sh\n');
    const real = fakeBinary(fresh('exec'));
    assert.equal(findOnPath('soniccraft', { PATH: `${plain}:${dirname(real)}` }, 'linux'), real);
    assert.equal(findOnPath('soniccraft', { PATH: plain }, 'linux'), null);
  });
});

describe('resolution', { skip: !unix && 'uses shell-script fakes' }, () => {
  test('SONICCRAFT_BIN wins, and must be an executable file', async () => {
    const bin = fakeBinary(fresh('env-bin'));
    assert.deepEqual(await resolveBinary({ version, env: { ...NO_PATH, SONICCRAFT_BIN: bin } }), { path: bin, source: 'env' });
    await assert.rejects(
      resolveBinary({ version, env: { ...NO_PATH, SONICCRAFT_BIN: join(scratch, 'missing') } }),
      (e) => e instanceof LauncherError && /SONICCRAFT_BIN .* not a file/.test(e.message),
    );
    const plain = join(fresh('plain-bin'), 'soniccraft');
    mkdirSync(dirname(plain));
    writeFileSync(plain, '#!/bin/sh\n');
    await assert.rejects(resolveBinary({ version, env: { ...NO_PATH, SONICCRAFT_BIN: plain } }), /not executable/);
    const batch = join(dirname(plain), 'soniccraft.cmd');
    writeFileSync(batch, '@echo off\n');
    await assert.rejects(resolveBinary({ version, platform: 'win32', env: { ...NO_PATH, SONICCRAFT_BIN: batch } }), /soniccraft\.exe/);
  });

  test('soniccraft on PATH comes next; an incompatible version gets a note', async () => {
    const notes = [];
    const log = (m) => notes.push(m);
    const bin = fakeBinary(fresh('path-bin'));
    assert.deepEqual(await resolveBinary({ version, env: { PATH: dirname(bin) }, log }), { path: bin, source: 'path' });
    assert.deepEqual(notes, [], 'a compatible version is used silently');
    const old = fakeBinary(fresh('old-bin'), 'exit 0', '99.0.0');
    assert.deepEqual(await resolveBinary({ version, env: { PATH: dirname(old) }, log }), { path: old, source: 'path' });
    assert.match(notes.join('\n'), /^note: using .* \(soniccraft 99\.0\.0\); this launcher expects /);
  });

  test('without a configured release URL the error says how to install', async () => {
    await assert.rejects(
      resolveBinary({ version, release: null, env: { ...NO_PATH, SONICCRAFT_BIN_CACHE: fresh('empty-cache') } }),
      (e) => e instanceof LauncherError && /cargo install --locked/.test(e.message) && /SONICCRAFT_BIN/.test(e.message),
    );
  });

  test('an unknown SONICCRAFT_FLAVOR is an error, not a silent default', async () => {
    const env = { ...NO_PATH, SONICCRAFT_FLAVOR: 'nomp3', SONICCRAFT_BIN_CACHE: fresh('flavor-cache') };
    await assert.rejects(resolveBinary({ version, release: null, env }), /SONICCRAFT_FLAVOR is `nomp3`; use `mp3` .* or `no-mp3`/);
  });

  test('musl systems get a clear error before anything is downloaded', async () => {
    const server = await serve({});
    try {
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: fresh('musl-cache'), SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(
        resolveBinary({ version, platform: 'linux', arch: 'x64', libc: 'musl', release: null, env }),
        /need glibc, and this system uses musl/,
      );
      assert.equal(server.hits.length, 0);
    } finally {
      await server.close();
    }
  });

  test('downloads the pinned archive, verifies it and caches it', async () => {
    const archive = releaseArchive();
    const server = await serve({ [`/v${version}/${archive.file}`]: archive.bytes });
    try {
      const cache = fresh('download-cache');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: `${server.base}/` };
      const logs = [];
      const first = await resolveBinary({ version, env, release: manifestFor(archive), log: (m) => logs.push(m) });
      assert.deepEqual(first, { path: join(cache, `v${version}`, archive.target, 'soniccraft'), source: 'download' });
      assert.match(spawnSync(first.path, ['x'], { encoding: 'utf8' }).stdout, /args: x/);
      assert.deepEqual(server.hits, [`/v${version}/${archive.file}`], 'a pinned checksum needs no .sha256 request');
      assert.match(logs.join('\n'), /downloading .*\n.*installed /);
      assert.deepEqual(readdirSync(dirname(dirname(first.path))), [archive.target], 'no staging leftovers');
      const second = await resolveBinary({ version, env, release: manifestFor(archive) });
      assert.deepEqual(second, { path: first.path, source: 'cache' });
      assert.equal(server.hits.length, 1, 'the cache is used without network requests');
    } finally {
      await server.close();
    }
  });

  test('the pinned checksum wins over a matching .sha256 from the server', async () => {
    const pinned = releaseArchive();
    const swapped = releaseArchive({ files: { soniccraft: '#!/bin/sh\necho evil\n' } });
    const path = `/v${version}/${swapped.file}`;
    const server = await serve({ [path]: swapped.bytes, [`${path}.sha256`]: swapped.sumLine });
    try {
      const cache = fresh('pinned-cache');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(resolveBinary({ version, env, release: manifestFor(pinned) }), /Checksum mismatch/);
      assert.ok(!existsSync(join(cache, `v${version}`, pinned.target)), 'nothing is installed');
    } finally {
      await server.close();
    }
  });

  test('without pinned checksums the published .sha256 is checked', async () => {
    const archive = releaseArchive();
    const path = `/v${version}/${archive.file}`;
    const wrong = Buffer.from(`${'0'.repeat(64)}  ${archive.file}\n`);
    const server = await serve({ [path]: archive.bytes, [`${path}.sha256`]: wrong });
    try {
      const cache = fresh('unpinned-cache');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(resolveBinary({ version, env, release: null }), /Checksum mismatch/);
      assert.ok(!existsSync(join(cache, `v${version}`, archive.target)));
    } finally {
      await server.close();
    }
  });

  test('SONICCRAFT_FLAVOR=no-mp3 downloads the build without LAME into its own cache directory', async () => {
    const archive = releaseArchive({ flavor: 'no-mp3' });
    const server = await serve({ [`/v${version}/${archive.file}`]: archive.bytes });
    try {
      const cache = fresh('no-mp3-cache');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base, SONICCRAFT_FLAVOR: 'no-mp3' };
      const found = await resolveBinary({ version, env, release: manifestFor(archive) });
      assert.equal(found.path, join(cache, `v${version}`, `${archive.target}-no-mp3`, 'soniccraft'));
    } finally {
      await server.close();
    }
  });

  test('a network failure with an unused HTTPS_PROXY says how to make Node use it', async () => {
    const server = await serve({});
    const base = server.base;
    await server.close(); // nothing listens on the port any more: the connection is refused
    const archive = releaseArchive();
    const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: fresh('proxy-cache'), SONICCRAFT_DOWNLOAD_BASE: base };
    const attempt = (extra) => resolveBinary({ version, env: { ...env, ...extra }, release: manifestFor(archive) });
    await assert.rejects(attempt({}), (e) => /^Cannot download .*ECONNREFUSED.*\.$/.test(e.message) && !/PROXY/.test(e.message));
    await assert.rejects(
      attempt({ HTTPS_PROXY: 'http://proxy.example:3128' }),
      (e) => e instanceof LauncherError && /ECONNREFUSED.*\. HTTPS_PROXY is set, but Node uses it only with NODE_USE_ENV_PROXY=1/.test(e.message),
    );
  });

  test('a missing release is a readable error', async () => {
    const server = await serve({});
    try {
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: fresh('none-cache'), SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(resolveBinary({ version, env, release: null }), /HTTP 404/);
    } finally {
      await server.close();
    }
  });

  test('an oversized download is cut off, even without a content-length', async () => {
    const archive = releaseArchive();
    const server = await serve({
      [`/v${version}/${archive.file}`]: (req, res) => {
        res.writeHead(200);
        res.write(Buffer.alloc(800));
        res.end(Buffer.alloc(800));
      },
    });
    try {
      const cache = fresh('big-cache');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(resolveBinary({ version, env, release: manifestFor(archive), maxBytes: 1024 }), /larger than 1024 bytes/);
      assert.ok(!existsSync(join(cache, `v${version}`)));
    } finally {
      await server.close();
    }
  });

  test('a stalled download times out instead of hanging the client', async () => {
    const archive = releaseArchive();
    const server = await serve({ [`/v${version}/${archive.file}`]: () => {} });
    try {
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: fresh('stall-cache'), SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(resolveBinary({ version, env, release: manifestFor(archive), timeoutMs: 300 }), /timed out/);
    } finally {
      await server.close();
    }
  });

  test('an archive without the binary is rejected', async () => {
    const archive = releaseArchive({ files: { 'README.md': 'no binary here\n' } });
    const server = await serve({ [`/v${version}/${archive.file}`]: archive.bytes });
    try {
      const cache = fresh('empty-archive-cache');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(resolveBinary({ version, env, release: manifestFor(archive) }), /does not contain the file/);
      assert.deepEqual(readdirSync(join(cache, `v${version}`)), [], 'the staging directory is removed');
    } finally {
      await server.close();
    }
  });

  test('a leftover cache directory without the binary is reported with what to remove', async () => {
    const archive = releaseArchive();
    const server = await serve({ [`/v${version}/${archive.file}`]: archive.bytes });
    try {
      const cache = fresh('blocked-cache');
      const blocked = join(cache, `v${version}`, archive.target);
      mkdirSync(blocked, { recursive: true });
      writeFileSync(join(blocked, 'README.md'), 'left over\n');
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base };
      await assert.rejects(
        resolveBinary({ version, env, release: manifestFor(archive) }),
        (e) => e instanceof LauncherError && e.message.includes(`Cannot install into ${blocked}`)
          && /Remove that directory and run this again\.$/.test(e.message),
      );
      assert.deepEqual(readdirSync(join(cache, `v${version}`)), [archive.target], 'the staging directory is removed');
    } finally {
      await server.close();
    }
  });

  test('two launchers downloading at once both end up with the binary', async () => {
    const archive = releaseArchive();
    const server = await serve({ [`/v${version}/${archive.file}`]: archive.bytes });
    try {
      const env = { ...NO_PATH, SONICCRAFT_BIN_CACHE: fresh('race-cache'), SONICCRAFT_DOWNLOAD_BASE: server.base };
      const [a, b] = await Promise.all([
        resolveBinary({ version, env, release: manifestFor(archive) }),
        resolveBinary({ version, env, release: manifestFor(archive) }),
      ]);
      assert.equal(a.path, b.path);
      assert.match(spawnSync(a.path, ['ok'], { encoding: 'utf8' }).stdout, /args: ok/);
    } finally {
      await server.close();
    }
  });
});

describe('Windows archives', { skip: unix && 'needs Windows tar.exe' }, () => {
  test('downloads and unpacks the .zip release with the system tar', async () => {
    const archive = releaseArchive();
    assert.ok(archive.file.endsWith('.zip'));
    const server = await serve({ [`/v${version}/${archive.file}`]: archive.bytes });
    try {
      const cache = fresh('zip-cache');
      const env = { ...process.env, PATH: '', SONICCRAFT_BIN_CACHE: cache, SONICCRAFT_DOWNLOAD_BASE: server.base };
      const found = await resolveBinary({ version, env, release: manifestFor(archive) });
      assert.equal(found.path, join(cache, `v${version}`, archive.target, 'soniccraft.exe'));
      assert.equal(readFileSync(found.path, 'utf8'), 'not a real program');
    } finally {
      await server.close();
    }
  });
});

/** Run the launcher (synchronously) with `env` added to a minimal environment. */
function launch(args, env = {}, options = {}) {
  return spawnSync(process.execPath, [launcher, ...args], {
    encoding: 'utf8',
    env: { PATH: process.env.PATH, HOME: scratch, ...env },
    ...options,
  });
}

describe('launcher', { skip: !unix && 'uses shell-script fakes' }, () => {
  test('runs `soniccraft mcp` with the arguments and propagates the exit code', () => {
    const bin = fakeBinary(fresh('launch-bin'));
    const run = launch(['--base-dir', '/tmp/project'], { SONICCRAFT_BIN: bin, FAKE_EXIT: '3' });
    assert.equal(run.stdout, 'args: mcp --base-dir /tmp/project\n');
    assert.equal(run.stderr, '');
    assert.equal(run.status, 3);
  });

  test('--help with other arguments goes to the server', () => {
    const bin = fakeBinary(fresh('help-bin'));
    const run = launch(['--base-dir', 'x', '--help'], { SONICCRAFT_BIN: bin });
    assert.equal(run.stdout, 'args: mcp --base-dir x --help\n');
  });

  test('failures go to stderr, never stdout', () => {
    const run = launch([], { PATH: '', SONICCRAFT_BIN_CACHE: fresh('empty-cache') });
    assert.equal(run.status, 1);
    assert.equal(run.stdout, '');
    assert.match(run.stderr, /^soniccraft-mcp: /);
  });

  test('download progress goes to stderr; stdout carries only the server', async () => {
    const archive = releaseArchive();
    const path = `/v${version}/${archive.file}`;
    const server = await serve({ [path]: archive.bytes, [`${path}.sha256`]: archive.sumLine });
    try {
      const child = spawn(process.execPath, [launcher, '--base-dir', 'p'], {
        env: { PATH: '', HOME: scratch, SONICCRAFT_BIN_CACHE: fresh('stdio-cache'), SONICCRAFT_DOWNLOAD_BASE: server.base },
      });
      let stdout = '';
      let stderr = '';
      child.stdout.on('data', (d) => (stdout += d));
      child.stderr.on('data', (d) => (stderr += d));
      // `close`, not `exit`: the output may still be in the pipes when the process exits.
      const code = await new Promise((resolve) => child.on('close', resolve));
      assert.equal(stdout, 'args: mcp --base-dir p\n');
      assert.match(stderr, /soniccraft-mcp: downloading .*\nsoniccraft-mcp: installed /);
      assert.equal(code, 0);
    } finally {
      await server.close();
    }
  });

  test('--version, --help and --install', () => {
    assert.equal(launch(['--version']).stdout, `soniccraft-mcp ${version}\n`);
    const help = launch(['--help']);
    assert.match(help.stdout, /SONICCRAFT_BIN/);
    assert.match(help.stdout, /--install/);
    const bin = fakeBinary(fresh('install-bin'), 'echo ran; exit 9');
    const install = launch(['--install'], { SONICCRAFT_BIN: bin });
    assert.equal(install.stdout, `${bin}\n`, 'prints the path and does not run the server');
    assert.equal(install.status, 0);
  });

  test('SIGTERM reaches the server, and its exit status is the launcher\'s', async () => {
    const marker = join(fresh('term'), 'got-term');
    mkdirSync(dirname(marker));
    const bin = fakeBinary(dirname(marker), `trap 'echo term > "${marker}"; exit 0' TERM\necho ready\nwhile :; do sleep 0.05; done`);
    const child = spawn(process.execPath, [launcher], { env: { PATH: process.env.PATH, SONICCRAFT_BIN: bin } });
    await new Promise((resolve) => child.stdout.once('data', resolve));
    child.kill('SIGTERM');
    const [code, signal] = await new Promise((resolve) => child.on('exit', (c, s) => resolve([c, s])));
    assert.equal(readFileSync(marker, 'utf8'), 'term\n');
    assert.deepEqual([code, signal], [0, null]);
  });

  test('a server killed by a signal takes the launcher down with the same signal', () => {
    const bin = fakeBinary(fresh('killed-bin'), 'kill -TERM $$\nsleep 5');
    const run = launch([], { SONICCRAFT_BIN: bin }, { timeout: 10_000 });
    assert.equal(run.signal, 'SIGTERM');
  });

  test('signals Node ignores or uses itself become exit status 128 + n, never 0', () => {
    // Node ignores SIGPIPE, and SIGUSR1 would start its inspector instead of terminating the launcher.
    for (const signal of ['SIGPIPE', 'SIGUSR1']) {
      const bin = fakeBinary(fresh('sig-bin'), `kill -${signal.slice(3)} $$\nsleep 5`);
      const run = launch([], { SONICCRAFT_BIN: bin }, { timeout: 10_000 });
      assert.deepEqual([run.status, run.signal], [128 + constants.signals[signal], null], signal);
      assert.equal(run.stderr, '', `${signal}: nothing printed (no inspector)`);
    }
  });

  test('a binary that cannot start is reported with a hint', () => {
    const dir = fresh('no-interp');
    mkdirSync(dir);
    const bin = join(dir, 'soniccraft');
    writeFileSync(bin, '#!/nonexistent/interpreter\n');
    chmodSync(bin, 0o755);
    const run = launch([], { SONICCRAFT_BIN: bin });
    assert.equal(run.status, 1);
    assert.equal(run.stdout, '');
    assert.match(run.stderr, /cannot start .*soniccraft: spawn .* ENOENT/);
  });
});

const packageDir = join(here, '..');

/** A scratch copy of this package (without node_modules), to run the publishing scripts in. */
function packageCopy() {
  const copy = fresh('package');
  cpSync(packageDir, copy, { recursive: true, filter: (path) => !path.includes('node_modules') && !path.endsWith('release.json') });
  return copy;
}

/** `sha256sum`-style lines for every archive of `version` (Windows tools mark binary mode with `*`). */
function sumsFor(release, { skip } = {}) {
  const lines = [];
  for (const target of RELEASE_TARGETS) {
    for (const flavor of ['mp3', 'no-mp3']) {
      const file = archiveFile(release, target, flavor);
      if (file !== skip) lines.push(`${createHash('sha256').update(file).digest('hex')} ${flavor === 'mp3' ? ' ' : '*'}${file}`);
    }
  }
  return `${lines.join('\n')}\n`;
}

describe('publishing', () => {
  const node = (script, args, cwd) => spawnSync(process.execPath, [join(cwd, 'scripts', script), ...args], { cwd, encoding: 'utf8' });

  test('pin-release writes release.json and the repository URL; an incomplete SHA256SUMS writes nothing', () => {
    const copy = packageCopy();
    const sums = join(copy, 'SHA256SUMS');
    writeFileSync(sums, sumsFor(version, { skip: archiveFile(version, RELEASE_TARGETS[0], 'no-mp3') }));
    const incomplete = node('pin-release.mjs', ['--sums', sums, '--repository', 'https://github.com/acme/soniccraft'], copy);
    assert.equal(incomplete.status, 1);
    assert.match(incomplete.stderr, /is incomplete: no SHA-256 for .*-no-mp3\.tar\.gz/);
    assert.ok(!existsSync(join(copy, 'release.json')));
    assert.match(readFileSync(join(copy, 'package.json'), 'utf8'), /OWNER/);

    writeFileSync(sums, sumsFor(version));
    const pinned = node('pin-release.mjs', ['--sums', sums, '--repository', 'https://github.com/acme/soniccraft/'], copy);
    assert.equal(pinned.status, 0, pinned.stderr);
    const release = loadRelease(version, join(copy, 'release.json'));
    assert.equal(release.downloadBase, 'https://github.com/acme/soniccraft/releases/download');
    assert.equal(Object.keys(release.sha256).length, RELEASE_TARGETS.length * 2);
    const manifest = JSON.parse(readFileSync(join(copy, 'package.json'), 'utf8'));
    assert.equal(manifest.repository.url, 'git+https://github.com/acme/soniccraft.git');
    assert.ok(!JSON.stringify(manifest).includes('OWNER'));
    const readme = readFileSync(join(copy, 'README.md'), 'utf8');
    assert.ok(readme.includes('https://github.com/acme/soniccraft') && !readme.includes('/OWNER/'), 'README links the real repository');
    assert.equal(node('check-release.mjs', [], copy).status, 0, 'prepack accepts the pinned package');

    const placeholder = node('pin-release.mjs', ['--sums', sums, '--repository', 'https://github.com/OWNER/soniccraft'], copy);
    assert.equal(placeholder.status, 2);
  });

  test('prepack refuses a package without release.json', () => {
    const check = node('check-release.mjs', [], packageCopy());
    assert.equal(check.status, 1);
    assert.match(check.stderr, /cannot be packed for publishing:\n {2}- .*release\.json is missing.*\n {2}- package\.json still has the OWNER placeholder\n {2}- README\.md still has the OWNER placeholder\n$/);
  });

  const npmCli = /npm-cli\.js$/.test(process.env.npm_execpath ?? '') ? process.env.npm_execpath : null;
  test('the package ships the launcher, its release manifest and the license texts', { skip: !npmCli && 'run through npm test' }, () => {
    const copy = packageCopy();
    writeFileSync(join(copy, 'SHA256SUMS'), sumsFor(version));
    assert.equal(node('pin-release.mjs', ['--sums', join(copy, 'SHA256SUMS'), '--repository', 'https://github.com/acme/soniccraft'], copy).status, 0);
    const pack = spawnSync(process.execPath, [npmCli, 'pack', '--dry-run', '--json'], {
      cwd: copy, encoding: 'utf8', env: { ...process.env, npm_config_update_notifier: 'false' },
    });
    assert.equal(pack.status, 0, pack.stderr);
    const listing = JSON.parse(pack.stdout);
    const entry = Array.isArray(listing) ? listing[0] : Object.values(listing)[0]; // npm 10 / npm 11+
    assert.deepEqual(entry.files.map((f) => f.path).sort(), [
      'LICENSE-APACHE', 'LICENSE-MIT', 'README.md', 'bin/soniccraft-mcp.mjs', 'lib/resolve.mjs', 'package.json', 'release.json',
    ]);
  });

  test('the license texts are the repository\'s', () => {
    for (const name of ['LICENSE-MIT', 'LICENSE-APACHE']) {
      const original = join(packageDir, '..', '..', name);
      if (!existsSync(original)) continue; // outside the repository
      assert.equal(readFileSync(join(packageDir, name), 'utf8'), readFileSync(original, 'utf8'), `${name} is a copy of /${name}`);
    }
  });
});

// End to end with the real server when a built binary is available (cargo build -p soniccraft-cli, or
// SONICCRAFT_TEST_BIN=<path>).
const built = [
  process.env.SONICCRAFT_TEST_BIN,
  ...['release', 'debug'].map((profile) => join(here, '..', '..', '..', 'target', profile, unix ? 'soniccraft' : 'soniccraft.exe')),
].find((path) => path && existsSync(path));

/** Start the launcher on the real binary; resolves the first stdout line of each request. */
function realServer() {
  const child = spawn(process.execPath, [launcher], {
    env: { ...process.env, SONICCRAFT_BIN: built },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let buffered = '';
  const waiting = [];
  child.stdout.on('data', (chunk) => {
    buffered += chunk;
    let end;
    while ((end = buffered.indexOf('\n')) >= 0) {
      const line = buffered.slice(0, end);
      buffered = buffered.slice(end + 1);
      waiting.shift()?.(line);
    }
  });
  const request = (message) => {
    const reply = new Promise((resolve) => waiting.push(resolve));
    child.stdin.write(`${JSON.stringify(message)}\n`);
    return reply.then(JSON.parse);
  };
  const exited = new Promise((resolve) => child.on('exit', (code, signal) => resolve({ code, signal })));
  return { child, request, exited };
}

const INITIALIZE = {
  jsonrpc: '2.0',
  id: 1,
  method: 'initialize',
  params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'test', version: '1' } },
};

describe('with the real soniccraft', { skip: !built && 'no built soniccraft binary' }, () => {
  test('MCP initialize through the launcher; closing stdin stops the server', async () => {
    const server = realServer();
    const response = await server.request(INITIALIZE);
    assert.equal(response.id, 1);
    assert.equal(response.result.serverInfo.name, 'soniccraft');
    server.child.stdin.end();
    assert.deepEqual(await server.exited, { code: 0, signal: null });
  });

  test('SIGTERM shuts the server down cleanly', { skip: !unix && 'POSIX signals' }, async () => {
    const server = realServer();
    await server.request(INITIALIZE);
    server.child.kill('SIGTERM');
    assert.deepEqual(await server.exited, { code: 0, signal: null });
  });

  test('a usage error of the server keeps its exit status and stdout stays empty', () => {
    const run = launch(['--no-such-option'], { SONICCRAFT_BIN: built, ...(unix ? {} : { SystemRoot: process.env.SystemRoot }) });
    assert.equal(run.status, 2);
    assert.equal(run.stdout, '');
    assert.match(run.stderr, /no-such-option/);
  });
});
