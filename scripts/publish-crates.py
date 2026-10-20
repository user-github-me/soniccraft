#!/usr/bin/env python3
"""Publish the workspace crates to crates.io: `cargo publish --workspace --locked`, skipping the crates
whose version is already there, and waiting out crates.io's rate limit for new crates.

cargo refuses to start a workspace publish when any selected crate's version already exists, so this
script reads the crates.io index first and adds `--exclude NAME` for every crate that is published.
cargo then packages and verifies the rest and uploads them in dependency order. After a failure, run it
again: it continues with the crates that are still missing.

crates.io allows each account a burst of 5 new crates, then one every 10 minutes
(https://crates.io/docs/rate-limits), so the first publish of more than 5 crates is refused with HTTP 429
after the fifth. The script then sleeps until the time crates.io names and runs cargo again, for at most
--max-wait seconds in total. New versions of existing crates (a burst of 30) are not held up.

Usage:
  scripts/publish-crates.py [--dry-run] [--list] [--max-wait SECONDS] [-- CARGO_PUBLISH_ARGS...]

  --list       print the crates that would be published, one per line, and exit
  --dry-run    package and verify like `cargo publish --dry-run`; upload nothing
  --max-wait   the longest total wait for the rate limit (default 3600 seconds)

Authentication is cargo's own: a token stored by `cargo login`, or CARGO_REGISTRY_TOKEN (as in
.github/workflows/crates.yml). Requires cargo and Python 3.8 or newer (standard library only).
"""

import argparse
import email.utils
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
INDEX = "https://index.crates.io"
USER_AGENT = "soniccraft-publish-crates (scripts/publish-crates.py)"
# cargo: "the remote server responded with an error (status 429 Too Many Requests): You have published too
# many new crates in a short period of time. Please try again after Tue, 20 Oct 2026 09:41:00 GMT and see ...".
RATE_LIMITED = re.compile(r"status 429|429 Too Many Requests|published too many")
RETRY_AFTER = re.compile(r"try again after (\w{3}, \d{1,2} \w{3} \d{4} \d{2}:\d{2}:\d{2} GMT)")
ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
# Waiting a little past the instant crates.io names avoids a second refusal at the boundary.
SLACK_SECONDS = 15
DEFAULT_RETRY_SECONDS = 600


def fail(message):
    sys.exit(f"error: {message}")


def workspace_crates():
    """(name, version) of the workspace members that may be published to crates.io."""
    try:
        out = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
    except FileNotFoundError:
        fail("`cargo` was not found on PATH")
    except subprocess.CalledProcessError as error:
        fail(f"`cargo metadata` failed:\n{error.stderr}")
    metadata = json.loads(out)
    members = set(metadata["workspace_members"])
    crates = []
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        publish = package.get("publish")  # null: any registry; []: publish = false
        if publish is not None and "crates-io" not in publish:
            continue
        crates.append((package["name"], package["version"]))
    if not crates:
        fail("no workspace member can be published to crates.io")
    return crates


def index_path(name):
    """The crate's file in the crates.io index (https://doc.rust-lang.org/cargo/reference/registry-index.html)."""
    name = name.lower()
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def published_versions(name):
    """The versions of `name` on crates.io, yanked ones included (they can never be published again)."""
    request = urllib.request.Request(
        f"{INDEX}/{index_path(name)}",
        headers={"User-Agent": USER_AGENT, "Cache-Control": "no-cache"},
    )
    for attempt in range(1, 5):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                body = response.read().decode("utf-8")
            break
        except urllib.error.HTTPError as error:
            if error.code in (404, 410, 451):  # what cargo treats as "no such crate"
                return set()
            if error.code < 500 or attempt == 4:
                fail(f"the crates.io index answered {error.code} for {name}: {error.reason}")
        except (urllib.error.URLError, OSError) as error:
            if attempt == 4:
                fail(f"cannot read the crates.io index for {name}: {error}")
        time.sleep(2**attempt)  # a dropped connection or a 5xx is usually gone a few seconds later
    return {json.loads(line)["vers"] for line in body.splitlines() if line.strip()}


def run_cargo(command):
    """Run cargo with its output shown as it comes; returns (exit status, the output)."""
    env = dict(os.environ)
    if sys.stderr.isatty():
        env.setdefault("CARGO_TERM_COLOR", "always")
    print("+ " + " ".join(command), file=sys.stderr, flush=True)
    process = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    captured = []
    for line in iter(process.stdout.readline, b""):
        sys.stderr.buffer.write(line)
        sys.stderr.buffer.flush()
        captured.append(line)
    return process.wait(), b"".join(captured).decode("utf-8", errors="replace")


def retry_time(output):
    """When crates.io allows the next publish, if the output reports its rate limit; else None."""
    text = ANSI_ESCAPE.sub("", output)
    if not RATE_LIMITED.search(text):
        return None
    match = RETRY_AFTER.search(text)
    if match:
        try:
            return email.utils.parsedate_to_datetime(match.group(1))
        except (TypeError, ValueError):
            pass
    return datetime.now(timezone.utc) + timedelta(seconds=DEFAULT_RETRY_SECONDS)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.split("\n\n")[0],
        usage="%(prog)s [--dry-run] [--list] [--max-wait SECONDS] [-- CARGO_PUBLISH_ARGS...]",
    )
    parser.add_argument("--dry-run", action="store_true", help="package and verify, upload nothing")
    parser.add_argument("--list", action="store_true", help="print the crates to publish and exit")
    parser.add_argument("--max-wait", type=int, default=3600, metavar="SECONDS",
                        help="longest total wait for the crates.io rate limit (default: 3600)")
    parser.add_argument("cargo_args", nargs="*", metavar="CARGO_PUBLISH_ARGS",
                        help="extra `cargo publish` arguments, after `--`")
    options = parser.parse_args()

    crates = workspace_crates()
    waited = 0
    while True:
        published = [(name, version) for name, version in crates if version in published_versions(name)]
        pending = [(name, version) for name, version in crates if (name, version) not in published]
        if options.list:
            for name, _ in pending:
                print(name)
            return 0
        for name, version in published:
            print(f"skipping {name} {version}: already on crates.io", file=sys.stderr)
        if not pending:
            print("nothing to publish: every crate's version is on crates.io", file=sys.stderr)
            return 0

        command = ["cargo", "publish", "--workspace", "--locked"]
        if options.dry_run:
            command.append("--dry-run")
        for name, _ in published:
            command += ["--exclude", name]
        command += options.cargo_args
        status, output = run_cargo(command)
        if status == 0:
            return 0
        when = None if options.dry_run else retry_time(output)
        if when is None:
            return status
        delay = max(0, int((when - datetime.now(timezone.utc)).total_seconds())) + SLACK_SECONDS
        if waited + delay > options.max_wait:
            print(
                f"error: crates.io accepts the next new crate at {when:%Y-%m-%d %H:%M:%S} UTC, beyond --max-wait"
                f" ({options.max_wait} s); run this script again then",
                file=sys.stderr,
            )
            return status
        print(
            f"crates.io rate limit: waiting {delay} s, until {when:%H:%M:%S} UTC, to publish the remaining crates",
            file=sys.stderr, flush=True,
        )
        time.sleep(delay)
        waited += delay


if __name__ == "__main__":
    sys.exit(main())
