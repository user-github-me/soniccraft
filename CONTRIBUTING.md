# Contributing to SonicCraft

Thanks for helping. SonicCraft is a small Rust workspace with a precise specification; most contributions
touch code, the spec and the agent-facing docs together. This guide explains how to build, test and keep
those in sync.

## Ground rules

- **[docs/DESIGN.md](docs/DESIGN.md) is the specification.** If you change behaviour, change DESIGN.md in
  the same pull request. If the code and the spec disagree, one of them is a bug: say which in the PR.
- **Diagnostic codes are a stable API.** Agents key their repairs on them. Add new codes (to
  `crates/soniccraft-core/src/diagnostics.rs`, DESIGN §7 and `skills/soniccraft/reference/diagnostics.md`);
  never rename or remove one.
- **Report types are a stable API.** The CLI's `--json`, MCP `structuredContent`, the manifest and the web
  preview all serialize `crates/soniccraft-engine/src/report.rs`. Add fields; do not rename or remove them.
- **Determinism.** Same timeline + same cache + same binary must give byte-identical outputs: no wall-clock
  time, unseeded randomness, host names or absolute paths in artifacts; stable iteration order (`IndexMap`,
  sorted `Vec`); integer sample-frame geometry.
- **No secrets, no paid APIs in tests.** Never commit API keys, recorded keys or personal voice ids. Tests
  use the `mock` provider, recorded or synthetic provider responses and in-process HTTP test servers;
  `macos_say` tests are gated with `#[cfg(target_os = "macos")]`.
- **No third-party audio.** Example assets are synthesized by `scripts/generate-example-assets.sh`.

## Setup

- Rust stable. The minimum supported version is 1.89 (`rust-version` in Cargo.toml; the CI job `msrv` builds
  with exactly that). A dependency update that needs a newer Rust must raise `rust-version` and the `msrv`
  job together. The workspace uses edition 2024.
- A C toolchain for the bundled LAME build behind the default `mp3` feature: Xcode Command Line Tools on
  macOS, `build-essential` on Debian/Ubuntu.
- On Windows: the Visual Studio Build Tools (MSVC), and `AWS_LC_SYS_PREBUILT_NASM=1` (or NASM on PATH) for
  rustls' aws-lc-sys.
- Node.js 20 or newer for the packages under `packages/`.
- Optional: ffmpeg (ogg/opus/m4a encoding, the decode fallback, regenerating example assets), macOS for
  the `macos_say` provider, Docker for a local Kokoro-FastAPI server.

```bash
cargo build                                    # all crates
cargo test --workspace                         # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build -p soniccraft-cli --no-default-features   # LAME-free build (mp3 via ffmpeg)
(cd packages/soniccraft-mcp && npm test)        # npm launcher; end-to-end tests use target/debug/soniccraft
(cd packages/soniccraft-web-preview && npm test)
```

Work on one crate at a time with `-p`, e.g. `cargo test -p soniccraft-core`. The dev profile optimizes
dependencies (DSP tests are slow unoptimized), so the first build takes a while. Use `--locked` (as CI does)
when you want exactly the dependency versions in `Cargo.lock`.

Try your build end to end with no API keys:

```bash
cargo run -p soniccraft-cli -- render examples/offline-demo/timeline.mock.yaml --json
```

`cargo run -p soniccraft-cli` runs `soniccraft`; add `--bin soniccraft-mcp` for the MCP binary.

## Repository checks

The scripts under `scripts/` are what CI runs besides `cargo`:

| Script | Purpose |
|---|---|
| `scripts/check-skill.sh` | Agent Skill package rules: frontmatter keys, `name`, description length, body size, links. |
| `scripts/check-docs.py` | Every diagnostic code is documented in DESIGN.md §7 and `reference/diagnostics.md`; every relative Markdown link and anchor resolves. |
| `scripts/sync-templates.sh [--check]` | Regenerates the `tech-briefing`, `two-person-podcast` and `offline-demo` skill templates from the examples (`--check`: fail if out of date). |
| `scripts/check-timelines.sh <soniccraft>` | Validates every example and template with the built CLI; only environment errors (missing API keys, unavailable local providers) are tolerated. |
| `scripts/smoke-render.sh <soniccraft>` | Renders the mock-only demo twice and checks outputs, loudness against the target, and byte-identical determinism. |
| `scripts/generate-example-assets.sh [--verify]` | Regenerates the example audio with ffmpeg (`--verify`: check the committed files with ffprobe). |
| `scripts/third-party-licenses.py [--target T] [--no-default-features] [-o FILE]` | Writes the `THIRD-PARTY-LICENSES.txt` of a build (every crate compiled in, with the license texts it ships); CI runs it for every release target and flavour. |
| `scripts/deny-license-hash.py FILE...` | Prints cargo-deny's hash of a license file, for the `[[licenses.clarify]]` entries in `deny.toml`. |

`cargo deny check` ([cargo-deny](https://embarkstudios.github.io/cargo-deny/)) enforces the license, ban and
source policy in `deny.toml`; CI runs it, and reports RustSec advisories in a separate, non-blocking job.

If you have Claude Code installed, `claude plugin validate --strict .claude-plugin/plugin.json` and
`claude plugin validate --strict .` check the plugin and marketplace manifests.

## Changing the DSL

A new or changed timeline field touches, in one PR:

1. `crates/soniccraft-core/src/model.rs` (doc comments become JSON Schema descriptions), the IR in `ir.rs`
   if downstream crates need it, and validation in `lower.rs` with precise diagnostics and hints;
2. the generated schema `skills/soniccraft/reference/timeline-schema.json` (the core crate's golden test;
   regenerate with `SONICCRAFT_BLESS=1 cargo test -p soniccraft-core`);
3. DESIGN.md §2 (and §3 to §10 if behaviour changes);
4. the skill: `skills/soniccraft/reference/dsl-reference.md`, `SKILL.md` if it is an essential rule, and
   `diagnostics.md` for new codes;
5. examples and templates if the feature is user-facing (edit `examples/*/timeline.yaml`, then run
   `scripts/sync-templates.sh`, and copy the regenerated templates into `crates/soniccraft-cli/templates/`,
   whose byte-identical copies a test enforces).

Adding a TTS provider: see "Common changes" in [docs/architecture.md](docs/architecture.md).

## The web preview

`packages/soniccraft-web-preview/preview.html` is the source of the render preview. The engine embeds a
vendored copy, `crates/soniccraft-engine/assets/preview.html`, so the crate stays publishable; a test fails
until the two are byte-identical. After editing the package page, run its tests and copy it:

```bash
(cd packages/soniccraft-web-preview && npm test)
cp packages/soniccraft-web-preview/preview.html crates/soniccraft-engine/assets/preview.html
```

## Example assets

Never hand-edit or replace files under `examples/*/assets/`. Change `scripts/generate-example-assets.sh`
and rerun it; it synthesizes every sound with ffmpeg (no samples, fixed seeds, bit-exact WAV muxing),
proves the music loop is seamless, installs the files into each example and checks the total size budget
(under 6 MB).

## Releasing

1. Set the version in `[workspace.package]` (Cargo.toml) and in `packages/soniccraft-mcp/package.json`, move
   the `## [Unreleased]` entries of CHANGELOG.md into the `## [X.Y.Z]` section and date it (the release notes
   are that section's body).
2. Push the tag `vX.Y.Z` (`vX.Y.Z-rc.N` for a pre-release). `.github/workflows/release.yml` checks that the
   versions match, builds and smoke-tests 10 archives (5 targets, with and without the LGPL MP3 encoder), and
   attaches them, `SHA256SUMS`, and the npm launcher with those checksums pinned, to a draft release.
3. Review the draft and publish it. With the repository variable `NPM_PUBLISH` set to `true` and either an
   `NPM_TOKEN` secret or npm trusted publishing configured for `release.yml`, publishing the release
   publishes `soniccraft-mcp` to npm.

Never rebuild a published release (the workflow refuses): the npm package pins its archives' checksums.
Release a new version instead.

## Pull requests

- Keep PRs focused; include tests for behaviour changes (assert diagnostic code, path and hint for new
  validation rules).
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`
  pass.
- Public items have rustdoc comments; errors carry context; no `unwrap()` on user input or I/O.
- Add an entry under `## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md).

## License

SonicCraft is dual-licensed under MIT OR Apache-2.0. Unless you explicitly state otherwise, any
contribution intentionally submitted for inclusion in this project by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or conditions.
