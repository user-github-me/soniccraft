#!/bin/sh
# Install the SonicCraft CLI (`soniccraft` and `soniccraft-mcp`) from a GitHub release on macOS, Linux and
# Windows (Git Bash):
#
#   curl -fsSL https://raw.githubusercontent.com/user-github-me/soniccraft/main/install.sh | sh
#
# Environment variables:
#   SONICCRAFT_VERSION      the release to install, e.g. 0.1.0 (default: the latest release)
#   SONICCRAFT_INSTALL_DIR  where to put the binaries (default: ~/.local/bin)
#   SONICCRAFT_NO_MP3=1     use the archive without the LGPL MP3 encoder (mp3 output then needs ffmpeg)
#
# The archive is checked against the SHA-256 published with the release before anything is installed.
set -eu

REPO=user-github-me/soniccraft
INSTALL_DIR=${SONICCRAFT_INSTALL_DIR:-$HOME/.local/bin}

say() { printf 'soniccraft-install: %s\n' "$*" >&2; }
fail() {
  say "error: $*"
  exit 1
}

command -v curl >/dev/null 2>&1 || fail "curl is required"

exe=
case $(uname -s) in
Darwin) os=apple-darwin ;;
Linux)
  os=unknown-linux-gnu
  if ls /lib/ld-musl-* >/dev/null 2>&1; then
    fail "the prebuilt binaries need glibc, not musl (Alpine): use the container image ghcr.io/$REPO or 'cargo install --locked soniccraft'"
  fi
  ;;
MINGW* | MSYS* | CYGWIN*)
  os=pc-windows-msvc
  exe=.exe
  ;;
*) fail "no prebuilt binary for $(uname -s): build from source with 'cargo install --locked soniccraft'" ;;
esac
case $(uname -m) in
x86_64 | amd64) arch=x86_64 ;;
arm64 | aarch64) arch=aarch64 ;;
*) fail "no prebuilt binary for the $(uname -m) CPU: build from source with 'cargo install --locked soniccraft'" ;;
esac
# A shell running under Rosetta reports x86_64 on Apple silicon: install the native build instead.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
  arch=aarch64
fi
# The Windows build is x86_64 only (Windows on Arm runs it under emulation).
if [ "$os" = pc-windows-msvc ]; then arch=x86_64; fi

version=${SONICCRAFT_VERSION:-}
if [ -z "$version" ]; then
  # github.com/<repo>/releases/latest redirects to .../releases/tag/v<version>
  url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
    fail "cannot reach github.com"
  case $url in
  */releases/tag/v*) version=${url##*/releases/tag/v} ;;
  *) fail "cannot find the latest release of $REPO (got $url)" ;;
  esac
fi
version=${version#v}

ext=tar.gz
if [ "$os" = pc-windows-msvc ]; then ext=zip; fi
name=soniccraft-v$version-$arch-$os
if [ "${SONICCRAFT_NO_MP3:-}" = 1 ]; then name=$name-no-mp3; fi
base=https://github.com/$REPO/releases/download/v$version

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t soniccraft)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' HUP INT TERM

say "downloading $name.$ext"
curl -fsSL --retry 3 -o "$tmp/$name.$ext" "$base/$name.$ext" || fail "download failed: $base/$name.$ext"
curl -fsSL --retry 3 -o "$tmp/$name.$ext.sha256" "$base/$name.$ext.sha256" || fail "download failed: $base/$name.$ext.sha256"
want=$(cut -d ' ' -f 1 <"$tmp/$name.$ext.sha256" | tr -d '\r' | tr 'A-F' 'a-f')
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name.$ext" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp/$name.$ext" | cut -d ' ' -f 1)
else
  fail "sha256sum or shasum is required to verify the download"
fi
[ "$want" = "$got" ] || fail "checksum mismatch for $name.$ext: expected $want, got $got"

mkdir "$tmp/x"
if [ "$ext" = zip ]; then
  command -v unzip >/dev/null 2>&1 || fail "unzip is required"
  unzip -q "$tmp/$name.$ext" -d "$tmp/x"
else
  tar -xzf "$tmp/$name.$ext" -C "$tmp/x"
fi
set -- "$tmp"/x/*/soniccraft$exe
[ -f "$1" ] || fail "soniccraft$exe is missing from $name.$ext"
src=${1%/*}

mkdir -p "$INSTALL_DIR"
for bin in soniccraft soniccraft-mcp; do
  # Copy next to the target, then rename over it: a running MCP server keeps its old file.
  cp "$src/$bin$exe" "$INSTALL_DIR/.$bin$exe.new"
  chmod 755 "$INSTALL_DIR/.$bin$exe.new"
  mv -f "$INSTALL_DIR/.$bin$exe.new" "$INSTALL_DIR/$bin$exe"
done
installed=$("$INSTALL_DIR/soniccraft$exe" --version 2>/dev/null) ||
  fail "$INSTALL_DIR/soniccraft$exe does not run here (the Linux builds need glibc 2.35 or newer): try 'cargo install --locked soniccraft'"
say "installed $installed to $INSTALL_DIR"

case ":$PATH:" in
*":$INSTALL_DIR:"*) ;;
*) say "$INSTALL_DIR is not on PATH: add it, e.g. export PATH=\"$INSTALL_DIR:\$PATH\"" ;;
esac
command -v ffmpeg >/dev/null 2>&1 || say "optional: install ffmpeg for ogg, opus and m4a output"
say "try it: soniccraft init demo && soniccraft render demo/timeline.yaml --preview"
