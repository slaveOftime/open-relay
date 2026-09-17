# 0.x capability and surface inventory (M0)

Baseline commit `931d7b9`, crate 0.3.3. This inventory feeds the 1.0
compatibility break (PLAN.md §14) and the ADRs: anything not listed here as
**keep** is a candidate for removal or replacement. Grounded in
`src/cli.rs`, `src/config.rs`, `src/protocol.rs`, `src/http/`, `src/session/`.

## Terminal capabilities actually provided today

| Capability | 0.x behavior | 1.0 disposition |
|---|---|---|
| Screen model | `vt100` parser, main + alternate screen, scrollback rows (`screen_scrollback_rows`) | Keep concept; engine decision per [ADR-0001](adrs/ADR-0001-terminal-engine.md) |
| Resize | Rebuild parser from width-trimmed formatted rows; panics reset parser | Replace — destructive (repro in `screen.rs`) |
| Queries | CPR/DSR + OSC 10/11 answered (wrong position); DA/XTVERSION/DECRQM/kitty filtered unanswered | Centralized broker at exact positions (ADR-0001) |
| OSC 10/11 set, OSC 0/1/2 title, OSC 9;4 progress | Tracked as session signals metadata | Keep as modeled session signals |
| OSC 8 hyperlinks, most other OSC, DCS/APC/PM/SOS | Stripped from stream | Capability profile decides; no blind strip/forward |
| Input mapping | Incomplete `map_key_to_input`; Ctrl-D = detach; DECCKM arrow rewrite incl. raw/paste; paste via timing (30/150 ms); focus reports discarded | Replace (ADR-0003): raw/semantic split, prefix detach |
| Snapshot restore | `state_formatted` + signals restore bytes | Replace with checkpoint + renderer adapters (ADR-0001) |
| Attach history seed | Bounded seed into host scrollback (≥1000-row floor ≤ session retention) | Keep bounded seed; full history via explicit view (ADR-0005) |

## CLI surface (`src/cli.rs`)

Commands: `daemon {start|stop|status}`, `start`, `update`, `notify
{disable|enable|send}`, `skill`, `ls`, `restart`, `stop`, `rm` (alias
`delete`), `attach`, `logs`, `send`, `apikey {add|ls|remove}`, `join
{start|stop|ls}`, `node {ls|...}`.

Disposition: command names are not the 1.0 machine API. PLAN §9.3 defines
the proposed surface (`start --json --size`, `observe`, `screen`,
`history`, `wait`, `control acquire`, `send --lease`). Existing commands
map or break per the §14 migration; `oly history` is new.

## Config surface (`AppConfig`)

Keep semantics, re-version the file (§14 step 6): `http_bind`,
`http_port`, `log_level`, `stop_grace_seconds`, `prompt_patterns`,
`silence_seconds`, `notification_min_interval_seconds`,
`session_eviction_seconds`, `max_running_sessions`, `notification_hook`,
`runtime_overrides`, web-push vapid keys/proxy.

Replace: `max_output_log_bytes` semantics (truncation → segmented
retention), `screen_scrollback_rows` ownership (moves into the terminal
profile), derived paths (`state_dir`, `sessions_dir`, `db_file`,
`lock_file`, `info_file`, `socket_file`) — reorganized under the new state
namespace.

## Storage (per session dir)

`output.log` (filtered PTY bytes), `events.log` (lifecycle/resize text
records), log index sidecars. → Segmented journal + checkpoints + indexes
(ADR-0002). Legacy import is provenance-labeled (§14 step 4).

SQLite: session metadata, node registry, API keys. Keep; schema versioned.

## Protocol surface

`PROTOCOL_VERSION = 11`; magic `ONW1`; node gzip-compressed JSON frames;
binary server→browser WS output; newline-JSON IPC. → One versioned
major/minor stream protocol with binary data frames (ADR-0004).
Hard-bumped; no bridge.

## Auth surface

Daemon password → Argon2 hash; browser token =
HMAC-SHA256(password-hash, label), no expiry/revocation; API keys for
agents; node ECDH + AES-GCM session crypto; query-token path for WS/SSE
upgrades. → Random revocable expiring sessions + scoped credentials
(ADR-0007). Existing node transport crypto stays under review.

## Notification surface

Silence-based `input_needed` detection (epoch comparison), OS notifications
(`notify-rust`), web push (VAPID), `notification_hook` shell hook. Keep
integration; reframe semantics as `likely_input_needed` with
reason/time/cursor evidence (PLAN §9.3).

## Federation surface

Node registry (pending RPC: one-shot + bounded stream), shared receive
loop, per-stream channels, node-compressed JSON. Keep routing/crypto
concepts; fix head-of-line blocking, add credits/deadlines/generation
fencing (ADR-0004).

## Deferred/not in scope for 1.0 (PLAN §1)

Panes/multiplexer UX, desktop terminal app, distributed scheduler, CRDT
collaboration, mandatory external infra, graphics protocols beyond the
declared profile, process-surviving daemon upgrades (ADR-0006), thousands
of sessions.
