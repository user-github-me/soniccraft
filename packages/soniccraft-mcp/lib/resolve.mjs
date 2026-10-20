// Locate the `soniccraft` binary that serves `soniccraft mcp`, downloading a prebuilt release when needed.
//
// Order: $SONICCRAFT_BIN, then `soniccraft` on PATH, then the per-user download cache, then a download of
// the GitHub release archive that matches this package's version. A published package carries
// `release.json` (written by .github/workflows/release.yml): the download URL and the SHA-256 of every
// archive, so a download is checked against a checksum that came from the npm registry, not from the
// server that served the archive.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  accessSync, chmodSync, constants as fsConstants, existsSync, lstatSync, mkdirSync, mkdtempSync,
  readFileSync, renameSync, rmSync, statSync, writeFileSync,
} from 'node:fs';
import { homedir } from 'node:os';
import { gunzipSync } from 'node:zlib';
import { dirname, extname, join, resolve } from 'node:path';

/** Release download base until the project's GitHub owner replaces the placeholder. */
export const DEFAULT_DOWNLOAD_BASE = 'https://github.com/OWNER/soniccraft/releases/download';

/** Largest archive accepted from a download (the release archives are 13 to 15 MB). */
export const MAX_ARCHIVE_BYTES = 128 * 1024 * 1024;

/** Largest unpacked tar accepted (the archives hold two binaries and a few text files). */
const MAX_UNPACKED_BYTES = 512 * 1024 * 1024;

/** Per-request limit for a download, so a stalled connection cannot hang an MCP client's startup. */
export const DOWNLOAD_TIMEOUT_MS = 300_000;

/** `${process.platform}-${process.arch}` → the Rust target of the release archive. */
const TARGETS = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  // Windows 11 on Arm runs x64 programs under emulation.
  'win32-arm64': 'x86_64-pc-windows-msvc',
  'win32-x64': 'x86_64-pc-windows-msvc',
};

/** The two builds of every release (see .github/workflows/release.yml). */
const FLAVORS = { mp3: '', 'no-mp3': '-no-mp3' };

/** An error whose message is meant for the user as is (no stack trace). */
export class LauncherError extends Error {}

/** The release target for a platform/arch pair, or null when no prebuilt binary exists. */
export function targetTriple(platform = process.platform, arch = process.arch) {
  return TARGETS[`${platform}-${arch}`] ?? null;
}

/** All release targets, for checking a release manifest. */
export const RELEASE_TARGETS = [...new Set(Object.values(TARGETS))].sort();

/** File name of the CLI binary. */
export function exeName(platform = process.platform) {
  return platform === 'win32' ? 'soniccraft.exe' : 'soniccraft';
}

/** Base name (without extension) of the release archive for a version, target and flavor. */
export function archiveName(version, target, flavor = 'mp3') {
  return `soniccraft-v${version}-${target}${FLAVORS[flavor]}`;
}

/** File name of the release archive: `.zip` for Windows targets, `.tar.gz` otherwise. */
export function archiveFile(version, target, flavor = 'mp3') {
  return `${archiveName(version, target, flavor)}.${target.includes('windows') ? 'zip' : 'tar.gz'}`;
}

/** Where downloaded binaries are kept: $SONICCRAFT_BIN_CACHE, else the platform's user cache directory. */
export function cacheRoot(env = process.env, platform = process.platform, home = homedir()) {
  if (env.SONICCRAFT_BIN_CACHE) return env.SONICCRAFT_BIN_CACHE;
  if (platform === 'win32') return join(env.LOCALAPPDATA || join(home, 'AppData', 'Local'), 'soniccraft', 'bin');
  if (platform === 'darwin') return join(home, 'Library', 'Caches', 'soniccraft', 'bin');
  return join(env.XDG_CACHE_HOME || join(home, '.cache'), 'soniccraft', 'bin');
}

function isFile(path) {
  try {
    return statSync(path).isFile();
  } catch {
    return false;
  }
}

function isExecutable(path, platform) {
  if (!isFile(path)) return false;
  if (platform === 'win32') return true;
  try {
    accessSync(path, fsConstants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * The first executable `name` on PATH, as an absolute path, or null. On Windows only `name.exe` counts:
 * the release binary is an .exe, and batch files or scripts found through PATHEXT cannot be started
 * without a shell.
 */
export function findOnPath(name, env = process.env, platform = process.platform) {
  const pathVar = env.PATH ?? env.Path ?? '';
  const dirs = pathVar.split(platform === 'win32' ? ';' : ':')
    .map((dir) => (platform === 'win32' ? dir.replace(/^"(.*)"$/, '$1') : dir))
    .filter(Boolean);
  const file = platform === 'win32' ? `${name}.exe` : name;
  for (const dir of dirs) {
    const candidate = resolve(dir, file);
    if (isExecutable(candidate, platform)) return candidate;
  }
  return null;
}

/** `soniccraft --version` of a binary ("0.1.0"), or null when it cannot be run. */
export function binaryVersion(path) {
  const run = spawnSync(path, ['--version'], {
    encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 10_000, windowsHide: true,
  });
  if (run.status !== 0) return null;
  return /(\d+\.\d+\.\d+\S*)/.exec(run.stdout)?.[1] ?? null;
}

/** "0.1.3" and "0.1.0" are compatible; "0.2.0" and "0.1.0" are not (minor versions may break before 1.0). */
export function compatible(a, b) {
  const [ma, na] = a.split('.');
  const [mb, nb] = b.split('.');
  return ma === mb && (ma !== '0' || na === nb);
}

/** The C library of this Linux system: 'glibc', 'musl', or null elsewhere (or when unknown). */
export function detectLibc(platform = process.platform) {
  if (platform !== 'linux') return null;
  try {
    if (process.report) process.report.excludeNetwork = true;
    const header = process.report?.getReport?.()?.header;
    if (header?.glibcVersionRuntime) return 'glibc';
  } catch {
    // fall through to the file system check
  }
  for (const loader of ['/lib/ld-musl-x86_64.so.1', '/lib/ld-musl-aarch64.so.1']) {
    if (existsSync(loader)) return 'musl';
  }
  return null;
}

/** `release.json` of this package. */
export const RELEASE_FILE = new URL('../release.json', import.meta.url);

/**
 * The release manifest shipped in the npm package (`release.json`) for `version`, or null in a
 * development checkout. A manifest that is present but incomplete is an error.
 * @returns {{version: string, downloadBase: string, sha256: Record<string, string>} | null}
 */
export function loadRelease(version, file = RELEASE_FILE) {
  let text;
  try {
    text = readFileSync(file, 'utf8');
  } catch (error) {
    if (error.code === 'ENOENT') return null;
    throw new LauncherError(`Cannot read ${file}: ${error.message}`);
  }
  let release;
  try {
    release = JSON.parse(text);
  } catch (error) {
    throw new LauncherError(`${file} is not valid JSON (${error.message}); reinstall soniccraft-mcp.`);
  }
  const problems = releaseProblems(release, version);
  if (problems.length) throw new LauncherError(`${file} is invalid: ${problems.join('; ')}; reinstall soniccraft-mcp.`);
  return release;
}

/** Everything wrong with a release manifest for `version` (empty when it is complete). */
export function releaseProblems(release, version) {
  if (!release || typeof release !== 'object') return ['it is not a JSON object'];
  const problems = [];
  if (release.version !== version) problems.push(`version is ${release.version}, expected ${version}`);
  if (typeof release.downloadBase !== 'string' || !/^https:\/\/\S+$/.test(release.downloadBase)
    || release.downloadBase.includes('/OWNER/')) {
    problems.push('downloadBase must be the https:// URL of the releases/download directory');
  }
  const sums = release.sha256 && typeof release.sha256 === 'object' ? release.sha256 : {};
  for (const target of RELEASE_TARGETS) {
    for (const flavor of Object.keys(FLAVORS)) {
      const file = archiveFile(version, target, flavor);
      if (!/^[0-9a-f]{64}$/.test(sums[file] ?? '')) problems.push(`no SHA-256 for ${file}`);
    }
  }
  return problems;
}

/**
 * Find (or download) the soniccraft binary.
 *
 * `release` is the manifest of a published package (default: `loadRelease(version)`, read only when a
 * download is needed) and `libc` the Linux C library (default: `detectLibc()`); tests choose both.
 * @returns {Promise<{path: string, source: 'env' | 'path' | 'cache' | 'download'}>}
 */
export async function resolveBinary({
  version,
  env = process.env,
  platform = process.platform,
  arch = process.arch,
  log = () => {},
  release,
  libc,
  timeoutMs = DOWNLOAD_TIMEOUT_MS,
  maxBytes = MAX_ARCHIVE_BYTES,
} = {}) {
  if (env.SONICCRAFT_BIN) {
    const bin = env.SONICCRAFT_BIN;
    if (!isFile(bin)) throw new LauncherError(`SONICCRAFT_BIN is set to ${bin}, which is not a file.`);
    if (platform === 'win32' && extname(bin).toLowerCase() !== '.exe') {
      throw new LauncherError(`SONICCRAFT_BIN must name soniccraft.exe; ${bin} cannot be started without a shell.`);
    }
    if (!isExecutable(bin, platform)) {
      throw new LauncherError(`SONICCRAFT_BIN is set to ${bin}, which is not executable (chmod +x it).`);
    }
    return { path: bin, source: 'env' };
  }

  const onPath = findOnPath('soniccraft', env, platform);
  if (onPath) {
    const found = binaryVersion(onPath);
    if (found && version && !compatible(found, version)) {
      log(`note: using ${onPath} (soniccraft ${found}); this launcher expects ${version}`);
    }
    return { path: onPath, source: 'path' };
  }

  const flavorName = env.SONICCRAFT_FLAVOR || 'mp3';
  if (!(flavorName in FLAVORS)) {
    throw new LauncherError(`SONICCRAFT_FLAVOR is \`${flavorName}\`; use \`mp3\` (the default) or \`no-mp3\`.`);
  }
  const target = targetTriple(platform, arch);
  const fromSource = 'Build it from source (cargo install --locked soniccraft), then run this again or set '
    + 'SONICCRAFT_BIN to its path.';
  if (!target) throw new LauncherError(`There is no prebuilt soniccraft for ${platform}-${arch}. ${fromSource}`);
  const dir = join(cacheRoot(env, platform), `v${version}`, `${target}${FLAVORS[flavorName]}`);
  const cached = join(dir, exeName(platform));
  if (isFile(cached)) return { path: cached, source: 'cache' };

  if (platform === 'linux' && (libc === undefined ? detectLibc(platform) : libc) === 'musl') {
    throw new LauncherError(`The prebuilt Linux binaries need glibc, and this system uses musl (e.g. Alpine). ${fromSource}`);
  }
  const manifest = release === undefined ? loadRelease(version) : release;
  const base = (env.SONICCRAFT_DOWNLOAD_BASE || manifest?.downloadBase || DEFAULT_DOWNLOAD_BASE).replace(/\/+$/, '');
  if (base.includes('/OWNER/')) {
    throw new LauncherError(
      'soniccraft was not found on PATH, and this build of soniccraft-mcp has no release download URL '
        + 'configured. Install soniccraft (cargo install --locked soniccraft), set SONICCRAFT_BIN to its path, '
        + 'or set SONICCRAFT_DOWNLOAD_BASE to a releases URL.',
    );
  }
  const file = archiveFile(version, target, flavorName);
  let expected = null;
  if (manifest) {
    expected = manifest.sha256[file];
    if (!expected) throw new LauncherError(`This soniccraft-mcp package has no checksum for ${file}; reinstall it.`);
  }
  await download({
    base, version, file, name: archiveName(version, target, flavorName), expected, dir, env, platform, log,
    timeoutMs, maxBytes,
  });
  return { path: cached, source: 'download' };
}

function describe(error) {
  if (error?.name === 'TimeoutError') return 'the download timed out';
  return error?.cause?.message ?? error?.message ?? String(error);
}

/**
 * A hint for a network failure when a proxy is configured that Node does not use: its fetch ignores
 * HTTPS_PROXY and friends unless NODE_USE_ENV_PROXY=1 (or --use-env-proxy) is set. '' otherwise.
 */
export function proxyHint(env = process.env, execArgv = process.execArgv) {
  const proxy = ['HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy'].find((name) => env[name]);
  if (!proxy) return '';
  const flag = /(^|\s)--use-env-proxy(\s|$)/;
  if (env.NODE_USE_ENV_PROXY === '1' || execArgv.includes('--use-env-proxy') || flag.test(env.NODE_OPTIONS ?? '')) {
    return '';
  }
  return ` ${proxy} is set, but Node uses it only with NODE_USE_ENV_PROXY=1 (Node versions whose \`node --help\` `
    + 'lists --use-env-proxy): set that and run this again.';
}

/** GET `url` into memory, refusing bodies over `maxBytes` and an https download that ends up on http. */
async function fetchBytes(url, { timeoutMs, maxBytes, env }) {
  const networkError = (error) => new LauncherError(`Cannot download ${url}: ${describe(error)}.${proxyHint(env)}`);
  let response;
  try {
    response = await fetch(url, { redirect: 'follow', signal: AbortSignal.timeout(timeoutMs) });
  } catch (error) {
    throw networkError(error);
  }
  const refuse = async (reason) => {
    await response.body?.cancel().catch(() => {});
    throw new LauncherError(`Cannot download ${url}: ${reason}`);
  };
  if (url.startsWith('https:') && !response.url.startsWith('https:')) {
    await refuse(`it was redirected to ${response.url}, which is not https`);
  }
  if (!response.ok) await refuse(`HTTP ${response.status}`);
  const declared = Number(response.headers.get('content-length') ?? 0);
  if (declared > maxBytes) await refuse(`${declared} bytes is more than the ${maxBytes} accepted`);
  const chunks = [];
  let total = 0;
  try {
    for await (const chunk of response.body) {
      total += chunk.length;
      if (total > maxBytes) throw new LauncherError(`Cannot download ${url}: it is larger than ${maxBytes} bytes`);
      chunks.push(chunk);
    }
  } catch (error) {
    if (error instanceof LauncherError) throw error;
    throw networkError(error);
  }
  return Buffer.concat(chunks, total);
}

/** The SHA-256 named in a `sha256sum`-style file for `file`, or null. */
function parseChecksum(text, file) {
  const [hash, name] = text.trim().split(/\s+/);
  if (!/^[0-9a-fA-F]{64}$/.test(hash ?? '')) return null;
  if (name && name.replace(/^\*/, '') !== file) return null;
  return hash.toLowerCase();
}

/** The tar used to unpack archives: Windows' own bsdtar (it reads .zip), never one found on PATH there. */
function tarCommand(platform, env) {
  if (platform !== 'win32') return existsSync('/usr/bin/tar') ? '/usr/bin/tar' : 'tar';
  const tar = join(env.SystemRoot || env.windir || 'C:\\Windows', 'System32', 'tar.exe');
  if (!isFile(tar)) {
    throw new LauncherError(`${tar} was not found (it ships with Windows 10 1803 and later); unpack the release `
      + 'archive yourself and set SONICCRAFT_BIN to soniccraft.exe.');
  }
  return tar;
}

function renameWithRetry(from, to, platform) {
  // Windows: a virus scanner may still hold the freshly unpacked .exe for a moment.
  for (let attempt = 0; ; attempt += 1) {
    try {
      renameSync(from, to);
      return;
    } catch (error) {
      if (platform !== 'win32' || attempt >= 10 || !['EPERM', 'EACCES', 'EBUSY'].includes(error.code)) throw error;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100 * (attempt + 1));
    }
  }
}

/** Download, verify and unpack one release archive into `dir` (atomically: a rename of a staged copy). */
async function download({ base, version, file, name, expected, dir, env, platform, log, timeoutMs, maxBytes }) {
  const url = `${base}/v${version}/${file}`;
  log(`downloading ${url}`);
  const archive = await fetchBytes(url, { timeoutMs, maxBytes, env });
  let want = expected;
  if (!want) {
    // No pinned checksum (development build pointed at SONICCRAFT_DOWNLOAD_BASE): use the published file.
    const sums = await fetchBytes(`${url}.sha256`, { timeoutMs, maxBytes: 4096, env });
    want = parseChecksum(sums.toString('utf8'), file);
    if (!want) throw new LauncherError(`${url}.sha256 is not a SHA-256 checksum file for ${file}.`);
  }
  const actual = createHash('sha256').update(archive).digest('hex');
  if (actual !== want) {
    throw new LauncherError(`Checksum mismatch for ${url} (expected ${want}, got ${actual}); nothing was installed.`);
  }

  const parent = dirname(dir);
  mkdirSync(parent, { recursive: true });
  const staging = mkdtempSync(join(parent, '.staging-'));
  try {
    // A .tar.gz is decompressed here: GNU tar's -z runs an external `gzip` found on PATH, and the
    // environment an MCP client starts the launcher with may have no PATH at all.
    const gzipped = file.endsWith('.tar.gz');
    const local = join(staging, gzipped ? file.slice(0, -'.gz'.length) : file);
    let contents = archive;
    if (gzipped) {
      try {
        contents = gunzipSync(archive, { maxOutputLength: MAX_UNPACKED_BYTES });
      } catch (error) {
        throw new LauncherError(`Cannot unpack ${file}: ${error.message}`);
      }
    }
    writeFileSync(local, contents);
    // The archive's checksum is verified; tar (bsdtar or GNU tar) also refuses absolute and `..` members.
    // --no-same-owner: run as root (e.g. in a container), tar would otherwise keep the archive's uid/gid.
    const untar = spawnSync(tarCommand(platform, env), ['--no-same-owner', '-xf', local, '-C', staging], {
      encoding: 'utf8', stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true,
    });
    if (untar.error || untar.status !== 0) {
      throw new LauncherError(`Cannot unpack ${file}: ${untar.error?.message ?? untar.stderr.trim()}`);
    }
    const unpacked = join(staging, name);
    const exe = exeName(platform);
    let entry;
    try {
      entry = lstatSync(join(unpacked, exe));
    } catch {
      entry = null;
    }
    if (!entry?.isFile()) throw new LauncherError(`${file} does not contain the file ${name}/${exe}.`);
    if (platform !== 'win32') {
      for (const bin of ['soniccraft', 'soniccraft-mcp']) {
        if (existsSync(join(unpacked, bin))) chmodSync(join(unpacked, bin), 0o755);
      }
    }
    try {
      renameWithRetry(unpacked, dir, platform);
    } catch (error) {
      // Another launcher finished the same download first.
      if (!isFile(join(dir, exe))) {
        throw new LauncherError(`Cannot install into ${dir}: ${error.message}. Remove that directory and run this again.`);
      }
    }
    log(`installed ${join(dir, exe)}`);
  } finally {
    rmSync(staging, { recursive: true, force: true });
  }
}
