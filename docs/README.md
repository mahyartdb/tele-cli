# Documentation

Every document in this folder, grouped by what you need it for. The repo root holds [README.md](../README.md) (overview), [CONTRIBUTING.md](../CONTRIBUTING.md) (how to contribute), and [SECURITY.md](../SECURITY.md) (reporting a vulnerability).

## Start here

| Document | Purpose |
|---|---|
| [getting-started.md](getting-started.md) | Install, get API credentials, log in, first commands |
| [examples.md](examples.md) | Recipes for common tasks |
| [capabilities.md](capabilities.md) | Feature matrix: every command against the Telegram API it maps to |

## Reference

| Document | Purpose |
|---|---|
| [cli-contract.md](cli-contract.md) | The public machine API: JSON envelopes, exit codes, MCP tool table |
| [architecture.md](architecture.md) | Codebase structure and data flow |

`cli-contract.md` is binding. `--json` and JSONL shapes change only additively; anything else is a bug.

## Operations

| Document | Purpose |
|---|---|
| [observability.md](observability.md) | Logging, output streams, exit codes |
| [perf-baselines.md](perf-baselines.md) | Offline measurement baselines and the budget protocol |
| [release.md](release.md) | Build targets, publishing, npm and GitHub releases |
| [live-testing.md](live-testing.md) | How live Telegram testing is run against real accounts |

## Security

| Document | Purpose |
|---|---|
| [security.md](security.md) | Threat model, trust boundaries, deliberate exposures |

## For agents

| Document | Purpose |
|---|---|
| [agents/domain.md](agents/domain.md) | Domain vocabulary for this codebase |
| [agents/issue-tracker.md](agents/issue-tracker.md) | How the local issue tracker works |
| [agents/triage-labels.md](agents/triage-labels.md) | Canonical triage labels |

Agents working *on* tele should also read [AGENTS.md](../AGENTS.md) at the repo root.

## Design decisions

Architecture decision records. Each states the context, the decision, and the consequences.

| ADR | Title |
|---|---|
| [001](decisions/001-session-kernel.md) | Session kernel |
| [002](decisions/002-capability-matrix.md) | Capability matrix |
| [003](decisions/003-cli-json-contract.md) | CLI JSON contract |
| [004](decisions/004-flood-and-parallel.md) | Flood and parallel handling (superseded by 008) |
| [005](decisions/005-unpublished-until-want-done.md) | Unpublished until every `want` row ships |
| [006](decisions/006-rust-grammers.md) | Rust and grammers pivot |
| [007](decisions/007-product-scope-v1.md) | Product scope v1 |
| [008](decisions/008-per-account-flood-weights.md) | Per-account flood weights |
| [009](decisions/009-hardening-perf-scope.md) | Hardening and performance scope |

## Specs

| Document | Purpose |
|---|---|
| [specs/live-qa-hardening.md](specs/live-qa-hardening.md) | Live QA hardening plan |

## Assets

`demo/` holds the images used by the README banner. Screenshots come from real local runs.
