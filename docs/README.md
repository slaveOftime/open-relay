# Documentation

## Using oly

| Document | Purpose |
|---|---|
| [CLI guide](./guides/cli.md) | Sessions, input, attach, logs, cursors, and export |
| [Remote access](./guides/remote-access.md) | Browser access, nodes, notifications, and state |
| [Agent skills](../skills/oly/SKILL.md) | Embedded guidance; includes worker, daemon, and app skills |

## The 0.5 release line

| Document | Purpose |
|---|---|
| [Upgrade from 0.3.x to 0.5.x](./releases/migration-0.5.md) | Breaking changes, required actions, backup, and rollback |
| [What's new in 0.5.x](./releases/whats-new-0.5.md) | Features and improvements, separate from migration |
| [Compatibility for 0.5](./releases/compatibility-0.5.md) | Supported contracts, node federation trust limits, and private interfaces |
| [0.5.0-beta.1 release notes](./releases/release-notes-0.5.0-beta.1.md) | Published scope, known limits, and verification |

## Contributing and maintaining

- [Contributing](../CONTRIBUTING.md) — development setup and review expectations
- [How to publish a release](./maintainers/release-process.md) — maintainer steps,
  tag-derived versions, channel selection, verification, and retries
- [Architecture](../ARCHITECTURE.md) — implementation and invariants
- [Performance](../PERFORMANCE.md) — measurement and benchmark recipes

Use lowercase, hyphenated filenames. `guides/` contains ongoing user guides,
`releases/` contains version-scoped upgrade/feature/compatibility documents and
exact-version release notes, and `maintainers/` contains publishing procedures.
Keep implementation references at the repository root and agent instructions
under `skills/`; link to them rather than copying them into each release doc.
