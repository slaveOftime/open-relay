# 0.5.0-beta.1 — persistent sessions, inspectable work, shared control

**Preview release. Stable 0.3 remains available. Back up state before upgrading.**

## Why try it

Run your existing interactive CLI in a daemon-owned PTY, leave it working, inspect
its output from a terminal/browser, and intervene without keeping one terminal
window open. The 0.5 design has one journal recording, one authoritative backend
terminal model, and a shared cursor-checked attach contract.

Highlights compared with 0.3:

- Journal-backed raw recording and derived canonical display bytes; bounded
  continuation pages, opaque cursors, fixed-prefix export, and explicit errors.
- Rendered tails, live/final screen inspection, sampled screen history, and waits
  that separate attention/completion gates from the requested output view.
- `oly info` for full session/process/activity/terminal/storage inspection and JSON.
- Compact live dashboard with working-directory tree, metadata editing, and cloning.
- Shared concurrent controllers, view-only observe mode, improved paste/mouse/
  scrollback handling, and Windows IPC fixes.
- Experimental pinned-identity federation with encrypted readiness and replay-checked
  application traffic, without an HTTP machine-key fallback.
- Bundled agent skills for session and worker supervision, config, and trusted apps.

## Intentional breaks and scope reductions

- Old `output.log`/`events.log` recordings are rejected, not converted. Metadata
  migrates forward; obsolete API-key/SSH-key credential tables are dropped.
- Upgrade CLI/daemon and every federation node together (IPC protocol 14).
  Re-authorize public identities and recreate joins. There is no SSH import or TOFU.
- Use current log flags and `export`; obsolete read flags are not accepted.
- Live journal byte-cap keys (`max_output_log_bytes`, `max_journal_bytes_per_session`)
  are removed with explicit errors: older-incarnation deletion could not bound
  an ordinary long-running session. Age cleanup for stopped sessions remains opt-in.
- Existing malformed config files/invalid regexes fail startup; reload preserves
  the last good snapshot. Unknown keys warn; resource/duration values are validated.
- Custom apps, proxies, and wwwroot overrides are off by default. Enable only
  reviewed operator code with `trusted_apps_enabled: true`; proxy entries accept
  loopback IP literals only. Apps share the control-plane origin and are not sandboxed.

## Install and upgrade

```sh
npm install -g @slaveoftime/oly@beta
oly --version
```

Or select the platform archive on GitHub Releases and verify its SHA-256 checksum.
Preview publication does not update crates.io or the stable Homebrew formula.

Finish live work, stop the old daemon, back up its entire state, install the
intended preview, then verify a fresh benign session. Do not run old binaries
against migrated state. See [Migration](../MIGRATION.md) for exact steps and rollback.

## Known boundaries

- Client disconnect does not stop the child; daemon stop/crash or reboot does.
  Recordings are not process recovery, and `restart` starts a new run.
- Prompt/idle and successful send are not proof of task completion or approval.
- Multiple controllers can race; coordinate input and resizing.
- No live recording disk quota is implemented. Use OS quotas; retain independent backups.
- Federation has no forward secrecy and is experimental; no mixed-version rollout.
- Windows named-pipe regression coverage does not substitute for a real suspend/
  resume soak test. Browser/mobile behavior also needs actual artifact smoke testing.
- Supported bundled binaries: Linux x64, macOS Apple Silicon, Windows x64.

See [Compatibility](./COMPATIBILITY.md) and [Release preparation](./RELEASING.md).
These notes describe intended release content; a maintainer must attach actual
CI/artifact evidence before publication, not mark unchecked gates as passed.
