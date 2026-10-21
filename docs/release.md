# Publish a release

A release is an annotated tag. You bump versions and push the tag. The `release` workflow in `.github/workflows/release.yml` builds the binaries, publishes the GitHub Release, and publishes npm. crates.io waits for an explicit order.

## Check the gates

1. Confirm `main` is green. The `ci` workflow (`.github/workflows/ci.yml`) gates on `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, an MSRV check on Rust 1.89, and `cargo audit`.
2. Confirm that `docs/capabilities.md` has no remaining `want` rows. The publication gate lives in [ADR-005](decisions/005-unpublished-until-want-done.md) and is currently met. Waive nothing silently.
3. Pick the version number. MAJOR breaks the contract, MINOR adds commands or JSON keys, PATCH fixes. The breaking-change definition sits in `docs/cli-contract.md` under "Stability". For why this stack exists, see [ADR-006](decisions/006-rust-grammers.md).

## Bump the version

1. Set `version` in `Cargo.toml` to the tag number without the `v` prefix. Use `0.6.2` for `v0.6.2`. Commit `Cargo.lock` in sync.
2. Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md`. The workflow extracts the section whose heading matches the tag version, so the heading must match exactly. Write entries when you land changes, not at release time.
3. Commit with a `chore:`, `feat:`, or `fix:` prefix and push to `main`.

Never commit `.env`, `*.session`, phones, or api hashes.

## Tag the release

```bash
git tag -a -m "vX.Y.Z" vX.Y.Z
git push origin vX.Y.Z
```

Tags must be annotated (`-a`). Lightweight tags are not valid releases — the workflow and changelog extraction expect an annotated tag object. Verify with `git cat-file -t vX.Y.Z` (should print `tag`, not `commit`).

The push triggers the `release` workflow.

## What the workflow builds

**Build job.** A matrix of 13 targets named by full Rust target triple, matching the ripgrep convention: windows x64 + arm64 (msvc), macOS arm64 + x64, and linux x64/arm64/armv7/i686/riscv64/powerpc64le across gnu and musl. Every Linux target cross-compiles through `cross` (docker images carry the C toolchain for libsql's SQLite); the arm64-musl build is fully static and runs in Termux/Android. Each target produces `tele-<version>-<target-triple>.tar.gz` (`.zip` on windows) plus a `.sha256`. Regenerate the count with `rg -c "target: " .github/workflows/release.yml`, which prints `13`.

**Release job.** Extracts the matching section from `CHANGELOG.md` as the release body and creates the GitHub Release through `softprops/action-gh-release@v2`. The finished release carries 39 assets: 13 archives, their `.sha256` checksum files, and 13 SPDX SBOMs (the windows-arm64 build is best-effort with `continue-on-error`, so a skipped build can drop that set to 36). The 13 build-provenance attestations are attached to the repository's attestation store, not uploaded as release assets — verify them with `gh attestation verify`, not the asset list.

**npm job.** Publishes 1 bundled package (`@qmahyar/telecli`) through OIDC trusted publishing (no token). The single package bundles all 13 target binaries (`tele-<triple>`) plus launcher `bin/tele.js` which picks the correct binary for the current platform; no separate platform packages. The `linux-arm64-musl` binary is static and runs in Termux/Android. The package installs both `tele` (primary) and `telecli` (deprecated alias). The job stages `npm/README.md` and `LICENSE` into the package root so the published tarball carries a README and the MIT license. The job runs in the `npm` GitHub environment — configure that environment once under **Settings → Environments** (required reviewers optional; it exists so the id-token-bearing publish step has a named protection boundary).

## Verify the release

1. Open the GitHub Release page and confirm the asset set: 13 archives + 13 `.sha256` files + 13 SPDX SBOMs (`tele-<version>-<target>.sbom.spdx.json`) — 39 assets (36 when the best-effort windows-arm64 build is skipped). The 13 build-provenance attestations are not release assets; verify them per step 4 with `gh attestation verify`. The count grows additively with the target matrix.
2. Download a binary and smoke-test it. `tele --help` and `tele --dry-run` must respond.
3. Run `npm view @qmahyar/telecli version` and confirm it shows the tag version. Then install and check:

```bash
npm install -g @qmahyar/telecli && tele --version
```

4. Verify build provenance and inspect the SBOM (GitHub CLI v2.61+):

```bash
gh attestation verify --repo mahyartdb/tele-cli tele-<version>-x86_64-pc-windows-msvc.zip
# inspect the dependency inventory:
jq '.packages[].name' tele-<version>-x86_64-pc-windows-msvc.sbom.spdx.json
```

## Handle an npm failure

Publishing authenticates through npm trusted publishing (OIDC). The `@qmahyar/telecli` package on npmjs.com must list this repository and the `release` workflow as its trusted publisher. OIDC tokens are short lived, so there is no secret to set or rotate.

The trusted publisher on npmjs.com is a separate record from the `repository` field in `package.json`. It has to name the repository the workflow actually runs in. Configure it under **npmjs.com → `@qmahyar/telecli` → Settings → Trusted Publisher** with exactly:

| Field | Value |
|---|---|
| Provider | GitHub Actions |
| Organization or user | `mahyartdb` |
| Repository | `tele-cli` |
| Workflow filename | `release.yml` |
| Environment name | `npm` |
| Allowed actions | `npm publish` (and `npm stage publish`) |

The environment name matters: the publish job declares `environment: npm`, so the OIDC subject is `repo:mahyartdb/tele-cli:environment:npm`. A configuration that names a different owner, repository, or environment is rejected at the token exchange, and npm reports it as a publish failure rather than a permission error:

```
npm error code E404
npm error 404 Not Found - PUT https://registry.npmjs.org/@qmahyar%2ftelecli - Not found
```

That 404 means "no trusted publisher matched this OIDC token", not "the package does not exist". Fix the trusted publisher on npmjs.com, then rerun the failed job:

```bash
gh run rerun <run-id> --job <npm-job-id>
```

npm CLI 12 can do the same from a terminal with a granular access token, which avoids the web form:

```bash
npm trust list @qmahyar/telecli
npm trust revoke --id <id> @qmahyar/telecli
npm trust github @qmahyar/telecli --repo mahyartdb/tele-cli --env npm --allow-publish --yes
```

The registry allows only one trusted publisher per package, so revoke the stale one before adding the new. Classic `npm_` tokens are rejected for both publishing and trust management; use a granular access token instead.

The GitHub Release ships even when npm fails. Do **not** run `npm publish` from the repo's `npm/` directory: it contains only the JS launcher, no binaries, and a package published from there always fails at spawn with "no binary bundled". The publish job builds the real package by downloading the 13 build artifacts and staging every binary into `pkg-main/bin/` before invoking npm. To publish by hand instead, reproduce those steps with your own npm login:

```bash
VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
mkdir -p pkg-main/bin
# stage every platform binary from the workflow artifacts (tele-<version>-<triple>.tar.gz / .zip):
#   extract each archive's tele (or tele.exe) to pkg-main/bin/tele-<triple>[.exe]
cp npm/bin/tele.js pkg-main/bin/tele.js
cp npm/README.md pkg-main/README.md
cp LICENSE pkg-main/LICENSE
jq --arg v "$VERSION" '.version = $v' npm/package.json > pkg-main/package.json
cd pkg-main && npm publish --access=public
```

Prefer rerunning the failed job (`gh run rerun <run-id> --job <npm-job-id>`) so the OIDC attestation chain stays intact.

The publish job checks the registry before it publishes and skips the step when the tag version is already there, so re-running a release is safe and does not fail with "cannot publish over previously published version".

## Verify build provenance

```bash
gh attestation verify --repo mahyartdb/tele-cli tele-<version>-x86_64-pc-windows-msvc.zip
```

The attestation carries SLSA v1 provenance and names the source repository, the workflow, and the tag. It resolves for every release published from the current history. Releases published before the October 2026 history rewrite reference pre-rewrite commit hashes that no longer exist, so their provenance cannot be verified against this repository; only releases from `v0.15.0` onward carry a resolvable attestation.

## Order crates.io separately

crates.io publication is not part of this pipeline. It happens only when you explicitly order it, because this is a personal tool. Run `cargo publish` when ordered.

## Roll back

There are no servers to revert. Point users at the previous tag. While nobody depends on the broken release, delete the tag and the GitHub Release, then cut them again. Once a release is out, prefer a SemVer PATCH on top.
