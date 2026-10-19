#!/usr/bin/env python3
"""Check the documentation for drift against the code (no build needed).

1. Every diagnostic code constant in crates/soniccraft-core/src/diagnostics.rs appears in docs/DESIGN.md §7
   and as a table row (`| `code` |`) in skills/soniccraft/reference/diagnostics.md, and that table lists no
   code that does not exist.
2. Every relative Markdown link in the repository's Markdown files resolves, including `#anchors` (GitHub
   heading slugs).

Usage: scripts/check-docs.py        (exit 1 listing every problem)
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKIP_DIRS = {"target", "node_modules", ".git"}
problems = []


def read(rel):
    with open(os.path.join(ROOT, rel), encoding="utf-8") as f:
        return f.read()


def check_codes():
    codes = re.findall(r'pub const [A-Z_]+: &str = "([a-z_]+)";', read("crates/soniccraft-core/src/diagnostics.rs"))
    design = read("docs/DESIGN.md")
    try:
        sec7 = design[design.index("## 7. Diagnostics"):design.index("## 8. Reports")]
    except ValueError:
        problems.append("docs/DESIGN.md: sections `## 7. Diagnostics` / `## 8. Reports` not found")
        return
    rows = set(re.findall(r"^\| `([a-z_]+)` \|", read("skills/soniccraft/reference/diagnostics.md"), re.M))
    for code in codes:
        if f"`{code}`" not in sec7:
            problems.append(f"docs/DESIGN.md §7: code `{code}` is not documented")
        if code not in rows:
            problems.append(f"skills/soniccraft/reference/diagnostics.md: no table row for `{code}`")
    for row in sorted(rows - set(codes)):
        problems.append(f"skills/soniccraft/reference/diagnostics.md: row `{row}` is not a diagnostic code")
    return len(codes)


def strip_code(text):
    text = re.sub(r"```.*?```", "", text, flags=re.S)
    return re.sub(r"`[^`\n]*`", "", text)


def slugs(path):
    out, seen = set(), {}
    with open(path, encoding="utf-8") as f:
        text = re.sub(r"```.*?```", "", f.read(), flags=re.S)
    for heading in re.findall(r"^#{1,6} (.+?)\s*#*\s*$", text, re.M):
        slug = re.sub(r"[^\w\- ]", "", heading.lower().replace("`", "")).replace(" ", "-")
        n = seen.get(slug, 0)
        seen[slug] = n + 1
        out.add(slug if n == 0 else f"{slug}-{n}")
    return out


def markdown_files():
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            if name.endswith(".md"):
                yield os.path.join(dirpath, name)


def check_links():
    count = 0
    for path in sorted(markdown_files()):
        rel = os.path.relpath(path, ROOT)
        with open(path, encoding="utf-8") as f:
            text = strip_code(f.read())
        for target in re.findall(r"\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)", text):
            if re.match(r"[a-z][a-z0-9+.-]*:", target):
                continue  # https:, mailto:, …
            count += 1
            file_part, _, anchor = target.partition("#")
            dest = os.path.normpath(os.path.join(os.path.dirname(path), file_part)) if file_part else path
            if not os.path.exists(dest):
                problems.append(f"{rel}: broken link `{target}`")
            elif anchor and dest.endswith(".md") and anchor not in slugs(dest):
                problems.append(f"{rel}: link `{target}` names a heading that does not exist")
    return count


codes = check_codes()
links = check_links()
if problems:
    for p in problems:
        print(f"error: {p}", file=sys.stderr)
    print(f"{len(problems)} documentation problem(s)", file=sys.stderr)
    sys.exit(1)
print(f"docs: ok ({codes} diagnostic codes documented, {links} relative links resolve)")
