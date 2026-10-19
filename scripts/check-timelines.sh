#!/usr/bin/env bash
# Validate every example and Agent Skill template with a built soniccraft CLI.
#
# Each file must validate without errors, except environment errors that depend on the machine:
# `missing_credentials` (no API keys in CI) and `provider_unavailable` (e.g. macOS say on Linux, no local
# Kokoro server). Timelines that need nothing from the environment must be fully valid, including in the
# dry run: the mock-only demo everywhere, and the macOS say demo on macOS.
#
# Examples are validated in place. Templates name their assets relative to themselves
# (`assets/audio/...`) and ship without them, so each template is copied into a directory prepared by
# `soniccraft init --template <name>` (which writes generated stand-ins at exactly those paths) and
# validated there. This also checks that every template is an `init` template.
#
# Usage: scripts/check-timelines.sh [path/to/soniccraft]     (default: soniccraft on PATH)
# Requires jq.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-soniccraft}"
command -v jq >/dev/null 2>&1 || {
  echo "error: jq is required" >&2
  exit 2
}
command -v "$BIN" >/dev/null 2>&1 || [ -x "$BIN" ] || {
  echo "error: soniccraft binary '$BIN' not found" >&2
  exit 2
}
case "$BIN" in
*/*) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;;
esac

TOLERATED='["missing_credentials", "provider_unavailable"]'
STRICT=("examples/offline-demo/timeline.mock.yaml")
if [ "$(uname -s)" = "Darwin" ]; then
  STRICT+=("examples/offline-demo/timeline.yaml" "skills/soniccraft/reference/templates/offline-demo.yaml")
fi

is_strict() {
  local f
  for f in "${STRICT[@]}"; do
    [ "$f" = "$1" ] && return 0
  done
  return 1
}

scratch="$(mktemp -d "${TMPDIR:-/tmp}/soniccraft-timelines.XXXXXX")"
trap 'rm -rf "$scratch"' EXIT

# run_json SUBCOMMAND FILE -> stdout: the JSON report; fails on exit codes other than 0 and 1
run_json() {
  local out code=0
  out="$("$BIN" "$1" "$2" --json)" || code=$?
  if [ "$code" -gt 1 ]; then
    echo "error: 'soniccraft $1 $2 --json' exited with $code" >&2
    return 1
  fi
  printf '%s' "$out"
}

# prepare_template REL -> stdout: the path of a copy of the template next to `init`-generated assets
prepare_template() {
  local name dir
  name="$(basename "$1" .yaml)"
  dir="$scratch/$name"
  if ! "$BIN" init "$dir" --template "$name" -q >/dev/null; then
    echo "error: 'soniccraft init --template $name' failed (every template must be an init template)" >&2
    return 1
  fi
  cp "$ROOT/$1" "$dir/template.yaml"
  printf '%s' "$dir/template.yaml"
}

status=0
shopt -s nullglob
files=("$ROOT"/examples/*/timeline*.yaml "$ROOT"/skills/soniccraft/reference/templates/*.yaml)
[ "${#files[@]}" -gt 0 ] || {
  echo "error: no timelines found" >&2
  exit 1
}

for abs in "${files[@]}"; do
  rel="${abs#"$ROOT"/}"
  target="$abs"
  case "$rel" in
  skills/*)
    if ! target="$(prepare_template "$rel")"; then
      echo "FAIL $rel"
      status=1
      continue
    fi
    ;;
  esac
  if ! report="$(run_json validate "$target")"; then
    status=1
    continue
  fi
  if is_strict "$rel"; then
    unexpected="$(jq -r '.diagnostics[] | select(.severity == "error") | "\(.code) \(.path // "-"): \(.message)"' <<<"$report")"
  else
    unexpected="$(jq -r --argjson ok "$TOLERATED" \
      '.diagnostics[] | select(.severity == "error") | select(.code as $c | ($ok | index($c)) == null) | "\(.code) \(.path // "-"): \(.message)"' <<<"$report")"
  fi
  if [ -n "$unexpected" ]; then
    echo "FAIL $rel"
    printf '%s\n' "$unexpected" | sed 's/^/       /'
    status=1
    continue
  fi
  tolerated="$(jq '[.diagnostics[] | select(.severity == "error")] | length' <<<"$report")"
  if is_strict "$rel"; then
    if ! plan="$(run_json plan "$target")" || ! jq -e '.valid == true and (.items | length) > 0' <<<"$plan" >/dev/null; then
      echo "FAIL $rel: dry run is not valid"
      status=1
      continue
    fi
    echo "ok   $rel (strict: validate + plan clean)"
  else
    echo "ok   $rel ($tolerated environment error(s) tolerated)"
  fi
done

exit "$status"
