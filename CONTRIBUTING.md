# Contributing to oly

Thanks for helping improve `oly`.

## Before you start

- Open an issue first for large changes so the direction is clear before you invest time.
- Keep pull requests focused. Small, reviewable changes land faster than broad refactors.
- If your change affects CLI behavior, docs, install flows, or remote supervision, update the relevant documentation in the same pull request.

## Local development

```sh
cargo test
```

Use the existing test suite as the baseline before and after your change.

### Dev state directory (`.dev-state/`)

When running an interactive dev daemon against this repo (e.g. `cargo
run -- daemon start --detach`), set **`OLY_STATE_DIR=./.dev-state`** so
the daemon writes its SQLite db, sockets, logs, and per-session
journal directories **into a single gitignored subdirectory** at the
repo root instead of:

- the system default (`~/.local/state/oly` on Linux) — which works, but
  mixes unrelated data across every clone you happen to be working on;
- the repo root — which works, but litters the working tree with one
  hex-named directory per session you've ever spawned (a fresh
  clone plus a few minutes of testing produces ~30 `<abc1234>/` dirs
  at the root, all gitignored by the `**/???????/` rule but still
  visible in `ls`).

The convention is therefore:

```sh
mkdir -p .dev-state
export OLY_STATE_DIR="$PWD/.dev-state"
cargo run -- daemon start --detach
# …work, run oly start / ls / logs / stop as normal…
cargo run -- daemon stop
```

`.dev-state/` is listed in `.gitignore`, and the existing
`**/???????/` rule continues to exclude per-session `<id>/` subdirs
under it. A single directory at the repo root is the entire dev
fingerprint; `rm -rf .dev-state` returns the checkout to a pristine
state.

The integration test suite (`tests/e2e_daemon.rs`,
`tests/cli_errors.rs`) already creates an isolated `OLY_STATE_DIR`
under `env::temp_dir()` per test, so it never writes into the repo
root and is unaffected by this convention. The same approach is used
by `PERFORMANCE.md`'s benchmark recipe.

If you touch packaging or release behavior, also review:

- `Cargo.toml`
- `npm/package.json`
- `.github/workflows/release.yml`

## Pull request checklist

- Explain the user-visible problem being solved
- Describe the chosen approach and any tradeoffs
- Add or update tests when behavior changes
- Update docs when install, commands, or workflows change

## Good issues to contribute

- Docs that clarify the “managed interactive CLI” story
- Platform packaging improvements
- Remote supervision and notification UX
- Reliability fixes for PTY/session handling
