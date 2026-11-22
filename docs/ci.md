# CI

`.github/workflows/ci.yml` runs on every push to `master`/`main` and on every pull request that targets either branch. Green CI is the precondition for merging and for a release; the release workflow (`.github/workflows/release.yml`) does not repeat these checks.

## What runs and when

- **Triggers:** `push` to `master` or `main`, `pull_request` into `master` or `main`. A push to a feature branch runs nothing; open the PR.
- **Concurrency:** group `ci-<workflow>-<ref>`, `cancel-in-progress: true`. A new push to the same branch cancels the older run. Different PRs never cancel each other.
- **Permissions:** `contents: read` for the whole workflow. No job needs more. Checkouts set `persist-credentials: false`, so the token is not left in `.git/config` for later steps.
- **Failure isolation:** every matrix uses `fail-fast: false`. One failing OS does not hide a passing one.

## Job and gate map

| Job | Runner | Gate | Local command |
|---|---|---|---|
| `fmt` | ubuntu | formatting | `cargo fmt --all -- --check` |
| `clippy` | ubuntu, windows | lints, warnings are errors | `cargo clippy --all-targets --locked -- -D warnings` |
| `test` | ubuntu, windows, macos | the test suite | `cargo test --locked` |
| `msrv` | ubuntu | builds on the declared MSRV | `rustup toolchain install 1.89 && cargo +1.89 check --workspace --all-targets --locked` |
| `audit` | ubuntu | RustSec advisories | `cargo audit` |
| `supply-chain` | ubuntu | tracked secrets, then `cargo deny check` | see [Run the gates locally](#run-the-gates-locally) |

Every cargo command that resolves dependencies passes `--locked`. CI therefore fails when `Cargo.toml` and `Cargo.lock` disagree instead of silently resolving a different dependency graph in the runner. `cargo fmt` does not resolve dependencies and takes no such flag. The release workflow builds with `--locked` for the same reason.

### Why the matrix is uneven

- **`fmt` runs once.** rustfmt output is platform-independent, and `.gitattributes` (`* text=auto eol=lf`) checks out LF everywhere. Running it on three OSes would do identical work three times.
- **`clippy` runs on Linux and Windows.** The crate only branches on `unix` versus `windows` (`cfg(unix)`, `cfg(windows)`, `cfg(not(windows))`, plus `cfg(not(any(unix, windows)))` fallbacks); there is no `cfg(target_os = "macos")` or `cfg(target_os = "linux")` anywhere in `src/`. Linux lints the `unix` code, Windows lints the `windows` code, and macOS would lint the same `unix` code as Linux. The Windows leg is not optional: it is the only place Windows-only code is linted.
- **`test` still runs on all three.** File permissions, session locking, path handling and console behavior are platform-specific at runtime, which is exactly what the suite covers. It also compiles the full dependency graph on macOS, so dropping the macOS clippy leg does not leave macOS compilation untested.

## Build settings

The `test` job is the only job that links the crate and its test binaries, and it is the wall-clock long pole. Two settings change what that build emits; neither changes what any gate checks.

**No debuginfo.** `CARGO_PROFILE_DEV_DEBUG=0` and `CARGO_PROFILE_TEST_DEBUG=0` are set on the `test` job. `cargo test` links four binaries — the `tele` binary (integration tests run it through `CARGO_BIN_EXE`), its unit-test harness, and the `contract` and `selection` test crates — and on Windows each link also writes a PDB. Measured on a warm cache before this setting existed: the Windows `cargo test` step spent 2m03s compiling and linking before the first test ran, against 46s on Linux for the same work. That gap is mostly debug-info and link cost, and a smaller `target/` is less to restore and save on every run.

What it does **not** change: optimisation level, `debug-assertions`, overflow checks, or which tests run. The profile is still the same unoptimised dev/test profile the suite is written against. The cost is that a panic backtrace in CI has no symbol names; an assertion failure still prints its own message and `file:line`, and anything that needs a real stack trace reproduces locally, where the profiles are untouched.

**`rust-lld` on Windows.** `RUSTFLAGS=-Clinker=rust-lld` is set for `windows-latest` only, and is the empty string elsewhere (the cache key ignores empty values). `rust-lld` ships inside the toolchain the job already installs — no new tool, package or download — and is a drop-in for `link.exe` in MSVC mode. It is not set on `clippy` (clippy emits metadata and never links the crate) and not on Linux or macOS: neither is the long pole, and changing their linker buys nothing here while adding a platform-specific failure mode.

`RUSTFLAGS` is part of the cache key, so the first run after a change to it rebuilds the dependency graph once and reuses that entry afterwards. It is a one-off cost, not a per-run one.

`CARGO_INCREMENTAL=0` needs no line here: `Swatinem/rust-cache`'s restore step exports it (`src/restore.ts` at the pinned SHA). Incremental artifacts cannot help a run that starts from a different source state, and they would only inflate `target/`.

## TL drift check

`tests/contract.rs::vendored_tl_api_matches_grammers_tl_types` compares the vendored `tl/api.tl` against the `grammers-tl-types` source in the Cargo registry. It resolves the registry from `$CARGO_HOME` (falling back to `$USERPROFILE/.cargo`) and looks for `registry/src/index.crates.io-*/grammers-tl-types-<version>/tl/api.tl`; when that file is absent it fails with a message that names the missing piece unless `TELE_SKIP_TL_DRIFT_CHECK=1` is set.

CI keeps the opt-out on the full matrix (a runner that cannot see the registry source should not redden all three legs) and then runs the same test **without** the opt-out on the Linux leg, defaulting `CARGO_HOME` to `$HOME/.cargo` so the lookup does not depend on the runner exporting it:

```yaml
- name: TL drift check (must not skip)
  if: matrix.os == 'ubuntu-latest'
  run: |
    export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
    cargo test --locked --test contract vendored_tl_api_matches_grammers_tl_types
```

So the schema is always actually verified on push and PR, not merely skipped. Locally the check runs as part of `cargo test --locked` whenever `CARGO_HOME` (or `USERPROFILE` on Windows) resolves; the opt-out exists for environments where the registry source cannot be located.

## Caching

`Swatinem/rust-cache` is used by `clippy`, `test` and `msrv`, pinned to a commit SHA. All three set `save-if: ${{ github.ref == 'refs/heads/main' }}`: pushes to `main` write the cache, PR runs only restore it. On a busy repository a PR branch that changes `Cargo.lock` can otherwise evict the good `main` entry by filling the 10 GB repository quota with a short-lived cache. A PR normally gets the warm `main` entry restored; if it misses, it builds cold but does not poison the cache for everyone else. It caches:

- `~/.cargo/registry` — index, crate archives, and the extracted sources under `registry/src`,
- `~/.cargo/git`,
- `~/.cargo/bin` plus `.crates.toml` / `.crates2.json`,
- `target/`.

Before saving, the action prunes `target/`, `registry/src`, `registry/cache`, the index `.cache` and `git/db` down to the packages in `Cargo.lock`, so an entry holds the current dependency graph rather than its history. That is also why the TL drift check is safe with the registry cached: the upstream `grammers-tl-types` source is either restored under `registry/src` or extracted by cargo during the build, and the check only reads the file after the build step has run.

The cache key is `v0-rust-<job>-<os>-<arch>-<env-hash>-<lock-hash>`: the job id and runner OS/architecture, a hash over `rustc -vV` for every installed toolchain plus every `CARGO*`/`CC*`/`CFLAGS`/`CXX`/`CMAKE`/`RUST*` environment variable with a non-empty value, and a hash over the workspace `Cargo.toml` manifests, `Cargo.lock`, and any `rust-toolchain*`/`.cargo/config.toml` files. Consequences worth knowing:

- The `test` job's `CARGO_PROFILE_*_DEBUG` and `RUSTFLAGS` settings are part of its env hash, so its entries are distinct from the other jobs' — and changing either setting starts a new entry once.
- `msrv` and the stable jobs never share an entry: the job id differs, and the installed toolchain set differs too (the `msrv` job installs 1.89 instead of stable).
- A `Cargo.lock` change invalidates the build cache, which is correct — a stale `target/` for a changed dependency graph is worse than a cold build.

`shared-key` is deliberately not used to merge the `clippy`, `test` and `msrv` entries for one OS. They cannot share build artifacts — `clippy` runs the compiler in check mode, `test` codegens and links, `msrv` uses a different toolchain — so each would restore the others' `target/`, rebuild over it, and the last job to finish would evict the entry the next run wants. Merging for the registry's sake is unnecessary too: a `target/`-complete entry already means no dependency is rebuilt. Measured on a warm cache, the Windows `test` job's build printed exactly one `Compiling` line (`telecli`) and no dependency recompiles.

`fmt`, `audit` and `supply-chain` do not use `rust-cache`. `fmt` compiles nothing. `audit` and `supply-chain` compile nothing in the repository and install their tools from prebuilt binaries, so nothing is restored from `~/.cargo/bin` there; `taiki-e/install-action` downloads the exact pinned version on every run and verifies its SHA-256 against the action's manifest, so a stale cached binary can never stand in for a failed install.

## Timeouts

Every job sets `timeout-minutes`; the GitHub default is 360 minutes, which turns a hung download or a deadlocked test into an hour-long red run.

| Job | Timeout | Budget rationale |
|---|---|---|
| `fmt` | 5 | toolchain install plus a sub-second check; cold checkout included |
| `clippy` | 25 | cold Windows build of the full dependency tree is the long pole; headroom for runner variance |
| `test` | 25 | full build plus 2,000+ tests on three OSes; Windows is the long pole — 3m39s measured before the build settings above, which remove the debug-info and link cost |
| `msrv` | 15 | `cargo check --workspace --all-targets`; metadata only, so only build scripts and proc macros fully codegen |
| `audit` | 10 | checksum-verified binary download plus advisory-DB fetch |
| `supply-chain` | 15 | secret scan (seconds) plus `cargo deny check` over four checks and a first-run advisory DB |

A timeout is a failure, not a retry. If a job legitimately needs longer, raise its budget in the same change and say why in the commit body. Do not remove the timeout.

## Pinned actions

Every `uses:` in `.github/workflows/ci.yml` carries a full 40-character commit SHA and a trailing comment naming the ref it came from. Floating refs (`@v2`, `@stable`) let a third party change what runs in this repository without a commit here. The release workflow pins its own actions separately and is not covered by this table.

| Action | Pinned SHA | Ref |
|---|---|---|
| `actions/checkout` | `3d3c42e5aac5ba805825da76410c181273ba90b1` | v7.0.1 |
| `dtolnay/rust-toolchain` | `6bed0761d98439e5a578e2877258200ad565ba87` | `stable` branch |
| `Swatinem/rust-cache` | `d1527c8f67ef9af357290475d4af4b0fab8f12e0` | v2.9.3 |
| `taiki-e/install-action` | `75a7c0f87bd666af3cdb75c5b9e9d03ab6044e6f` | v2.90.6 |

`actions/checkout` v7 runs on Node 24. The v4 pin ran on Node 20, which GitHub now forces onto Node 24 at run time and reports as a deprecation warning on every job; v7 is that same runtime, declared. v5 and later require a runner of at least 2.327.1, which GitHub-hosted runners are.

`dtolnay/rust-toolchain` is branch-per-toolchain: the pinned SHA is an earlier snapshot of the `stable` branch (the branch has moved on since; a pin does not track it), and that snapshot's default input is `toolchain: stable`. The `msrv` job overrides it with `toolchain: "1.89"`, which the action maps to the `1.89` rustup toolchain. Pinning the SHA pins the action's code, not the Rust release — `stable` still means "current stable at run time", which is intended.

`taiki-e/install-action` in `ci.yml` is a composite action (no Node runtime) and is left at v2.90.6: a newer patch exists, but the install step is a second either way, and moving it means re-checking that `cargo-audit@0.22.2` and `cargo-deny@0.20.2` are still in its manifests. `release.yml` uses v2.90.8 for its `cross` install. Two pinning styles exist: a `@<tool_name>` branch ref (for example `@cross`) carries a default `tool:` input, while a version-tag ref such as `@v2.90.8` requires an explicit `with: tool: cross`. Bumping from the former to the latter without adding that input makes the step warn and exit 0 without installing anything.

Two tool versions are pinned independently of the action SHA: `cargo-audit@0.22.2` and `cargo-deny@0.20.2`. Both exist in install-action v2.90.6's manifests with per-platform SHA-256 checksums, so they install as verified downloads. `cargo-deny` 0.20.2 was also run against this repository: all four checks (advisories, bans, licenses, sources) ran to completion, with one documented advisory waiver, see below. Advisories are evaluated against the current `Cargo.lock` on every run, so a newly published advisory reddens the job until the dependency is bumped or the advisory is waived in `deny.toml`.

### Updating a pin

Resolve the commit, not the tag object. Annotated tags point at a tag object whose `object.sha` is not the commit.

```bash
# branch-pinned action
gh api repos/dtolnay/rust-toolchain/git/ref/heads/stable --jq .object.sha

# tag-pinned action
ref=$(gh api repos/taiki-e/install-action/git/ref/tags/v2.91.0)
sha=$(jq -r .object.sha <<<"$ref")
type=$(jq -r .object.type <<<"$ref")
if [ "$type" = tag ]; then
  sha=$(gh api "repos/taiki-e/install-action/git/tags/$sha" --jq .object.sha)
fi
echo "$sha"
```

Then:

1. Replace the SHA in the `uses:` line and update the `# vX.Y.Z` comment to match.
2. If the action is `taiki-e/install-action`, confirm the `tool:` input is still set. A `@<tool_name>` branch pin carries a default, but a version-tag pin does not and the step silently installs nothing without an explicit `with: tool:`. Bump the `cargo-audit@` / `cargo-deny@` versions deliberately and confirm the new versions are in the action's `manifests/*.json`. A version missing from the manifest falls back to cargo-binstall with a warning, which loses the checksum-verified prebuilt path; a version that does not exist fails the install step.
3. Validate the YAML (see below) and let the PR run the full matrix before merging.

## Run the gates locally

Everything CI runs is a plain command. On a machine with the toolchain installed:

```bash
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"   # TL drift test needs it on Linux/macOS
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0   # what CI's test job builds
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
rustup toolchain install 1.89
cargo +1.89 check --workspace --all-targets --locked
cargo audit
cargo deny check
```

The two `CARGO_PROFILE_*_DEBUG` exports are optional locally: they reproduce CI's build (no debuginfo, smaller `target/`, no symbol names in a panic backtrace) and change nothing a test asserts. Leave them unset when you want a debuggable stack trace. On Windows, add `RUSTFLAGS=-Clinker=rust-lld` to match the `test` job's linker.

`cargo audit` and `cargo deny` are not part of the Rust distribution. Install the same versions CI uses:

```bash
cargo install --locked cargo-audit --version 0.22.2
cargo install --locked cargo-deny --version 0.20.2
```

The secret scan is the first step of `supply-chain`, so a leak fails the job before the advisory-DB download. Run it from the repository root exactly as CI does:

```bash
set -euo pipefail
tracked="$(mktemp)"
git ls-files > "$tracked"
if grep -E '(^|/)\.env$|\.session(-journal|-wal|-shm)?$' "$tracked"; then
  echo "tracked secret files (.env / sessions) must never be committed" >&2
  exit 1
fi
if grep -E 'id_rsa|id_ed25519|\.pem$|\.key$|\.p12$|\.pfx$|\.kdbx$' "$tracked"; then
  echo "tracked private-key material must never be committed" >&2
  exit 1
fi
if git grep -nE 'TELE_API_HASH\s*=\s*"?[0-9a-fA-F]{16,}' -- . ':!Cargo.lock'; then
  echo "hex-like TELE_API_HASH assignment in tracked files; use YOUR_API_HASH_HERE placeholders" >&2
  exit 1
fi
```

On Windows, run that block in Git Bash. The cargo commands are identical in PowerShell.

Before pushing a workflow change, parse it:

```bash
python -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))"
```

If `actionlint` is installed, run it too; it catches expression and context errors that a YAML parser does not.

## Troubleshooting

### `cargo audit` or `cargo deny` fails to install

The install step is a single `taiki-e/install-action` download of a pinned version. There is no fallback command, because a fallback that runs the tool after its install failed can only succeed from a stale binary and would then hide the real error. Read the failing step's log:

- A `::warning::install-action: ... fallback to cargo-binstall` line means the pinned version or the runner's platform is not in the action's manifest, so the tool is installed through cargo-binstall instead of the checksum-verified release asset. If the version does not exist, the install step fails. Pin a version that `manifests/cargo-audit.json` / `manifests/cargo-deny.json` lists, or bump the action SHA to a release that lists it.
- A checksum mismatch means the release asset changed under a version that should be immutable. Do not set the `checksum` input to `false`; change the pinned version.
- A network failure on GitHub Releases is transient. Rerun the job.

### `cargo audit` reports a vulnerability

The report lists the advisory id, crate, version and title, and exits non-zero. That is a real finding. Upgrade the dependency, or — when no fixed version exists — pass a narrow `--ignore RUSTSEC-XXXX-YYYY` (or record it in `.cargo/audit.toml`) and say in the commit why it does not apply. `cargo deny` keeps its own ignore list in `deny.toml` under `[advisories]`; the two tools do not share configuration. The current list carries one reason-bearing entry, `RUSTSEC-2026-0543` (chrono is unmaintained transitively via grammers/rmcp with no fixed release). Re-check it on every dependency bump and remove the entry if a fixed graph becomes possible. A failed advisory-DB download is a different message (`failed to fetch advisory database`) and is not a finding; rerun.

### `msrv` fails

Two causes, different fixes:

- Cargo reports that a package requires a newer rustc — a dependency raised its own MSRV. Pin the dependency back or replace it; do not raise our MSRV to keep the bump.
- A compile error in our own code under 1.89 — the change used a newer language or library feature. Rewrite it, or raise the MSRV deliberately in `Cargo.toml` (`rust-version`), `AGENTS.md`, and this document together. That is a product decision, not a CI fix.

### `secret scan` fails

The step prints the offending path (first two checks) or `path:line:text` from `git grep` (third check).

- Tracked `.env` or `*.session*`: `git rm --cached <path>`, add the path to `.gitignore`, and rotate anything the file contained. Removing it from the working tree is not enough — the history still has it.
- Tracked key material (`id_rsa`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.kdbx`): same treatment. Test fixtures belong in a generated temp directory, not the index.
- Hex-like `TELE_API_HASH` assignment: replace the value with `YOUR_API_HASH_HERE`. Real credentials live outside the repository in `%APPDATA%\tele\.env`; an untracked local `.env` is expected and does not trip the scan.

The scan only reads tracked files, so it never inspects the real credential file. `Cargo.lock` is excluded from the content check because it legitimately contains hex hashes.

### `TL drift check (must not skip)` fails

`vendored tl/api.tl is stale (differs from grammers-tl-types <version> at <path>)` means the dependency was bumped without re-vendoring the schema. The failure prints the exact copy command:

```bash
cp <path> tl/api.tl
```

Then rerun `cargo test --locked --test contract vendored_tl_api_matches_grammers_tl_types`. If instead the message starts with `cannot locate upstream api.tl for grammers-tl-types`, the registry source is missing on the runner; that is an environment problem, and the message says so.

### `fmt` or `clippy` fails

`fmt`: run `cargo fmt --all`. The check runs on Linux, so a formatting difference a Windows contributor sees locally is usually a line-ending problem — check `.gitattributes` before editing code.

`clippy`: a Windows-only warning is in `cfg(windows)` code and is real. Fix it. Do not drop the Windows leg, and do not add `#[allow]` to silence a lint without a comment naming the reason.

### The Windows `test` leg fails while linking

The job links with `rust-lld` (`RUSTFLAGS=-Clinker=rust-lld`), which ships inside the toolchain. A failure here names the linker and the object it could not handle; reproduce it locally with `rustc -Clinker=rust-lld --version` and then `cargo test --locked` under the same `RUSTFLAGS`. If a dependency's build script produces something lld cannot link, remove the `RUSTFLAGS` line for that leg and say in the commit why — the job gets slower, nothing else changes.

### A CI backtrace has addresses but no function names

Expected: the `test` job builds with `CARGO_PROFILE_DEV_DEBUG=0` / `CARGO_PROFILE_TEST_DEBUG=0` to keep the Windows link out of the critical path. Assertion failures still print their message and `file:line`. Reproduce locally without those variables to get a symbolised stack.

## Changing the workflow

- Keep every gate on both `push` and `pull_request`. A gate that only runs on `push` is discovered after merge.
- Keep `permissions` at `contents: read` unless a new job demonstrably needs more, and scope the extra permission to that job.
- Pin any new action to a full commit SHA with a `# ref` comment, and add it to the table above.
- Never write `command || other-command` as an install fallback. Install once, fail loudly, and let the next step run the tool it expects.
- Keep the `test` job's build settings unless a measurement replaces them: `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0` and the Windows `RUSTFLAGS=-Clinker=rust-lld`. They remove the debug-info and link cost from the only leg that links the crate, and they change no gate.
- Validate the YAML, run the gates, and keep the job count honest: a new check belongs in an existing job unless it needs a different OS or a different tool.
