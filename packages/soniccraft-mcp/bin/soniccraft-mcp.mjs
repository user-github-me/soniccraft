#!/usr/bin/env node
// soniccraft-mcp: run the SonicCraft MCP server (`soniccraft mcp`) on stdin/stdout.
//
// stdout belongs to the MCP protocol, so everything this launcher says goes to stderr, and the server
// inherits stdin, stdout and stderr untouched. Arguments are passed through to `soniccraft mcp` (e.g.
// `--base-dir DIR`); only a single `--help`, `--version` or `--install` argument is the launcher's own.

import { spawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { constants } from 'node:os';
import { LauncherError, resolveBinary } from '../lib/resolve.mjs';

const { version } = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'));
const WINDOWS = process.platform === 'win32';

/**
 * Signals the launcher re-raises on itself when the server died from one. Node ignores or uses some
 * others (SIGPIPE, SIGUSR1 starts the inspector) and crash signals would dump core: for those the
 * launcher exits with status 128 + n, as a shell reports them.
 */
const RERAISE = new Set(['SIGHUP', 'SIGINT', 'SIGTERM', 'SIGKILL']);

const USAGE = `soniccraft-mcp ${version}: the SonicCraft MCP server (runs \`soniccraft mcp\` over stdio)

Usage: soniccraft-mcp [--base-dir DIR]     serve MCP on stdin/stdout (options go to \`soniccraft mcp\`)
       soniccraft-mcp --install            download soniccraft if needed and print its path
       soniccraft-mcp --version | --help

Finds soniccraft via $SONICCRAFT_BIN, then PATH, then a downloaded release in the user cache, and
otherwise downloads the release that matches this version (checked against its pinned SHA-256).

Environment:
  SONICCRAFT_BIN            path of the soniccraft binary to run
  SONICCRAFT_BIN_CACHE      where downloaded binaries are kept
  SONICCRAFT_DOWNLOAD_BASE  release download base URL (a mirror of …/releases/download)
  SONICCRAFT_FLAVOR         \`no-mp3\` downloads the build without the LGPL MP3 encoder
`;

function say(message) {
  process.stderr.write(`soniccraft-mcp: ${message}\n`);
}

/** Report `message` and exit with status 1 once stderr has flushed. */
function fail(message) {
  process.exitCode = 1;
  process.stderr.write(`soniccraft-mcp: ${message}\n`, () => process.exit(1));
}

/** What a failed spawn most likely means, as a hint. */
function spawnHint(error, path) {
  switch (error.code) {
    case 'ENOENT':
      // The file exists (it was resolved), so what is missing is its program interpreter.
      return process.platform === 'linux'
        ? ' (the prebuilt Linux binaries need glibc; on musl systems such as Alpine, build soniccraft from source)'
        : '';
    case 'EACCES':
      return ` (make it executable: chmod +x ${path})`;
    case 'ENOEXEC':
    case 'EBADARCH':
      return ` (it was built for another platform; remove it or set SONICCRAFT_BIN)`;
    default:
      return '';
  }
}

/** Run the server; forward termination signals; exit the way it exits. */
function serve(path, args) {
  const child = spawn(path, ['mcp', ...args], { stdio: 'inherit', windowsHide: true });
  let interrupts = 0;
  const onSignal = (signal) => {
    if (!WINDOWS) {
      // The server shuts down cleanly on SIGINT and SIGTERM (cancelling renders in progress).
      child.kill(signal);
      return;
    }
    // Windows: Ctrl+C reaches the server through the shared console already, and child.kill() would
    // terminate it without cleanup. Wait for it to exit; a second Ctrl+C terminates it.
    interrupts += 1;
    if (signal === 'SIGHUP' || interrupts > 1) child.kill();
  };
  const signals = WINDOWS ? ['SIGINT', 'SIGBREAK', 'SIGHUP'] : ['SIGINT', 'SIGTERM', 'SIGHUP'];
  for (const signal of signals) process.on(signal, onSignal);

  let failed = false;
  child.on('error', (error) => {
    failed = true;
    fail(`cannot start ${path}: ${error.message}${spawnHint(error, path)}`);
  });
  child.on('exit', (code, signal) => {
    for (const s of signals) process.off(s, onSignal);
    if (failed) return;
    if (signal && !WINDOWS) {
      const status = 128 + (constants.signals[signal] ?? 0);
      if (RERAISE.has(signal)) {
        // Die from the same signal, so the MCP client sees why the server stopped.
        process.kill(process.pid, signal);
        setTimeout(() => process.exit(status), 1000);
        return;
      }
      process.exit(status);
    }
    process.exit(code ?? 1);
  });
}

async function main() {
  const args = process.argv.slice(2);
  const only = args.length === 1 ? args[0] : null;
  if (only === '--help' || only === '-h') {
    process.stdout.write(USAGE);
    return;
  }
  if (only === '--version' || only === '-V') {
    process.stdout.write(`soniccraft-mcp ${version}\n`);
    return;
  }
  let binary;
  try {
    binary = await resolveBinary({ version, log: say });
  } catch (error) {
    fail(error instanceof LauncherError ? error.message : String(error?.stack ?? error));
    return;
  }
  if (only === '--install') {
    process.stdout.write(`${binary.path}\n`);
    return;
  }
  serve(binary.path, args);
}

main().catch((error) => fail(String(error?.stack ?? error)));
