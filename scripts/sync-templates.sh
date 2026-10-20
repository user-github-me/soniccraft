#!/usr/bin/env bash
# Regenerate the Agent Skill templates that mirror runnable examples, so the two can never drift:
#
#   examples/<name>/timeline.yaml  ->  skills/soniccraft/reference/templates/<name>.yaml
#
# for <name> in tech-briefing, two-person-podcast and offline-demo (`soniccraft init --template`
# choices). The template is the example with its editor modeline replaced by a template header. Its
# `source:` paths stay relative to the timeline (`assets/<audio|sfx>/<file>`): `soniccraft init` writes
# generated stand-ins for those assets at exactly those paths, and an installed skill has no repository
# around it to point into. scripts/check-timelines.sh validates each template inside a directory
# prepared by `soniccraft init`.
#
# The CLI crate (package `soniccraft`) vendors byte-identical copies in crates/soniccraft-cli/templates/
# (a test enforces it): copy the regenerated files there too.
#
# Edit the example, then run this script. The other templates (voiceover-with-music.yaml,
# dialogue-crosstalk.yaml) are hand-written and not touched.
#
# Usage:
#   scripts/sync-templates.sh          rewrite the generated templates
#   scripts/sync-templates.sh --check  exit 1 if any generated template is out of date (CI)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMPLATES="$ROOT/skills/soniccraft/reference/templates"
NAMES=(tech-briefing two-person-podcast offline-demo)

render_template() { # NAME -> stdout
  local name="$1" src="$ROOT/examples/$1/timeline.yaml"
  [ -f "$src" ] || {
    echo "error: $src not found" >&2
    exit 1
  }
  cat <<EOF
# SonicCraft template "$name".
# \`source:\` paths are relative to this file. \`soniccraft init --template $name\` writes the timeline
# together with generated, royalty-free stand-ins for those assets; when you copy the file by hand, put
# audio files at those paths (or point \`source:\` elsewhere, or delete the music/sfx tracks).
# (Maintainers: generated from examples/$name/timeline.yaml by scripts/sync-templates.sh.)
EOF
  # Drop the editor modeline (first line).
  sed -e '1{/^# yaml-language-server:/d;}' "$src"
}

main() {
  local mode="${1:-write}" name out tmp status=0
  case "$mode" in
  write | --check) ;;
  -h | --help)
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
    return 0
    ;;
  *)
    echo "error: unknown argument '$mode' (try --help)" >&2
    return 2
    ;;
  esac
  mkdir -p "$TEMPLATES"
  for name in "${NAMES[@]}"; do
    out="$TEMPLATES/$name.yaml"
    tmp="$(mktemp)"
    render_template "$name" >"$tmp"
    if [ "$mode" = "--check" ]; then
      if ! cmp -s "$tmp" "$out"; then
        echo "out of date: ${out#"$ROOT"/} (run scripts/sync-templates.sh)" >&2
        diff -u "$out" "$tmp" >&2 || true
        status=1
      fi
      rm -f "$tmp"
    else
      mv "$tmp" "$out"
      echo "wrote ${out#"$ROOT"/}" >&2
    fi
  done
  return "$status"
}

main "$@"
