#!/usr/bin/env bash
# Check the Agent Skill package in skills/soniccraft/ against the Agent Skills specification
# (agentskills.io) and the portability rules SonicCraft relies on:
#
#   - skills/soniccraft/SKILL.md exists and there is no SKILL.md at the repository root;
#   - frontmatter uses only the six portable keys (name, description, license, compatibility, metadata,
#     allowed-tools); `name` equals the directory name and is a valid skill name;
#   - `description` is a single line of 1-1024 characters (and safe as a plain YAML scalar);
#     `compatibility` is at most 500 characters;
#   - the body is under 500 lines;
#   - every relative Markdown link in the skill resolves to a file, and the expected references,
#     templates and the Codex sidecar exist.
#
# Usage: scripts/check-skill.sh        (exit 1 on the first class of failure, listing every problem)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SKILL_DIR="$ROOT/skills/soniccraft"
SKILL="$SKILL_DIR/SKILL.md"
errors=0

fail() {
  echo "error: $*" >&2
  errors=$((errors + 1))
}

[ -f "$SKILL" ] || {
  echo "error: $SKILL not found" >&2
  exit 1
}
[ ! -e "$ROOT/SKILL.md" ] || fail "SKILL.md at the repository root would short-circuit skill discovery; remove it"

# --- frontmatter -----------------------------------------------------------------------------------
[ "$(head -n 1 "$SKILL")" = "---" ] || fail "SKILL.md must start with a '---' frontmatter line"
end_line="$(awk 'NR > 1 && /^---[[:space:]]*$/ { print NR; exit }' "$SKILL")"
[ -n "$end_line" ] || {
  echo "error: unterminated frontmatter in SKILL.md" >&2
  exit 1
}
frontmatter="$(sed -n "2,$((end_line - 1))p" "$SKILL")"

allowed=" name description license compatibility metadata allowed-tools "
while IFS= read -r key; do
  case "$allowed" in
  *" $key "*) ;;
  *) fail "frontmatter key '$key' is not one of the portable keys:$allowed" ;;
  esac
done < <(printf '%s\n' "$frontmatter" | sed -n 's/^\([A-Za-z0-9_-]*\):.*/\1/p')

value_of() { # KEY -> the raw single-line value (empty if absent)
  printf '%s\n' "$frontmatter" | sed -n "s/^$1:[[:space:]]*//p" | head -n 1
}

name="$(value_of name)"
dir_name="$(basename "$SKILL_DIR")"
[ "$name" = "$dir_name" ] || fail "name '$name' must equal the directory name '$dir_name'"
printf '%s' "$name" | grep -Eq '^[a-z0-9]+(-[a-z0-9]+)*$' || fail "name '$name' must be lowercase a-z, 0-9 and single hyphens"
[ "${#name}" -le 64 ] || fail "name is longer than 64 characters"

description="$(value_of description)"
case "$description" in
"" | "|"* | ">"*) fail "description must be a non-empty single-line value" ;;
esac
desc_chars="$(printf '%s' "$description" | wc -m | tr -d ' ')"
[ "$desc_chars" -le 1024 ] || fail "description is $desc_chars characters (max 1024)"
case "$description" in
\"* | \'*) ;;
*": "* | *" #"*) fail "unquoted description contains ': ' or ' #', which breaks YAML; quote it or rephrase" ;;
esac

compatibility="$(value_of compatibility)"
if [ -n "$compatibility" ]; then
  compat_chars="$(printf '%s' "$compatibility" | wc -m | tr -d ' ')"
  [ "$compat_chars" -le 500 ] || fail "compatibility is $compat_chars characters (max 500)"
fi

# --- body ------------------------------------------------------------------------------------------
total_lines="$(wc -l <"$SKILL" | tr -d ' ')"
body_lines=$((total_lines - end_line))
[ "$body_lines" -lt 500 ] || fail "SKILL.md body is $body_lines lines (keep it under 500; move detail to reference/)"

# --- required files --------------------------------------------------------------------------------
for f in reference/dsl-reference.md reference/providers.md reference/mixing-guide.md \
  reference/diagnostics.md reference/mcp-tools.md reference/timeline-schema.json \
  reference/templates/tech-briefing.yaml reference/templates/two-person-podcast.yaml \
  reference/templates/voiceover-with-music.yaml reference/templates/offline-demo.yaml \
  reference/templates/dialogue-crosstalk.yaml agents/openai.yaml; do
  [ -f "$SKILL_DIR/$f" ] || fail "missing skills/soniccraft/$f"
done

# --- relative links --------------------------------------------------------------------------------
while IFS= read -r md; do
  while IFS= read -r target; do
    case "$target" in
    http://* | https://* | mailto:* | \#*) continue ;;
    esac
    path="${target%%#*}"
    [ -n "$path" ] || continue
    [ -e "$(dirname "$md")/$path" ] || fail "${md#"$ROOT"/}: broken link '$target'"
  done < <(grep -o '\](\([^)]*\))' "$md" | sed 's/^](\(.*\))$/\1/' || true)
done < <(find "$SKILL_DIR" -name '*.md' | sort)

if [ "$errors" -gt 0 ]; then
  echo "skill check failed with $errors problem(s)" >&2
  exit 1
fi
echo "skills/soniccraft: ok (description $desc_chars chars, body $body_lines lines)"
