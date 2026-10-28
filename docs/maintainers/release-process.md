# How to publish an oly release

For maintainers. Upgrading an installation? Use [Migration to 0.5](../releases/migration-0.5.md).
For release status and evidence, see the [versioned release notes](../releases/release-notes-0.5.0.md).

## 1. Choose the version and channel

The **tag**, not the branch name or GitHub release checkbox, determines publication:

| Tag | GitHub | npm | crates.io / Homebrew |
|---|---|---|---|
| `vX.Y.Z-beta.N` or `vX.Y.Z-rc.N` | Prerelease, not latest | `beta` | Skipped |
| `vX.Y.Z` | Stable, latest | `latest` | Publish / update formula |

For example, fixes after `v0.5.0-beta.2` can ship as `v0.5.0-beta.3`.
Promotion to `v0.5.0` is a separate maintainer decision, not an automatic step.
Confirm registry credentials (`NPM_TOKEN`, and stable-only `CARGO_REGISTRY_TOKEN`)
and stable-only permission to push the Homebrew update to the default branch.

## 2. Prepare and merge a PR

- Write versioned release notes at
  `docs/releases/release-notes-<version>.md` (for example,
  `release-notes-0.5.0-beta.3.md`): why upgrade, breaking changes, migration
  link, known limits, and verification. Keep feature highlights separate from
  migration. The Release workflow publishes this file as the GitHub release
  body, so commit it before tagging; a missing file fails the release job
  before a GitHub release is created.
- Keep `Cargo.toml` and the root `oly` entry in `Cargo.lock` at the same version.
  For a **stable** release, commit the intended version in both before tagging:
  `cargo publish --locked` rejects uncommitted version changes. A beta may use
  the base source version (for example, `0.5.0` for `v0.5.0-beta.3`).
- Validate the updated PR head, not an older green commit. Review source/config,
  docs/workflow contracts, Rust tests/Clippy, and frontend tests/build/Chromium.
- Test shipped artifacts and upgrade/rollback with isolated or backed-up state.
  Include actual Windows sleep/resume and mobile-device checks when applicable.
  Record automated passes, maintainer reports, skipped checks, and remaining
  limits separately; CI alone does not prove these physical-device behaviors.

```sh
python3 tests/docs_sync.py
python3 tests/release_workflow.py
gh pr checks <PR-number> --watch
# Merge using repository policy; do not bypass protections.
gh pr merge <PR-number> --merge --match-head-commit <verified-head-SHA>
```

Before promoting a 0.5.x release, review [Compatibility for 0.5](../releases/compatibility-0.5.md#gates-for-05x-releases)
and update README channel/migration wording deliberately.

## 3. Tag the merged commit

```sh
git switch main
git pull --ff-only origin main
# Verify this SHA is the intended merged release commit.
git log -1 --oneline
git tag -a v0.5.0-beta.3 <merged-SHA> -m "oly 0.5.0-beta.3"
git push origin refs/tags/v0.5.0-beta.3
```

Do not move or reuse a published tag. No manual GitHub release creation is
required: pushing the tag starts [the Release workflow](../../.github/workflows/release.yml).
It creates/updates the GitHub release from the tag's versioned notes file;
commit-generated notes are not used. Creating or editing a GitHub release is
**not** a configured workflow trigger; if using the website, explicitly run
Actions → Release with an existing tag.

### What the automation changes

1. Every platform checks out the exact tag. `scripts/release-version.py` sets
   the root Cargo manifest/lock version from it, without changing dependencies.
2. Locked tests and release builds run for Linux x64, Linux arm64, macOS Apple
   Silicon, and Windows x64 (Windows tests run serially). Linux arm64 builds on
   a native `ubuntu-24.04-arm` runner, so the artifact is the binary the suite
   actually exercised; there is no cross toolchain to configure.
3. `scripts/release-package.py` creates ZIPs and SHA-256 sidecars. After all
   platform builds finish, one release job attaches the archives, their
   checksums, and `SHA256SUMS.txt` to a single GitHub release. The body is
   rendered by `scripts/release-notes.py` from
   `docs/releases/release-notes-<version>.md`, with relative links pinned to
   the tag.
4. npm's version is set from the tag; the four built binaries are bundled and
   published to `beta` or `latest`.
5. Stable tags also publish the crate and commit the Homebrew version/hashes.
   The formula deliberately covers only macOS Apple Silicon and Linux x64;
   `npm/bin/oly.js` is the launcher that maps every supported platform, and a
   release regression asserts the formula never names an asset the matrix does
   not build.

Cargo/npm version edits happen **only inside release checkouts**, not as version
commits back to `main`. The stable-only Homebrew job commits only the formula.

## 4. Verify, update notes, and announce

```sh
gh run list --workflow Release --limit 3
gh run watch <run-ID> --exit-status
gh release view <tag>
npm view @slaveoftime/oly dist-tags --json
```

Download all three archives and verify their hashes against `SHA256SUMS.txt`.
Install the published package, inspect its UI/skills, and verify binary version
and platform selection. For stable releases also verify crates.io and Homebrew;
for beta releases confirm stable channels were left unchanged.

The release body is the tag's versioned notes file, with relative repository
links pinned to the tag. If a published body needs correction, render that
tag's notes (`python3 scripts/release-notes.py <tag> --from-git --output
body.md`) and update it with `gh release edit <tag> --notes-file body.md`.
Do not leave relative links in a release body, and do not link newly moved docs
to an old tag that still has their old paths. Announce the channel, migration
warning, and known limits only after verifying publication.

## If publication fails

Inspect failed jobs first: publication is not atomic, and some registries or
assets may already have succeeded. Prefer `gh run rerun <run-ID> --failed`.
If a full retry is necessary, use Actions → Release → Run workflow with the
**existing immutable tag**, or `gh workflow run release.yml --ref main -f tag=<tag>`.

A rerun is **not guaranteed idempotent**: npm/crates versions cannot be
republished, and downloaded workflow artifacts expire after one day. Confirm
what succeeded and which artifacts remain before retrying. Never retag or
publish a different binary under an existing version to conceal a failure.
