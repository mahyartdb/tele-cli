# Pull request

## What changed

<!-- One or two sentences. What behavior is different after this merges? -->

## Why

<!-- The problem this solves. Link the issue: Fixes #123 -->

## Type of change

- [ ] Bug fix
- [ ] New capability
- [ ] Documentation
- [ ] Refactor or cleanup
- [ ] Build, CI, or release tooling

## Checklist

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test` passes
- [ ] `docs/capabilities.md` updated if a capability row changed
- [ ] `docs/cli-contract.md` updated if a JSON shape, flag, or exit code changed
- [ ] `CHANGELOG.md` updated under `[Unreleased]`
- [ ] Machine-API changes are additive only — no renamed, removed, or retyped JSON keys

## Notes for reviewers

<!-- Anything that needs a second look: risky paths, offline/online split, secret handling. -->
