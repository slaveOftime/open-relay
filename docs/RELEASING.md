# Release preparation

This is a checklist, **not a declaration that a release is ready**. Version
selection, tagging, registry credentials, and publication require maintainer
approval. Do not infer a stable release from a development branch name.

## Release contract

The workflow in [`.github/workflows/release.yml`](../.github/workflows/release.yml)
uses the effective tag (push tag or manual input):

| Tag shape | GitHub | npm | crates.io / Homebrew |
|---|---|---|---|
| `vX.Y.Z-beta.N`, `vX.Y.Z-rc.N` | Prerelease | `beta` | Skipped |
| `vX.Y.Z` | Stable release | `latest` | Publish / update default-branch formula |

A branch called `v0.5.0` is **not** a prerelease tag. The source manifest is
currently `0.5.0`; a preview needs an explicit prerelease suffix. Choose the
release version deliberately; do not bump to 1.0 just for marketing if the
protocol and upgrade contract are still changing.

The workflow runs `scripts/release-version.py` to derive the root version in
**both** `Cargo.toml` and `Cargo.lock` from the tag before locked tests/builds or
publication. It rejects invalid tags and pre-existing root-version mismatches,
changes no dependency versions, and is repeatable. Source manifest/lock versions
must match each other, but need not already include the prerelease suffix.

## Before tagging

- [ ] Choose version/channel and record scope, breaking changes, and known limits.
- [ ] Ensure `Cargo.toml` and the `oly` entry in `Cargo.lock` match. Verify the
      tag-derived transformation with offline release tests and locked Cargo commands.
- [ ] Confirm fresh installs and upgrades use the intended npm scoped package,
      Cargo crate, and Homebrew formula. npm's source placeholder version is
      replaced by the workflow; Homebrew hashes must come from actual artifacts,
      never guesses.
- [ ] Reconcile README channel notice and migration wording when promoting
      0.5 to stable; ensure published package links and assets are usable.
- [ ] Write user-facing release notes: why upgrade, incompatibilities, backup /
      rollback, changed flags, and supported platforms. Generated commit notes
      alone are not the migration story.
- [ ] Confirm required secrets/permissions (`NPM_TOKEN`, `CARGO_REGISTRY_TOKEN`,
      release uploads, default-branch formula update); check registry version
      availability and retry behavior. Do not overwrite an existing release.

## Automated evidence

Run from a clean checkout with Rust, Node.js/npm, and the relevant native build
prerequisites. Preserve the results by platform/version:

```sh
python3 tests/docs_sync.py
python3 tests/release_workflow.py
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --release --locked
```

Windows PTY tests run serially (`cargo test --locked -- --test-threads=1`).
Frontend gates mirror `ci-web.yml`: `npm ci`, lint, format check, unit tests,
build, and Chromium E2E from `web/` with its documented Playwright setup.
The release build itself runs npm install/build to embed the web UI; coordinate
shared build/formatter ownership and inspect any resulting lockfile changes.

Offline docs/workflow tests validate contracts and benign fixture packaging;
they **do not** prove real binaries, registries, authentication, or all platforms.

## Manual smoke tests on shipped artifacts

Use temporary `OLY_STATE_DIR` and a unique `OLY_SOCKET_NAME` on Windows; never
reuse the operator's daemon. Use benign helpers and coordinate port ownership.

- [ ] Fresh install: version/help/embedded skills; first-run auth and actual UI assets.
- [ ] Session: launch, read tail/screen/info, send text/keys, observe the child
      response, detach, reattach, resize, stop, inspect exit code, restart, remove.
- [ ] Main-screen scrollback and alternate-screen TUIs: no duplicated history,
      correct mouse/paste behavior, observer never drives input/geometry.
- [ ] Multiple controllers: coordinated concurrent input and last-resize behavior.
- [ ] Byte continuation/export: page chaining through exit, partial UTF-8/control
      sequences, timeout cursor preservation, stale-session fencing, slow clients.
- [ ] Upgrade/rollback from backed-up 0.3 state: explicit old-recording rejection,
      metadata preserved, obsolete credentials removed, no old binary on migrated DB.
- [ ] Two-node federation: independent public-key pins, good join, wrong pin/name
      rejection, remote info/logs/input/attach, disconnect/reconnect without local
      daemon/session loss.
- [ ] Browser auth/logout, origin checks, remote tunnel/proxy, notifications,
      and an app route if included in release scope.
- [ ] Windows suspend/resume soak test. The named-pipe disconnect regression is
      useful but is **not** evidence of actual laptop sleep/resume reliability.
- [ ] Disk pressure/retention: explicit failures, protected incarnations, soft
      byte-budget behavior, age deletion only on isolated stopped-session data.

## After authorized publication

- [ ] Verify all three platform archives, per-asset hashes, and aggregate checksum.
- [ ] Install the published npm package on each supported platform; confirm bundled
      binary selection, executable permissions, UI assets, and reported version.
- [ ] Confirm npm dist-tag, GitHub prerelease/latest state, and (stable only)
      crates.io and Homebrew version/hash. Never assume all parallel jobs succeeded.
- [ ] Link release notes/migration from the stable README and announce known limits.
- [ ] Report passed, failed, skipped, and manual-only checks separately.
