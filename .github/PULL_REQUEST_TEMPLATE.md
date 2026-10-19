## Summary

<!-- What changes and why. Link the issue if there is one. -->

## Checklist

- [ ] `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass
- [ ] Behaviour changes are reflected in `docs/DESIGN.md`
- [ ] DSL, provider or diagnostic changes are reflected in `skills/soniccraft/` (references, templates) and the README
- [ ] Example changes were followed by `scripts/sync-templates.sh`
- [ ] No API keys, personal voice ids or third-party audio added
- [ ] `CHANGELOG.md` has an entry under `[Unreleased]`
