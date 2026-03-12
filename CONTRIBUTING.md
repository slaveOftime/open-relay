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
