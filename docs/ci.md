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

`Swatinem/rust-cache` is used by `clippy`, `test` and `msrv`, pinned to a commit SHA. It caches:

- `~/.cargo/registry` (index and crate archives; `registry/src` is deliberately **not** cached — it is re-extracted from the archives by the preceding `cargo test` step, which is what makes the TL drift check's registry lookup work),
- `~/.cargo/git`,
- `~/.cargo/bin` plus `.crates.toml` / `.crates2.json`,
- `target/`.

The cache key is `v0-rust-<job>-<os>-<arch>-<env-hash>-<lock-hash>`: the job id and runner OS/architecture, a hash over `rustc -vV` for every installed toolchain plus all `CARGO*`/`RUST*`/`CC*`/`CFLAGS`/`CXX`/`CMAKE` environment variables, and a hash over the workspace `Cargo.toml` manifests, `Cargo.lock`, and any `rust-toolchain*`/`.cargo/config.toml` files. Consequences worth knowing:

- `msrv` and the stable jobs never share an entry: the job id differs, and the installed toolchain set differs too (the `msrv` job installs 1.89 instead of stable).
- A `Cargo.lock` change invalidates the build cache, which is correct — a stale `target/` for a changed dependency graph is worse than a cold build.
- Each job keeps its own `target/` entry. `clippy` and `test` build with different compiler wrappers, so they cannot reuse each other's artifacts anyway.

`fmt`, `audit` and `supply-chain` do not use `rust-cache`. `fmt` compiles nothing. `audit` and `supply-chain` compile nothing in the repository and install their tools from prebuilt binaries, so nothing is restored from `~/.cargo/bin` there; `taiki-e/install-action` downloads the exact pinned version on every run and verifies its SHA-256 against the action's manifest, so a stale cached binary can never stand in for a failed install.

## Timeouts

Every job sets `timeout-minutes`; the GitHub default is 360 minutes, which turns a hung download or a deadlocked test into an hour-long red run.

| Job | Timeout | Budget rationale |
|---|---|---|
| `fmt` | 5 | toolchain install plus a sub-second check; cold checkout included |
| `clippy` | 25 | cold Windows build of the full dependency tree is the long pole; headroom for runner variance |
| `test` | 25 | full build plus 2,000+ tests on three OSes; cold Windows is the long pole |
| `msrv` | 15 | `cargo check --workspace --all-targets`; metadata only, so only build scripts and proc macros fully codegen |
| `audit` | 10 | checksum-verified binary download plus advisory-DB fetch |
| `supply-chain` | 15 | secret scan (seconds) plus `cargo deny check` over four checks and a first-run advisory DB |

A timeout is a failure, not a retry. If a job legitimately needs longer, raise its budget in the same change and say why in the commit body. Do not remove the timeout.

## Pinned actions

Every `uses:` carries a full 40-character commit SHA and a trailing comment naming the ref it came from. Floating refs (`@v2`, `@stable`) let a third party change what runs in this repository without a commit here.

| Action | Pinned SHA | Ref |
|---|---|---|
| `actions/checkout` | `11d5960a326750d5838078e36cf38b85af677262` | v4 |
| `dtolnay/rust-toolchain` | `6bed0761d98439e5a578e2877258200ad565ba87` | `stable` branch |
| `Swatinem/rust-cache` | `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` | v2 |
| `taiki-e/install-action` | `75a7c0f87bd666af3cdb75c5b9e9d03ab6044e6f` | v2.90.6 |

`dtolnay/rust-toolchain` is branch-per-toolchain: the pinned SHA is an earlier snapshot of the `stable` branch (the branch has moved on since; a pin does not track it), and that snapshot's default input is `toolchain: stable`. The `msrv` job overrides it with `toolchain: "1.89"`, which the action maps to the `1.89` rustup toolchain. Pinning the SHA pins the action's code, not the Rust release — `stable` still means "current stable at run time", which is intended.

Two tool versions are pinned independently of the action SHA: `cargo-audit@0.22.2` and `cargo-deny@0.20.2`. Both exist in install-action v2.90.6's manifests with per-platform SHA-256 checksums, so they install as verified downloads. `cargo-deny` 0.20.2 was also run against this repository: the **full** `cargo deny check` (advisories, bans, licenses, sources) passes — advisories with one documented waiver, see below.

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
2. If the action is `taiki-e/install-action`, bump the `cargo-audit@` / `cargo-deny@` versions deliberately and confirm the new versions are in the action's `manifests/*.json`. A version missing from the manifest falls back to cargo-binstall with a warning, which loses the checksum-verified prebuilt path; a version that does not exist fails the install step.
3. Validate the YAML (see below) and let the PR run the full matrix before merging.

## Run the gates locally

Everything CI runs is a plain command. On a machine with the toolchain installed:

```bash
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"   # TL drift test needs it on Linux/macOS
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
rustup toolchain install 1.89
cargo +1.89 check --workspace --all-targets --locked
cargo audit
cargo deny check
```

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

## Changing the workflow

- Keep every gate on both `push` and `pull_request`. A gate that only runs on `push` is discovered after merge.
- Keep `permissions` at `contents: read` unless a new job demonstrably needs more, and scope the extra permission to that job.
- Pin any new action to a full commit SHA with a `# ref` comment, and add it to the table above.
- Never write `command || other-command` as an install fallback. Install once, fail loudly, and let the next step run the tool it expects.
- Validate the YAML, run the gates, and keep the job count honest: a new check belongs in an existing job unless it needs a different OS or a different tool.
