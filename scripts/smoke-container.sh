#!/usr/bin/env bash
# Smoke-test a SonicCraft container image (see Dockerfile):
#
#   1. `soniccraft --version` runs;
#   2. the mock-only offline demo renders to WAV and to Ogg (ffmpeg inside the image) into a mounted
#      directory, with a valid --json report and loudness near the target;
#   3. `soniccraft mcp` answers an MCP initialize request on stdin.
#
# Usage: scripts/smoke-container.sh IMAGE        (needs docker and python3)

set -euo pipefail

IMAGE="${1:?usage: scripts/smoke-container.sh IMAGE}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/soniccraft-container.XXXXXX")"
trap 'rm -rf "$work"' EXIT

version="$(docker run --rm "$IMAGE" --version)"
echo "ok   $version"

cp "$ROOT/examples/offline-demo/timeline.mock.yaml" "$work/"
cp -R "$ROOT/examples/offline-demo/assets" "$work/assets"
# Write as the calling user, so the files are ours (the image's default user is uid 1000).
run=(docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -v "$work:/work" "$IMAGE")

"${run[@]}" render timeline.mock.yaml -o demo.wav --json >"$work/report.json"
python3 - "$work/report.json" "$work/demo.wav" <<'EOF'
import json, os, sys
report = json.load(open(sys.argv[1]))
assert report["output"]["format"] == "wav", report["output"]
assert os.path.getsize(sys.argv[2]) == report["output"]["bytes"] > 0
lufs, target = report["loudness"]["integrated_lufs"], report["loudness"]["target_lufs"]
assert abs(lufs - target) < 0.5, (lufs, target)
print(f"ok   mock demo rendered: {report['output']['duration_ms'] / 1000:.2f} s at {lufs:.1f} LUFS")
EOF

"${run[@]}" render timeline.mock.yaml -o demo.ogg --format ogg -q >/dev/null
[ -s "$work/demo.ogg" ] || {
  echo "FAIL no demo.ogg" >&2
  exit 1
}
echo "ok   ogg through the image's ffmpeg ($(wc -c <"$work/demo.ogg" | tr -d ' ') bytes)"

reply="$(printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}' |
  docker run -i --rm "$IMAGE" mcp)"
python3 -c 'import json, sys; r = json.loads(sys.argv[1].splitlines()[0]); assert r["result"]["serverInfo"]["name"] == "soniccraft", r; print("ok   MCP initialize:", r["result"]["protocolVersion"])' "$reply"
