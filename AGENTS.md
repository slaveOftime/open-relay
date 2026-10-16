# Agent guide

## Project basics
- `oly` supervises interactive CLI programs in daemon-owned PTYs with recorded output.
- Rust backend/CLI lives in `src/`; React + TypeScript + Vite + xterm.js UI lives in `web/`.
- Release builds embed the web UI and require Rust and Node.js/npm.
- Client disconnect preserves the child; daemon stop/crash or reboot does not.

## Before starting a task
- Inspect `git status` and relevant source/tests; preserve unrelated changes.
- Read [CONTRIBUTING.md](./CONTRIBUTING.md) for development setup and expectations.
- Daemon/native/CLI/protocol work: read [ARCHITECTURE.md](./ARCHITECTURE.md) first.
- Web UI work: read [web/FRONTEND.md](./web/FRONTEND.md); styling follows [web/DESIGN.md](./web/DESIGN.md).
- Web commands, layout, and browser tests: [web/README.md](./web/README.md).
- User guides, upgrades, features, and compatibility: [docs/README.md](./docs/README.md).
- Packaging/version/publication work: [release process](./docs/maintainers/release-process.md).

## Where code lives
- `src/cli.rs`, `src/client/`: command parsing and client operations.
- `src/daemon/`, `src/http/`, `src/node/`: RPC, HTTP/WebSocket, and federation.
- `src/session/`, `src/terminal/`: PTY lifecycle, journal, replay, logs, and terminal model.
- `web/src/`: UI; `tests/` and `web/e2e/`: integration/browser coverage.
- `skills/`: agent guidance embedded by `oly skill`; keep it aligned with behavior.

## Coding principles
- Make small, focused changes; verify assumptions against source and tests.
- Preserve one journal, one authoritative backend terminal model, and one attach contract.
- Reuse existing modules and UI primitives; keep domain logic separate from rendering.
- Add regression tests for behavior changes; update relevant docs and skills together.
- Treat child output as untrusted data; do not weaken auth or trust boundaries for convenience.
- Use isolated daemon state (`OLY_STATE_DIR=./.dev-state`); never disrupt the operator's daemon.
- Verify observable results, not just input delivery or idle state; report skipped/untested checks.
- Do not tag, publish, or change release channels without maintainer authorization.

## Documentation maintenance
- Keep root `README.md` under 160 lines; use `docs/README.md` as the documentation index.
- Use lowercase, hyphenated filenames: `docs/guides/` for usage, `docs/releases/` for versioned docs, `docs/maintainers/` for procedures.
- Keep migrations focused on breaking changes and upgrade/rollback; put enhancements in what's-new docs and shipped scope in exact-version release notes.
- Update architecture/frontend guidance and embedded skills with related changes; distinguish verified results from maintainer reports and untested claims.
- When moving or adding docs, update the index, all links, and doc-test references; avoid hard-coded doc filenames in runtime errors.

## Validation
- Start with relevant tests, then expand coverage according to the changed behavior.
- Rust: `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `cargo test --locked`.
- Windows PTY tests: run serially with `cargo test --locked -- --test-threads=1`.
- In `web/`: `npm ci`, `npm run lint`, `npm run format:check`, `npm test`, `npm run build`.
- UI interaction/effect/focus changes: run relevant Playwright specs; compilation alone is insufficient.
- Docs/packaging: `python3 tests/docs_sync.py` and `python3 tests/release_workflow.py`. Both assume a POSIX host (forward-slash paths, `bash`, `sha256sum`), so run them in WSL/Linux or rely on CI.

## CI layout
Workflows are scoped by change area, so a PR only runs what its diff can break:
- `CI` (`ci.yml`) — Rust: fmt, clippy, the three-OS test matrix, audit, and the release-workflow regressions.
- `CI-WEB` (`ci-web.yml`) — `web/**`: lint, vitest, build, Playwright.
- `CI-DOCS` (`ci-docs.yml`) — Markdown, `docs/`, `skills/`, `src/**`: the offline documentation contracts.

`CI-DOCS` is separate from `CI` because `docs_sync.py` cross-checks `src/**` and `skills/**` against the docs, so a docs-only change must be checked without waking the Rust matrix. `CI-WEB` covers the web bundle, but `CI` also lists `web/**`: `build.rs` runs the npm build for release profiles and `src/http/mod.rs` embeds `web/dist`, so only `CI` proves the bundle is still embeddable — mirror `build.rs`'s web `cargo:rerun-if-changed` entries there. When a new top-level directory becomes a compile-time input (`include_str!`, `sqlx::migrate!`, a `rust-embed` folder, or a `build.rs` resource), add it to the `ci.yml` trigger lists: an omitted input is a silent coverage gap, not a failure. Every workflow also accepts `workflow_dispatch`, which ignores filters.

