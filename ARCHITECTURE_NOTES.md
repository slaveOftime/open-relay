# Architecture Notes and Edge Cases

Companion reference for `ARCHITECTURE.md` and `ARCHITECTURE_PTY.md`.

This file collects the detailed edge cases, platform quirks, operational
limitations, and lookup notes that are useful during debugging or extension
work, but noisy in the primary architecture narratives.

---

## 1) Architecture-Wide Escape Sequence Edge Cases

This section documents every non-obvious terminal escape sequence problem
discovered during development.

### EC-1: OSC Color Query Blocking (opencode / AI agents)

**Problem**: Some AI coding tools (e.g., opencode) probe the terminal's
foreground/background colors at startup using OSC 10/11 queries:
```
ESC ] 10 ; ? BEL      # query foreground color
ESC ] 11 ; ? BEL      # query background color
```
A real terminal would respond with a color spec.  The daemon is not a terminal,
so there is no response.  The child application blocks indefinitely waiting for
the reply.

**Root cause**: The PTY output path was transparent — it forwarded bytes to the
ring buffer but never synthesized terminal responses.

**Fix**: `extract_query_responses_no_client()` in `src/session/pty.rs`
intercepts these queries and writes a synthetic response to the master fd
before the bytes reach the filtered ring buffer:
- Reads `$COLORFGBG` environment variable if set (common in color-aware
  terminals).
- Falls back to white foreground (`rgb:ffff/ffff/ffff`) and black background
  (`rgb:0000/0000/0000`).

**Source**: `src/session/pty.rs` — `TerminalQuery::ForegroundColor`,
`TerminalQuery::BackgroundColor`.

---

### EC-2: Duplicate `\x1b[?1049h` Blanks Alternate Screen

**Problem**: Both the attach client (via
`crossterm::execute!(stdout, EnterAlternateScreen)`) AND the child process may
emit `\x1b[?1049h`.  The second invocation resets the alternate screen buffer,
wiping any output already drawn.

**Root cause**: The client unconditionally entered the alternate screen on
attach, assuming it must set up terminal state.  But the child already manages
its own screen.

**Fix**: The attach client must **not** call `EnterAlternateScreen`.  The
child's own `\x1b[?1049h` establishes the alternate screen.  The client's
`RawModeGuard` teardown sends `\x1b[?1049l` on detach to cleanly exit
regardless.

**Affected file**: `src/client/attach.rs` — `run_attach_inner`,
`run_attach_polled`.

---

### EC-3: Resize Before Initial Data Race (Blank Screen on Reattach)

**Problem**: A newly attaching client sends `AttachResize` to inform the daemon
of its terminal size.  If this resize arrives before the initial terminal snapshot is
delivered, the child emits `\x1b[2J\x1b[H` (clear screen, cursor home)
followed by a full repaint.  Those bytes arrive as `AttachData` frames **ahead
of** or **mixed with** the initial replay, resulting in a blank or corrupted
initial display.

**Root cause**: Resize and initial data were not sequenced — the client
optimistically sent resize as soon as the IPC connection opened.

**Fix**: The `AttachStreamInit` frame carries `initial_data` (the full ring
replay).  The client writes this replay to the terminal **before** sending
`AttachResize`.  The daemon applies the resize only when it processes the
`AttachResize` message, guaranteeing the replay is already consumed
client-side.

**Affected files**: `src/client/attach.rs` (send resize after parsing init),
`src/daemon/rpc.rs` (emit `AttachStreamInit` with ring data).

---

### EC-4: DECCKM Not Tracked Live (Wrong Arrow Keys in AI Tools)

**Problem**: An attached AI tool enables Application Cursor Keys
(`\x1b[?1h`) after the initial attach handshake.  The daemon captures the
initial mode at attach time via `AttachStreamInit`, but never emits
`AttachModeChanged` while the child is running.  The client's
`child_app_cursor_keys` stays `false`.  When the user presses an arrow key, the
client sends `\x1b[A` (normal mode) but the child expects `\x1bOA`
(application mode) — movement is silently ignored or misinterpreted.

**Root cause**: The mode-change detection in the PTY reader updates
`RuntimeState` correctly, but no IPC signal was wired up to propagate the
change to attached clients.

**Fix**: After `push_output()` updates the mode snapshot, the streaming paths
compare the current `ModeSnapshot` after each forwarded chunk and emit an
`AttachModeChanged` frame when needed.

**Affected file**: `src/daemon/rpc.rs` — live broadcast loop.

---

### EC-5: Bracketed Paste Re-Wrapping

**Problem**: `crossterm` parses `Event::Paste(text)` by stripping the
`\x1b[200~` / `\x1b[201~` markers.  If the child has enabled Bracketed Paste
Mode, it expects those markers — without them, the pasted text may be executed
as commands rather than inserted as literal text.

**Root cause**: `crossterm` strips markers at the event layer.  The attach
client re-emits the text without re-adding them.

**Fix**: When `child_bracketed_paste == true`, the client prepends
`\x1b[200~` and appends `\x1b[201~` before writing the paste bytes to the IPC
`AttachInput` frame.

**Affected file**: `src/client/attach.rs` — input handler.

---

### EC-6: ConPTY Bare CPR Echo

**Problem**: On Windows, ConPTY echoes cursor-position-report responses
(`[35;1R`) without the leading ESC byte back into the master-side output.
These bare sequences pollute the output stream visible to clients.

**Root cause**: ConPTY implementation quirk — it echoes the response to the
master before the child consumes it.

**Fix**: `EscapeFilter` in `src/session/pty.rs` strips these bare CPR sequences
in the PTY reader before they reach the canonical retained stream.

**Source**: `src/session/pty.rs` — `EscapeFilter`.

---

### EC-7: OSC 7 / Generic OSC Echo

**Problem**: Shells and terminal apps send OSC 7
(`\x1b]7;file://hostname/path\x07`) to notify the terminal of the current
working directory.  Some terminals echo this back through the PTY master,
creating feedback loops.  Other OSC sequences may similarly be echoed.

**Root cause**: Echo pass-through from terminal emulator to PTY master.

**Fix**: `EscapeFilter` strips generic OSC sequences (BEL-terminated and
ST-terminated) in the PTY reader before they are retained or broadcast.

**Source**: `src/session/pty.rs`.

---

### EC-8: Partial Escape Sequences Across `read()` Boundaries

**Problem**: The PTY reader thread reads 4 KB chunks.  An escape sequence
(e.g., an OSC string or a multi-byte CPR response) can be split across two
consecutive `read()` calls.  Naive per-chunk processing would fail to recognize
the split sequence.

**Root cause**: POSIX `read()` on PTY master provides no framing — sequences
can span arbitrary chunk boundaries.

**Fix**: Two carry-forward buffers:
- `query_tail: Vec<u8>` in the reader loop — carries the tail of a chunk that
  might be the start of a query sequence.
- `EscapeFilter.pending: Vec<u8>` in `src/session/pty.rs` — carries incomplete
  escape sequences across calls to `filter()`.

Both are reset when the sequence is completed or proven not to be an escape
sequence.

**Source**: `src/session/pty.rs` — `EscapeFilter`; `src/session/runtime.rs` —
`query_tail`.

---

### EC-9: Terminal Left in Bad State After Unexpected Client Exit

**Problem**: If `oly attach` is killed with SIGKILL, or panics, Rust
destructors do not run.  The terminal is left in raw mode (echo disabled,
canonical mode off).  The user sees no input feedback and may not be able to
recover without external intervention.

**Root cause**: Raw mode is a process-level stty setting that survives process
exit on POSIX.  `RawModeGuard::drop` sends a normalization string, but `Drop`
is not called on SIGKILL.

**Mitigation**:
- `RawModeGuard::drop` sends a comprehensive normalize sequence:
  `\x1b[?1049l\x1b[!p\x1b[0m\x1b[?25h\x1b[?1000l...\x1b[?2004l`
- This covers all known mode toggles for a clean detach under normal signals
  (SIGTERM, Ctrl-C, panic unwind).
- For SIGKILL recovery: users must run `stty sane` or `reset` from another
  terminal or SSH session.

**Cross-platform note**: On Windows, `crossterm` restores console mode via its
own `Drop` implementation; ConPTY mode restoration is automatic.

**Source**: `src/client/attach.rs` — `RawModeGuard::teardown_terminal`.

---

### EC-10: Multi-Byte UTF-8 Split Across IPC Socket Reads

**Problem**: Any IPC read could fail with `I/O error: incomplete utf-8 byte
sequence from index N` (typically `N` just under 4096).  The most visible
symptom was `sync lost: …` in the `oly ls --follow` TUI, but every IPC helper
was affected — including the attach stream, so a session whose payload happened
to straddle a boundary could drop the attachment.

**Root cause**: `read_line_bounded` in `src/ipc.rs` decoded each `fill_buf`
chunk with `str::from_utf8` *before* appending it to the output `String`.  The
local socket hands back arbitrary byte counts (~4 KB on Windows named pipes),
so a 2–4 byte UTF-8 character straddling that boundary was rejected as
`Utf8Error { error_len: None }`.  Only payloads that were both large **and**
contained non-ASCII text (CJK titles, box-drawing characters, emoji, Windows
paths with accents) tripped it, which is why it looked intermittent.

**Fix**: Accumulate the raw bytes in a `Vec<u8>` and decode **once**, after the
newline terminator (or EOF) is reached.  Genuinely invalid UTF-8 is still
rejected with `ErrorKind::InvalidData`, and the `MAX_IPC_LINE_BYTES` cap is
still enforced against the accumulated length before each `extend_from_slice`.

**Source**: `src/ipc.rs` — `read_line_bounded`.

---

### EC-11: Inline Session View Leaked Into the Main Screen Buffer

**Problem**: Pressing Enter in `oly ls --follow` handed the terminal to a child
`oly attach` / `oly logs` on the **main** screen buffer.  The child's output was
interleaved with the surrounding shell scrollback, so scrolling during (or
after) an inline session showed unrelated content.

**Root cause**: `TuiTerminal::suspend` called `LeaveAlternateScreen` before
spawning the child, on the assumption that the child needs the main buffer.  It
does not — per EC-2 the attach client deliberately never enters the alternate
screen, so it simply renders wherever the parent left the terminal.

**Fix**: `suspend` now stays on the alternate screen and only releases raw mode,
clearing the buffer and homing the cursor so the child starts clean.  `resume`
re-issues `EnterAlternateScreen` unconditionally, because the child's
`RawModeGuard` teardown emits `\x1b[?1049l` (EC-9) which drops the terminal back
to the main buffer even though the list TUI never left it.

**Source**: `src/client/list_tui.rs` — `TuiTerminal::suspend` / `resume`.

---

## 2) Architecture-Wide Limitations and Operational Notes

### Terminal Restoration on SIGKILL (All Platforms)

Rust `Drop` implementations do not run on `SIGKILL`.  Raw mode is not
restored.  Users must run `stty sane` (Linux/macOS) or close and reopen the
terminal (Windows).

### Windows ConPTY Quirks

- ConPTY echoes bare CPR responses (`[35;1R`) into the master output stream.
  These are stripped by `BARE_CPR_RE`.
- ConPTY does not support all xterm extensions (e.g., mouse reporting beyond
  basic).
- `SIGWINCH` does not exist on Windows; `portable_pty` translates resize events
  to ConPTY API calls.
- Named pipes (IPC) have different path conventions: `\\.\pipe\oly-<user>`.

### macOS Terminal.app

- Does not set `$COLORFGBG` by default.  The color-query fallback
  (white-on-black) is used.
- `EnterAlternateScreen` from the client side causes double invocation of
  `\x1b[?1049h` — the attach client must not call it (see EC-2).

### Escape Sequence Fragmentation

Large bursts of PTY output can send escape sequences across `read()` chunk
boundaries.  The carry-forward buffers (`query_tail`, `EscapeFilter.pending`)
handle known sequences, but novel sequences from future terminal capabilities
may not be handled.

### Multi-Client Input Multiplexing

All attached clients write to the same PTY master.  Concurrent input from
multiple clients interleaves at the byte level.  This is correct for automation
(only one client sends input) but may be confusing for human multi-attach
scenarios.  There is no UI indication of which client is typing.

### Ring Buffer Overflow

If a session emits more than `ring_capacity_bytes` of output before any client
attaches, early output is lost.  `output.log` always has the full history.

### Federation Latency

Each input/output hop through a secondary node adds one WebSocket round-trip.
High-throughput sessions (e.g., video in terminal) may lag noticeably over
high-latency links.

### Auth Bypass Risk on Loopback-Only Deployments

If `bind = "127.0.0.1"` and `auth_enabled = false`, any local process can
control sessions.  This is intentional for trusted developer environments but
must not be used on shared machines.

---

## 3) Agent and Operator Lookup Notes

When an agent needs context, start from `ARCHITECTURE.md`, then only open
targeted files listed below for the exact flow being changed.

### Practical Lookup Map

| Problem area | Files to open |
|---|---|
| Session lifecycle bug | `src/session/runtime.rs`, `src/session/store.rs`, `src/daemon/lifecycle.rs` |
| IPC protocol bug | `src/daemon/rpc.rs`, `src/ipc.rs` |
| CLI attach behavior | `src/client/attach.rs`, `src/client/input.rs` |
| Terminal mode tracking | `src/session/runtime.rs` (`push_output`), `src/client/attach.rs` (mode tracking) |
| Escape sequence handling | `src/session/pty.rs` (`EscapeFilter`, `extract_query_responses_no_client`) |
| API behavior bug | `src/http/*.rs`, `src/db.rs` |
| Web terminal/UI bug | `web/src/pages/SessionDetailPage.tsx`, `web/src/components/XTerm.tsx` |
| Key input translation | `web/src/utils/keyInput.ts`, `src/client/attach.rs` |
| Federation issues | `src/node/*`, `src/client/join.rs`, `src/http/nodes.rs` |
| Push notifications | `src/notification/*` |
| Cross-platform / PTY | `src/session/runtime.rs`, `portable_pty` crate docs |

---

## 4) PTY Cross-Platform Edge Cases

### ConPTY (Windows)

| Issue | Impact | Mitigation |
|---|---|---|
| Echo of device responses | CPR/DSR/OSC responses appear in master output | `EscapeFilter` strips them once in the PTY reader before retention and fan-out |
| DSR query forwarding | Queries like `\x1b[6n` forwarded to attach clients cause CPR response feedback loop | `EscapeFilter` strips DSR queries in the canonical filtered stream |
| ESC split across reads | Sequences split at arbitrary byte boundaries | `pending` field in `EscapeFilter` carries fragments |
| No SIGWINCH | ConPTY uses `ResizePseudoConsole()` internally | `portable_pty` abstracts this |
| No SIGCHLD | Child exit detected via `WaitForSingleObject` | `try_wait()` polls periodically |
| Process group semantics | No `setsid()` / process groups | ConPTY manages child lifetime |
| Color response format | May differ from POSIX terminal | Static fallback in `terminal_report_colors()` |

### POSIX PTY (Linux / macOS)

| Issue | Impact | Mitigation |
|---|---|---|
| File descriptor leaks | Child inherits daemon's fds | `portable_pty` sets up PTY handles with `CLOEXEC` semantics |
| Zombie processes | Parent must `waitpid()` after SIGCHLD | `try_wait()` polled in completion check loop |
| Signal during fork | SIGCHLD between fork/exec can confuse child | `portable_pty` blocks signals during fork |
| PTY master close race | Close master while child is writing → SIGPIPE | Reader thread detects `read() == 0` and breaks cleanly |
| macOS PTY buffer size | Smaller than Linux (4KB vs 16KB) | No special handling needed; affects throughput only |
| EMFILE/ENFILE in accept | FD exhaustion prevents new connections | Back off and retry with exponential delay |

### Encoding Assumptions

- PTY output is treated as raw bytes, not decoded as UTF-8
- `String::from_utf8_lossy` used only in filter functions that need regex
- JSON framing uses base64 for binary data, preserving all bytes
- Cross-chunk `pending` buffers in `EscapeFilter` operate on bytes (`Vec<u8>`)
  so non-UTF-8 PTY output is preserved exactly
