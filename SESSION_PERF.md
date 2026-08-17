# Session Throughput Rewrite — Windows Verification Handoff

**Status**: complete and verified on Linux. **Unverified on Windows and macOS.**
**Date**: 2026-08-17
**Base commit**: `81ffff4` (`feature/general-improvement`)

This document exists so a future agent (or human) can pick up the work, understand
what changed and why, and complete the platform verification that could not be done
in the original Linux-only environment.

If you only read one section, read [§5 What Needs Verifying on Windows](#5-what-needs-verifying-on-windows).

---

## 1) Why the work happened

Pasting a large block of text into an attached session was very slow.

The cause was measured, not guessed. The PTY reader thread used to run roughly
**twenty sequential `regex::bytes::Regex::replace_all` passes** over every chunk it
read, each one allocating a fresh `Vec<u8>`, and then walked the same bytes **twice
more** — once to extract terminal capability probes, once for activity accounting.

Measured baseline on 16 MB of synthetic TUI output:

| Stage | Throughput |
|---|---|
| `filter_cpr_chunk_bytes` @4 KiB chunks | 49.8 MB/s |
| `extract_query_responses_no_client` | 34.1 MB/s |
| `vt100::Parser::process` | 113 MB/s |
| `TerminalSignals::observe` | 353 MB/s |
| persist (`output.log` append) | 1.7–2.1 GB/s |

The escape filter and the probe extractor dominated everything else by an order of
magnitude. Because a child process **echoes every pasted byte back** through the PTY
master, every single pasted byte paid that ~20–35 MB/s cost. That is the slowness.

---

## 2) What changed

### The centrepiece: `src/session/scan.rs` (new, ~1105 lines)

One byte-level VT state machine, `PtyScanner`, that walks each chunk **exactly once**
and simultaneously produces all four things the reader needs:

```
ScanOut {
    filtered:       Vec<u8>,           // canonical stream: rendered, retained, persisted, broadcast
    queries:        Vec<TerminalQuery>,// probes needing a daemon-generated reply
    progress_bytes: usize,             // forwarded but not counted as screen activity
    // plus changed-signal flags, retrieved via take_changed_signals()
}
```

Key properties:

- **Bulk plain-text copy.** `memchr` (Unix) / `memchr3` (Windows) finds the next byte
  that could introduce a sequence; everything before it is copied in one
  `extend_from_slice`. Almost all bytes take this path.
- **Zero steady-state allocation.** The reader keeps one `ScanOut` alive and reuses
  its buffers across every chunk.
- **One carry-forward buffer.** `PtyScanner.pending` holds an incomplete sequence
  split across a `read()` boundary. Capped at `MAX_PENDING_ESCAPE_BYTES` (4 KiB) so
  plain text that merely *looks* like the start of a sequence can never stall the
  stream. The old design needed two such buffers (`query_tail` and
  `EscapeFilter.pending`) kept manually in sync.
- **Cheap change detection.** `take_changed_signals()` returns `Some` only when
  something actually changed, so for nearly every chunk the reader skips all signal
  work while holding the write lock.

### `src/session/pty.rs` — rewritten, 1952 → 513 lines

Deleted: the regex cascade, `EscapeFilter`, `extract_query_responses_no_client`,
`find_next_terminal_query`, and the whole OSC helper zoo.

Kept and slimmed: `PtyHandle`, `RuntimeChild`, `TerminalQuery` (now `Copy`, with a
`response(cursor)` method), `TerminalSignals` (gained `record_osc` and
`set_cursor_style`, both returning change flags), colour helpers, `collect_chunk_bytes`.

### The new reader pipeline (`src/session/runtime.rs`)

```
read(64 KiB) → PtyScanner::scan(&buf[..n], &mut out)
             → one write lock: apply changed signals + push_output(&filtered, meaningful_len) → cursor
             → OutputLog::append(&filtered)          // persistent file handle
             → broadcast_tx.send(filtered)           // Bytes, refcounted
             → for query in out.queries.drain(..): writer_tx.blocking_send(query.response(cursor))
```

### Supporting optimizations

| Area | File | Change |
|---|---|---|
| Read size | `session/runtime.rs` | 4 KiB → `PTY_READ_BUFFER_BYTES` = 64 KiB, heap-allocated once |
| Log append | `session/persist.rs` | New `OutputLog` persistent handle; was open/write/flush/close **per chunk** |
| Broadcast | `session/runtime.rs` | `Arc<Bytes>` → `Bytes` (already refcounted; the `Arc` was redundant) |
| Writer queue | `session/runtime.rs` | `PTY_WRITER_QUEUE_CAPACITY` 256 → 4096 |
| Mode publication | `session/runtime.rs` | New `SharedModes` (`AtomicU8`); attach loops do a relaxed atomic load instead of a `RwLock` read **per chunk**. `get_mode_snapshot()` removed. |
| Attach poll | `session/store/attach.rs` | `ATTACH_INPUT_OUTPUT_POLL_INTERVAL` 50 ms → 4 ms |
| Frame coalescing | `daemon/rpc_attach.rs`, `http/ws.rs` | Drain all queued chunks, emit one frame. Cap `MAX_COALESCED_CHUNK_BYTES` = 1 MiB |
| Client batching | `client/attach.rs` | Drain via `try_recv()`, concatenate, single `write_all` + `flush`. Cap `MAX_BATCHED_FRAME_BYTES` = 1 MiB |
| IPC framing | `ipc.rs` | New `encode_frame()`: one buffered write + flush instead of three separate awaits |

The 1 MiB caps exist because `MAX_IPC_LINE_BYTES` is 10 MB and attach payloads are
base64-encoded (~1.37× expansion). Do not raise them without re-checking that budget.

---

## 3) Results measured on Linux

Component benchmarks (temporary bench module, since removed):

| Benchmark | Before | After | Speedup |
|---|---|---|---|
| Scanner, TUI output @4 KiB | 49.8 MB/s | 1709 MB/s | **34×** |
| Scanner, TUI output @64 KiB | 56.4 MB/s | 1711 MB/s | **30×** |
| Scanner, plain paste echo @64 KiB | ~35 MB/s | 10,748 MB/s | **~300×** |

End-to-end, running two real daemons (the pre-change binary rebuilt from `git stash`)
against identical workloads, 3 runs averaged:

| Paste size | Before | After | Speedup |
|---|---|---|---|
| 400 KB | 0.458 s | **0.084 s** | **5.4×** |
| 2 MB | 1.162 s | **0.289 s** | **4.0×** |

Reproduce the end-to-end number with the harness in [§6](#6-reproducing-the-end-to-end-paste-benchmark).

---

## 4) Correctness guarantees already in place

- **`chunking_does_not_change_the_result`** (in `src/session/scan.rs`) is the core
  invariant test: it re-scans the same input at *every* chunk size from 1 byte up to
  the full length and asserts the filtered output is byte-identical every time. If you
  change the scanner, this test is your safety net — do not weaken it.
- **Golden fixtures** `output-copilot.expected` and `output-opencode.expected`
  (driven from `src/session/logs/`) pass byte-for-byte, which is the pixel-perfect
  rendering guarantee.
- Full suite on Linux: **398/398 unit, 19/19 `e2e_pty`, 9/9 `e2e_daemon`,
  1/1 `e2e_csvlens`, 17/17 `cli_errors`.**
- `cargo fmt --check`, `cargo check --all-targets` and `cargo clippy` are clean;
  zero clippy warnings originate from `scan.rs` or `pty.rs`.

---

## 5) What needs verifying on Windows

### 5.1 The one deliberate behaviour change — read this first

`src/session/scan.rs:45`:

```rust
const BARE_CONPTY_FORMS: bool = cfg!(windows);
```

**Background.** Windows ConPTY sometimes drops the introducing `ESC` byte of a
sequence, so a cursor report arrives as `[35;1R` and a title as `]0;title\x07`.
The old code stripped these "bare" forms **on every platform**.

**The change.** They are now recognised **only on Windows**. On Unix those exact bytes
are ordinary program output, and stripping them corrupted any program that
legitimately printed something like `[12;3R`.

**Why it matters for you.** This is the single highest-risk part of the rewrite and
it is the part that *cannot* be exercised on Linux. On Windows this constant flips to
`true`, which activates:

- `memchr3(0x1b, b'[', b']', ...)` instead of `memchr(0x1b, ...)` in the hot loop
  (`scan.rs:147`) — a different, less selective scan path;
- `bare_csi()` (`scan.rs:348`) and `bare_osc()` (`scan.rs:392`);
- the escape-reintroduction logic at `scan.rs:326`, which puts the `ESC` back on a
  bare title so downstream renderers see a well-formed sequence.

Seven unit tests are `#[cfg(windows)]`-gated and **have never executed**:

| Test | What it pins down |
|---|---|
| `bare_cursor_reports_are_stripped` | `[35;1R`, `[?35;1R`, `[6n`, `[5n` are removed |
| `bare_title_regains_its_escape_introducer` | `]0;title\x07` → `\x1b]0;title\x07` |
| `bare_title_is_not_double_escaped` | an already-escaped title is left alone (no `\x1b\x1b`) |
| `bare_colour_replies_are_stripped` | `]10;rgb:ffff/ffff/ffff\x07` is removed |
| `bare_backslash_is_not_a_terminator` | `C:\Users\me` in a title survives intact |
| `truncated_bare_candidate_is_not_held_back` | text ending in `[12` is flushed, not buffered |
| `bracketed_text_is_never_treated_as_a_sequence` | *(`cfg(not(windows))` — the Unix mirror image)* |

**First action on a Windows box: just run the test suite.** These seven fire
automatically.

### 5.2 Verification checklist

Work top to bottom; each step is cheap and the early ones catch the most.

- [ ] **1. Unit tests.** `cargo test --release` on `windows-latest` (or any Windows
      machine with the MSVC toolchain). This alone executes all seven gated tests plus
      `chunking_does_not_change_the_result` under `BARE_CONPTY_FORMS = true`.
      *Expected: 398 passed, 0 failed.*
- [ ] **2. E2E suites.** `cargo test --release --test e2e_pty --test e2e_daemon
      --test e2e_csvlens --test cli_errors`.
      *Expected: 19 / 9 / 1 / 17, all green.* These spawn real ConPTY children, so they
      exercise the bare-form path against genuine ConPTY output rather than fixtures.
- [ ] **3. Interactive smoke test — rendering.** `oly start --detach cmd.exe`, attach,
      and confirm the prompt renders correctly. Then run something that repaints the
      full screen. Watch specifically for: stray `[…R` fragments on screen, a corrupted
      or truncated window title, or doubled `ESC` bytes.
- [ ] **4. Interactive smoke test — the original symptom.** Attach and paste a large
      block (≥100 KB) of text. It should appear essentially instantly. This is the
      whole point of the change.
- [ ] **5. Title with backslashes.** In the attached session, `cd` into a deep path
      such as `C:\Users\<you>\AppData\Local` and confirm the title is not truncated at
      the first backslash and that no tail of it spills onto the screen. This is the
      regression `bare_backslash_is_not_a_terminator` guards, and it was a real bug
      once — only BEL and `ESC \` terminate an OSC; a lone backslash is payload.
- [ ] **6. A TUI app.** Run a full-screen TUI (`csvlens`, or any app using the
      alternate screen) and confirm the alternate screen enters and exits cleanly and
      that no escape fragments leak. Compare against the same app run outside `oly`.
- [ ] **7. Paste throughput number.** Optional but valuable: run the harness in §6
      under PowerShell/WSL and record the Windows figure so there is a baseline.
- [ ] **8. macOS.** Same as steps 1–2 and 3–4. `BARE_CONPTY_FORMS` is `false` there, so
      it takes the same code path as Linux; risk is low but it is genuinely untested.

### 5.3 If a Windows test fails

The failure almost certainly lives in `bare_csi` / `bare_osc` / the escape
reintroduction at `scan.rs:326`, not in the shared machinery — the shared path is
what Linux already exercises 398 tests' worth.

Debug in this order:

1. Add a failing input as a `#[cfg(windows)]` unit test in `scan.rs` using the existing
   `Harness` helper (`harness.filter_text(...)`). Keep it small and byte-exact.
2. Check whether `chunking_does_not_change_the_result` also fails — if it does, the bug
   is in how `pending` handles a split bare sequence, which is the subtlest part.
3. Only then reach for an interactive repro.

Do **not** "fix" a Windows failure by setting `BARE_CONPTY_FORMS` back to `true`
unconditionally. That reintroduces the Unix corruption bug the constant was created to
solve.

---

## 6) Reproducing the end-to-end paste benchmark

This is the harness used for the 400 KB / 2 MB numbers. It needs a scratch state dir
and a dedicated socket so it never touches a real daemon.

```bash
# 1. Scratch environment — never reuse the developer's real state dir
export OLY_STATE_DIR=/tmp/pastebench/oly
export OLY_SOCKET_NAME=open-relay.pastebench.sock
mkdir -p "$OLY_STATE_DIR"

# 2. Start an isolated daemon
./target/release/oly daemon start --foreground-internal --no-auth --no-http &
sleep 3

# 3. Paste into a `cat` session and time the round-trip through output.log
OLY=./target/release/oly
BYTES=400000
ID=$($OLY start --detach cat | grep -oE '[0-9a-f]{6,}' | head -1)
awk -v n=$((BYTES/64)) 'BEGIN{for(i=0;i<n;i++) printf "the quick brown fox jumps over the lazy dog 0123456789 %05d ", i}' > /tmp/paste.txt
SZ_IN=$(stat -c %s /tmp/paste.txt)
LOG="$OLY_STATE_DIR/sessions/$ID/output.log"

START=$(date +%s.%N)
N=$(( (SZ_IN/100000)+1 ))                       # chunked to dodge ARG_MAX
for k in $(seq 1 $N); do
  split -n $k/$N /tmp/paste.txt > /tmp/part.txt
  $OLY send "$ID" "$(cat /tmp/part.txt)" >/dev/null
done
until [ "$(stat -c %s "$LOG" 2>/dev/null || echo 0)" -ge $((SZ_IN-100)) ]; do sleep 0.02; done
END=$(date +%s.%N)
awk -v a=$END -v b=$START 'BEGIN{printf "elapsed=%.3fs\n", a-b}'

$OLY stop "$ID"
```

To get a *before* number, rebuild the pre-change binary and point the harness at it:

```bash
git stash && cargo build --release && cp target/release/oly /tmp/oly-baseline && git stash pop
```

**Gotchas that cost time the first go round:**

- `oly send` takes the payload as an argv element, so a single large paste hits
  `Argument list too long`. Split it — hence the `split -n k/N` loop.
- Wait for `output.log` to reach `SZ_IN`, not `2*SZ_IN`. `cat` echoes once, not twice.
- `bc` is not installed in every environment; the snippet uses `awk` for arithmetic.
- Kill *only* your own daemon PID afterwards. Check `ps -o args` first — a developer
  machine very likely has a real `oly daemon` running that must not be touched.

---

## 7) File map

| File | Role |
|---|---|
| **`src/session/scan.rs`** | **New.** `PtyScanner`, `ScanOut`, `classify_csi`, `is_passthrough_osc`, `bare_csi`, `bare_osc`, 51 tests. The thing to read first. |
| `src/session/pty.rs` | Rewritten. PTY ownership + terminal semantics only: `PtyHandle`, `RuntimeChild`, `TerminalQuery::response()`, `TerminalSignals`, `collect_chunk_bytes`. |
| `src/session/runtime.rs` | `SharedModes`, `PTY_READ_BUFFER_BYTES`, `PTY_WRITER_QUEUE_CAPACITY`, `push_output`, and the rewritten reader thread (~l. 570–665). |
| `src/session/persist.rs` | `OutputLog` persistent append handle. `append_output_raw` is now `#[cfg(test)]`. |
| `src/session/store/` | `shared_modes()`, `ATTACH_INPUT_OUTPUT_POLL_INTERVAL`. `get_mode_snapshot()` removed. |
| `src/daemon/rpc_attach.rs`, `src/http/ws.rs` | Attach relays: chunk coalescing + lock-free mode polling. |
| `src/client/attach.rs` | Client-side frame batching (~l. 440–505). |
| `src/ipc.rs` | `encode_frame()`. |
| `ARCHITECTURE_PTY.md` | §3 module layout, §5 reader/writer, §6 snapshot & replay, §7 mode tracking, §8 escape pipeline, §16 constraints — all rewritten. |
| `ARCHITECTURE.md`, `ARCHITECTURE_NOTES.md` | Updated; EC-1, EC-6, EC-7, EC-8 and the invariants list now describe `PtyScanner`. |

---

## 8) Design decisions worth knowing before you change anything

These were deliberate. Changing them without understanding why will reintroduce bugs.

- **The daemon answers only CPR, DSR, OSC 10 and OSC 11.** DA1, DA2, XTVERSION,
  DECRPM and kitty probes are *stripped but never answered* — the daemon is not a real
  terminal, and fabricating a capability reply corrupts the child's stdin.
- **OSC passthrough allowlist is exactly OSC 0/1/2 and OSC 9;4.** Everything else is
  stripped. OSC 9;4 (progress) bytes are excluded from `meaningful_bytes()` so a
  progress spinner does not read as screen activity; title bytes *do* count.
- **Only BEL and `ESC \` terminate an OSC.** A lone backslash is payload. See §5.2
  step 5.
- **`CSI <n> u` is not stripped** — that is the kitty keyboard stack pop, which is real
  output. Only the marked variants (`CSI ? u`, `CSI = n u`, …) are probes.
- **`regex` is still a dependency** — `src/notification/prompt.rs` uses it. It is simply
  no longer anywhere near the PTY hot path.
- **`SessionRuntime` is constructed literally in test helpers** in `runtime.rs` and
  `store/testsupport.rs`. Adding a field breaks all of them; find them with
  `grep -rn 'SessionRuntime {' src/`.
- **Release profile is `opt-level="z"`, `lto="fat"`, `codegen-units=1`** — expect 80–90 s
  builds. Budget for it; do not assume the build hung.

---

## 9) Known remaining risks

| Risk | Severity | Notes |
|---|---|---|
| `BARE_CONPTY_FORMS` path never executed | **High** | The entire reason for this document. §5. |
| macOS never exercised | Low | Same code path as Linux (`cfg!(windows)` is false). |
| 4 ms attach poll interval under sustained load | Low | All tests pass; a `#[cfg(test)]` 100 ms timeout applies in tests, so the production value is comparatively untested. Watch CPU on an idle attached session. |
| Client-side `AttachRenderer` on Windows still has its own escape handling | Low | Not touched by this work. Consider unifying it onto `PtyScanner` later — not required for correctness today. |
