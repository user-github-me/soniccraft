#!/usr/bin/env bash
# End-to-end smoke test of a built soniccraft CLI, with no API keys.
#
# Renders a timeline (default: the mock-only offline demo) twice into a temporary directory with a
# fresh TTS cache and checks that:
#   1. the render succeeds and the output file exists with the expected format;
#   2. integrated loudness is within 0.5 LU of the target and the true peak respects the ceiling;
#   3. every side file listed in the report exists;
#   4. the overlap policy pushed cue_03 after cue_02 (the README exchange);
#   5. the second render is served entirely from the cache and every output file is byte-identical.
#
# Usage: scripts/smoke-render.sh [path/to/soniccraft] [timeline]
# Requires jq.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${1:-soniccraft}"
TIMELINE="${2:-examples/offline-demo/timeline.mock.yaml}"
case "$TIMELINE" in
/*) ;;
*) TIMELINE="$ROOT/$TIMELINE" ;;
esac
command -v jq >/dev/null 2>&1 || {
  echo "error: jq is required" >&2
  exit 2
}

work="$(mktemp -d "${TMPDIR:-/tmp}/soniccraft-smoke.XXXXXX")"
trap 'rm -rf "$work"' EXIT

check() { # DESCRIPTION JQ_FILTER REPORT
  if jq -e "$2" "$3" >/dev/null; then
    echo "ok   $1"
  else
    echo "FAIL $1" >&2
    jq '{output, loudness, synthesis, diagnostics}' "$3" >&2 || true
    exit 1
  fi
}

render() { # RUN_NAME
  mkdir -p "$work/$1"
  "$BIN" render "$TIMELINE" -o "$work/$1/demo.wav" --cache-dir "$work/cache" --json >"$work/$1/report.json"
}

echo "rendering ${TIMELINE#"$ROOT"/} (run 1: fresh cache)"
render run1
echo "rendering ${TIMELINE#"$ROOT"/} (run 2: from cache)"
render run2
r1="$work/run1/report.json"
r2="$work/run2/report.json"

check "output is a 48 kHz wav" '.output.format == "wav" and .output.sample_rate == 48000 and .output.bytes > 0' "$r1"
[ -s "$work/run1/demo.wav" ] || {
  echo "FAIL output file missing" >&2
  exit 1
}
check "integrated loudness within 0.5 LU of the target" \
  '(.loudness.integrated_lufs - .loudness.target_lufs) as $d | $d < 0.5 and $d > -0.5' "$r1"
check "true peak at or below the ceiling" '.loudness.true_peak_dbtp <= .loudness.true_peak_target_dbtp + 0.1' "$r1"
check "cue_03 was pushed after cue_02" \
  '([.items[] | select(.id == "cue_03")][0].start_ms) >= ([.items[] | select(.id == "cue_02")][0].end_ms)' "$r1"
check "second render served from the cache" '.synthesis.synthesized == 0 and .synthesis.cache_hits == .synthesis.cues' "$r2"

files=("$(basename "$(jq -r '.output.path' "$r1")")")
while IFS= read -r f; do
  [ -n "$f" ] || continue
  [ -s "$f" ] || {
    echo "FAIL side file missing: $f" >&2
    exit 1
  }
  files+=("$(basename "$f")")
done < <(jq -r '(.files.subtitles // [])[].path, (.files.manifest // empty)' "$r1")
echo "ok   side files exist: ${files[*]}"

for f in "${files[@]}"; do
  cmp -s "$work/run1/$f" "$work/run2/$f" || {
    echo "FAIL $f differs between two renders (determinism)" >&2
    exit 1
  }
done
echo "ok   ${#files[@]} output files byte-identical across renders"
