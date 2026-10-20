#!/usr/bin/env python3
"""Write THIRD-PARTY-LICENSES.txt for a soniccraft binary build: every crate compiled into it, with the
license texts those crates ship.

The crate set is what cargo really builds for one target and feature selection (`cargo tree -e normal`,
so build scripts' and tests' dependencies are left out; procedural macros are kept). License texts come
from the crates' sources in the cargo registry (`cargo metadata` downloads them): LICENSE*/COPYING*/
NOTICE*/COPYRIGHT*/UNLICENSE* files, the manifest's `license-file`, files in a `licenses/` directory,
and license files of vendored code in subdirectories. Identical texts are printed once. A crate that
ships no license file gets the repository's canonical MIT (with the crate's authors) or Apache-2.0 text;
if neither applies the script fails, so a release never ships an incomplete notice.

Usage:
  scripts/third-party-licenses.py [--target TRIPLE] [--no-default-features] [--output FILE]

The output is deterministic (no dates, no absolute paths). The release workflow
(.github/workflows/release.yml) puts one in every archive; CI runs it to catch dependencies without
usable license texts early. Requires cargo and Python 3.8 or newer (standard library only).
"""

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
PACKAGE = "soniccraft"

LICENSE_NAME = re.compile(r"^(licen[cs]e|copying|notice|copyright|unlicense)", re.IGNORECASE)
LICENSE_DIRS = {"licenses", ".licenses", "licences"}
# Directories whose files are never part of a binary (and whose license-looking files are fixtures).
SKIP_DIRS = {
    "tests", "test", "testdata", "test-data", "examples", "example", "benches", "bench", "fuzz",
    "fixtures", "debian", "target", ".git", ".github", ".cargo", "node_modules",
}
MAX_DEPTH = 4


def run(args):
    try:
        return subprocess.run(args, cwd=ROOT, check=True, capture_output=True, text=True).stdout
    except FileNotFoundError:
        sys.exit(f"error: `{args[0]}` was not found on PATH")
    except subprocess.CalledProcessError as error:
        sys.exit(f"error: `{' '.join(args)}` failed:\n{error.stderr}")


def host_target():
    for line in run(["rustc", "-vV"]).splitlines():
        if line.startswith("host: "):
            return line[len("host: "):].strip()
    sys.exit("error: cannot determine the host target from `rustc -vV`")


def feature_args(options):
    args = []
    if options.no_default_features:
        args.append("--no-default-features")
    if options.features:
        args += ["--features", options.features]
    return args


def built_crates(options):
    """(name, version) of every package `cargo build -p soniccraft` compiles for the target."""
    out = run(
        ["cargo", "tree", "--locked", "-p", PACKAGE, "-e", "normal", "--target", options.target,
         "--prefix", "none", "--format", "{p}"] + feature_args(options)
    )
    crates = set()
    for line in out.splitlines():
        match = re.match(r"^(\S+) v(\S+)", line.strip())
        if match:
            crates.add((match.group(1), match.group(2)))
    if not crates:
        sys.exit("error: `cargo tree` listed no crates")
    return crates


def license_files(package):
    """Relative paths of the license texts a package ships, in a stable order."""
    root = Path(package["manifest_path"]).parent
    found = set()
    if package.get("license_file"):
        found.add(Path(package["license_file"]).as_posix())
    for directory, dirs, files in os.walk(root):
        rel = Path(directory).relative_to(root)
        depth = len(rel.parts)
        dirs[:] = sorted(
            d for d in dirs
            if depth + 1 < MAX_DEPTH and d.lower() not in SKIP_DIRS
            and (not d.startswith(".") or d.lower() in LICENSE_DIRS)
        )
        in_license_dir = any(part.lower() in LICENSE_DIRS for part in rel.parts)
        for name in files:
            if in_license_dir or LICENSE_NAME.match(name):
                found.add((rel / name).as_posix())
    return sorted(found, key=lambda p: (p.count("/"), p.lower()))


def read_text(path):
    text = path.read_bytes().decode("utf-8", errors="replace").replace("\r\n", "\n").replace("\r", "\n")
    lines = [line.rstrip() for line in text.split("\n")]
    while lines and not lines[0]:
        lines.pop(0)
    while lines and not lines[-1]:
        lines.pop()
    return "\n".join(lines)


def license_ids(expression):
    return set(re.findall(r"[A-Za-z0-9.+-]+", (expression or "").replace("/", " OR "))) - {"OR", "AND", "WITH"}


def fallback_texts(package):
    """Canonical texts for a crate without license files: MIT (with its authors) and/or Apache-2.0."""
    ids = license_ids(package.get("license"))
    texts = []
    if "MIT" in ids:
        mit = read_text(ROOT / "LICENSE-MIT").split("\n")
        authors = ", ".join(re.sub(r"\s*<[^>]*>", "", a).strip() for a in package.get("authors") or [])
        holder = authors or f"the {package['name']} authors"
        mit = [f"Copyright (c) {holder}" if line.startswith("Copyright") else line for line in mit]
        texts.append(("no license file shipped; canonical MIT text", "\n".join(mit)))
    if "Apache-2.0" in ids:
        texts.append(("no license file shipped; canonical Apache-2.0 text", read_text(ROOT / "LICENSE-APACHE")))
    return texts


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--target", help="Rust target triple (default: the host)")
    parser.add_argument("--no-default-features", action="store_true", help="the `-no-mp3` build")
    parser.add_argument("--features", help="extra cargo features of soniccraft")
    parser.add_argument("--output", "-o", help="file to write (default: stdout)")
    options = parser.parse_args()
    options.target = options.target or host_target()

    metadata = json.loads(run(["cargo", "metadata", "--format-version", "1", "--locked"]))
    packages = {(p["name"], p["version"]): p for p in metadata["packages"]}
    version = next(p["version"] for p in metadata["packages"] if p["name"] == PACKAGE and p["source"] is None)

    crates = []
    for key in sorted(built_crates(options)):
        package = packages.get(key)
        if package is None:
            sys.exit(f"error: {key[0]} {key[1]} is not in `cargo metadata`")
        if package["source"] is None:
            continue  # a workspace crate: covered by LICENSE-MIT / LICENSE-APACHE
        if not package["source"].startswith("registry+"):
            sys.exit(f"error: {key[0]} {key[1]} does not come from a registry ({package['source']})")
        crates.append(package)

    texts = {}  # normalized text -> [title, text, [users]]
    missing = []
    for package in crates:
        label = f"{package['name']} {package['version']}"
        entries = []
        root = Path(package["manifest_path"]).parent
        for rel in license_files(package):
            entries.append((rel, read_text(root / rel)))
        entries = [(title, text) for title, text in entries if text]
        if not entries:
            entries = fallback_texts(package)
            if not entries:
                missing.append(f"{label} ({package.get('license') or 'no license expression'})")
                continue
        for title, text in entries:
            key = " ".join(text.split())
            entry = texts.setdefault(key, [title, text, []])
            entry[2].append(f"{label}: {title}")
    if missing:
        sys.exit(
            "error: no license text found for: " + ", ".join(missing)
            + "\nAdd the text to the crate's entry in this script or choose another dependency."
        )

    names = {p["name"] for p in crates}
    flavour = "--no-default-features" if options.no_default_features else "default features"
    if options.features:
        flavour += f", features {options.features}"
    out = [
        f"Third-party licenses of soniccraft {version} for {options.target} ({flavour})",
        "=" * 100,
        "",
        "The soniccraft and soniccraft-mcp binaries contain code from the crates below, all published on",
        "crates.io, where each crate's source code is available at https://crates.io/crates/<name>/<version>.",
        "SonicCraft itself is licensed under MIT OR Apache-2.0 (LICENSE-MIT, LICENSE-APACHE).",
        "",
    ]
    if any(n == "symphonia" or n.startswith("symphonia-") or n == "option-ext" for n in names):
        out += [
            "* The MPL-2.0 crates (symphonia*, option-ext) are used unmodified; their source code is at the",
            "  crates.io addresses above (MPL-2.0 section 3.2).",
        ]
    if "mp3lame-sys" in names:
        out += [
            "* mp3lame-sys and mp3lame-encoder (LGPL-3.0) statically link LAME 3.100 (LGPL-2.0-or-later) for",
            "  native MP3 encoding. licenses/README.md has the license texts and explains how to relink a",
            "  modified LAME; the `-no-mp3` archives contain no LGPL code.",
        ]
    out += [
        "* The list includes crates that only run at compile time (procedural macros).",
        "",
        f"Generated by scripts/third-party-licenses.py from Cargo.lock: {len(crates)} crates.",
        "",
        "Crates",
        "-" * 100,
        "",
    ]
    width = max(len(f"{p['name']} {p['version']}") for p in crates) + 2
    for package in crates:
        label = f"{package['name']} {package['version']}"
        out.append(f"{label:<{width}}{package.get('license') or 'see license file'}")
    out += ["", "", "License texts", "-" * 100]
    for title, text, users in texts.values():
        out += ["", "=" * 100, "Used by:"] + [f"  {u}" for u in users] + ["=" * 100, "", text]
    document = "\n".join(out) + "\n"

    if options.output:
        with open(options.output, "w", encoding="utf-8", newline="\n") as file:
            file.write(document)
    else:
        sys.stdout.write(document)


if __name__ == "__main__":
    main()
