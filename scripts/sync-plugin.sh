#!/usr/bin/env bash
# Copy the Agent Skill into the Claude Code plugin, so the plugin directory holds only what the plugin ships
# (its manifest and icon, the MCP server config and the skill) and not the rest of the repository:
#
#   skills/soniccraft/  ->  plugin/skills/soniccraft/
#
# skills/soniccraft/ is the source: `npx skills add` and the schema's `$id` URL use that path. Edit it, then
# run this script.
#
# Usage:
#   scripts/sync-plugin.sh          rewrite plugin/skills/soniccraft
#   scripts/sync-plugin.sh --check  exit 1 if the plugin's copy is out of date (CI)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/skills/soniccraft"
DST="$ROOT/plugin/skills/soniccraft"

if [ "${1:-}" = --check ]; then
  if ! diff -r "$SRC" "$DST" >/dev/null 2>&1; then
    diff -rq "$SRC" "$DST" || true
    echo "error: plugin/skills/soniccraft is out of date: run scripts/sync-plugin.sh" >&2
    exit 1
  fi
  echo "plugin/skills/soniccraft: in sync with skills/soniccraft"
  exit 0
fi

rm -rf "$DST"
mkdir -p "$(dirname "$DST")"
cp -R "$SRC" "$DST"
echo "wrote plugin/skills/soniccraft"
