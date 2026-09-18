//! PTY ownership plus the terminal query and notification types.
//!
//! The byte-level scanning that classifies pseudo-terminal output lives in
//! [`super::scan`]; this module owns the pseudo-terminal itself and the
//! semantic types the scanner produces.

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, trace, warn};

// ---------------------------------------------------------------------------
// PtyHandle — owns the pseudo-terminal file descriptors, reader and writer
// threads, and child process
// ---------------------------------------------------------------------------

/// Pure pseudo-terminal ownership struct. Manages the master file descriptor,
/// the child process, and
/// the dedicated reader/writer threads.  No business logic (notifications,
/// session metadata, etc.) lives here.
pub struct PtyHandle {
    pub(crate) child: RuntimeChild,
    /// Channel to the dedicated pseudo-terminal writer thread.
    pub(crate) writer_tx: mpsc::Sender<Vec<u8>>,
    /// Kept alive so the master file descriptor stays open; resize goes through
    /// this.  Wrapped in a `parking_lot::Mutex` so that `PtyHandle` is `Sync`,
    /// which lets the outer `SessionRuntime` live behind a `parking_lot::RwLock`.
    pub(crate) pty_master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
}

impl PtyHandle {
    /// Send raw bytes to the child process standard input via the writer thread.
    pub fn try_write_input(&self, data: Vec<u8>) -> std::result::Result<(), TrySendError<Vec<u8>>> {
        let len = data.len();
        let result = self.writer_tx.try_send(data);
        match &result {
            Ok(()) => trace!(bytes = len, "queued PTY stdin write"),
            Err(TrySendError::Full(_)) => debug!(bytes = len, "PTY stdin queue is full"),
            Err(TrySendError::Closed(_)) => debug!(bytes = len, "PTY stdin queue is closed"),
        }
        result
    }

    /// Resize the pseudo-terminal. Returns `true` on success.
    pub fn resize(&self, rows: u16, cols: u16) -> bool {
        let mut guard = self.pty_master.lock();
        let Some(master) = guard.as_mut() else {
            debug!(
                rows,
                cols, "PTY resize skipped because master handle is unavailable"
            );
            return false;
        };
        let result = master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok();
        debug!(rows, cols, resized = result, "PTY resize completed");
        result
    }

    /// Release pseudo-terminal-owned handles once the session is completed.
    ///
    /// This closes the stored master handle and replaces the public writer
    /// sender with a permanently closed channel so future writes fail fast.
    pub fn release_resources(&mut self) {
        debug!("releasing PTY resources");
        self.pty_master.lock().take();
        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let previous_tx = std::mem::replace(&mut self.writer_tx, closed_tx);
        drop(previous_tx);
    }

    /// Forcefully terminate the child process or close the Windows ConPTY handle.
    pub fn kill(&mut self) -> std::io::Result<()> {
        let result = self.child.kill();
        match &result {
            Ok(()) => debug!("PTY child kill requested"),
            Err(err) => warn!(%err, "failed to kill PTY child"),
        }
        result
    }

    /// M5-5: terminate the entire process tree rooted at the child.
    ///
    /// The child is a process-group leader (portable-pty `setsid`), so a
    /// group signal reaches every descendant; the direct-child kill follows
    /// as a fallback for children that escaped the group (and is the whole
    /// implementation on Windows, where ConPTY closes the tree with the
    /// console).
    pub fn kill_process_tree(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            let reached = signal_process_group(pid, SIGKILL);
            debug!(pid, reached, "SIGKILL sent to session process group");
        }
        self.kill()
    }

    /// Perform a non-blocking child-process exit check.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        let result = self.child.try_wait_code();
        match &result {
            Ok(Some(code)) => debug!(exit_code = code, "PTY child exit observed"),
            Ok(None) => {}
            Err(err) => warn!(%err, "failed to poll PTY child status"),
        }
        result
    }

    /// Return the child process identifier when the platform exposes one.
    #[allow(dead_code)]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

// ---------------------------------------------------------------------------
// RuntimeChild (wraps portable_pty::Child)
// ---------------------------------------------------------------------------

pub enum RuntimeChild {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
    #[cfg(test)]
    Mock {
        exit_code: Option<i32>,
    },
}

impl RuntimeChild {
    /// Return the wrapped child process identifier when available.
    pub fn process_id(&self) -> Option<u32> {
        match self {
            Self::Pty(child) => child.process_id(),
            #[cfg(test)]
            Self::Mock { .. } => None,
        }
    }

    /// Terminate the wrapped child process.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Pty(child) => child.kill(),
            #[cfg(test)]
            Self::Mock { exit_code } => {
                if exit_code.is_none() {
                    *exit_code = Some(1);
                }
                Ok(())
            }
        }
    }

    /// Perform a non-blocking wait and normalize the exit status to an `i32`.
    pub fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Pty(child) => child
                .try_wait()
                .map(|opt| opt.map(|status| status.exit_code() as i32)),
            #[cfg(test)]
            Self::Mock { exit_code } => Ok(*exit_code),
        }
    }
}

// ---------------------------------------------------------------------------
// Process-group signalling (M5-5: process-tree kill)
// ---------------------------------------------------------------------------

/// Signal the process group led by `pgid`.
///
/// portable-pty spawns children under `setsid`, so the session child's pid is
/// also its process-group id and every descendant the child forks shares it.
/// Signalling `-pgid` therefore reaches the whole tree (grandchildren a bare
/// `Child::kill` would orphan).
///
/// Zero-dependency: `kill(2)` is bound directly. Returns `true` when the
/// signal was delivered to at least one process in the group. An `ESRCH`
/// (group fully reaped) is a quiet `false`; other errors log and return
/// `false`.
#[cfg(unix)]
pub fn signal_process_group(pgid: u32, signal: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    // Negative pid targets the process group (POSIX killpg semantics).
    let rc = unsafe { kill(-(pgid as i32), signal) };
    if rc == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(3) {
        // ESRCH: no such process group — the tree is already gone.
        return false;
    }
    warn!(pgid, signal, %err, "process-group signal failed");
    false
}

/// POSIX signal numbers used for tree escalation. Defined locally so the
/// escalation policy reads without a libc dependency.
#[cfg(unix)]
pub const SIGTERM: i32 = 15;
#[cfg(unix)]
pub const SIGKILL: i32 = 9;

// ---------------------------------------------------------------------------
// Terminal capability queries
// ---------------------------------------------------------------------------

/// A terminal-capability probe the child emitted that the daemon answers on
/// the session's behalf.
///
/// Only probes with a *session-global* answer appear here. Capability probes
/// whose answer describes the user's own terminal (device attributes,
/// XTVERSION, DEC private mode reports, kitty keyboard flags) are filtered out
/// of the output stream but deliberately left unanswered: the daemon does not
/// know what terminal — if any — is attached, and injecting a guess into the
/// child's standard input corrupts the input stream of anything that was not
/// waiting for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalQuery {
    /// Cursor Position Report probe (`CSI 6 n`).
    CursorPositionReport,
    /// Device Status Report probe (`CSI 5 n`).
    DeviceStatusReport,
    /// Foreground colour probe (`OSC 10 ; ?`).
    ForegroundColor,
    /// Background colour probe (`OSC 11 ; ?`).
    BackgroundColor,
}

/// Derive the foreground/background RGB pair the session engine reports for
/// `OSC 10 ; ?` / `OSC 11 ; ?` probes: the COLORFGBG environment when set,
/// falling back to a conservative white-on-black pair.
pub(crate) fn terminal_report_rgb() -> ((u8, u8, u8), (u8, u8, u8)) {
    if let Ok(raw) = std::env::var("COLORFGBG") {
        let parsed: Vec<u8> = raw
            .split(';')
            .filter_map(|part| part.trim().parse::<u8>().ok())
            .collect();
        if parsed.len() >= 2 {
            let foreground = xterm_color_to_rgb(parsed[parsed.len() - 2]);
            let background = xterm_color_to_rgb(parsed[parsed.len() - 1]);
            return (foreground, background);
        }
    }

    ((0xff, 0xff, 0xff), (0x00, 0x00, 0x00))
}

/// Convert an xterm 256-color palette index into an RGB triple.
fn xterm_color_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0 => (0x00, 0x00, 0x00),
        1 => (0xcd, 0x00, 0x00),
        2 => (0x00, 0xcd, 0x00),
        3 => (0xcd, 0xcd, 0x00),
        4 => (0x00, 0x00, 0xee),
        5 => (0xcd, 0x00, 0xcd),
        6 => (0x00, 0xcd, 0xcd),
        7 => (0xe5, 0xe5, 0xe5),
        8 => (0x7f, 0x7f, 0x7f),
        9 => (0xff, 0x00, 0x00),
        10 => (0x00, 0xff, 0x00),
        11 => (0xff, 0xff, 0x00),
        12 => (0x5c, 0x5c, 0xff),
        13 => (0xff, 0x00, 0xff),
        14 => (0x00, 0xff, 0xff),
        15 => (0xff, 0xff, 0xff),
        16..=231 => {
            let value = index - 16;
            let red = value / 36;
            let green = (value % 36) / 6;
            let blue = value % 6;
            let levels = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            (
                levels[red as usize],
                levels[green as usize],
                levels[blue as usize],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

// ---------------------------------------------------------------------------
// TerminalSignals — one-way notifications the screen parser does not model
// ---------------------------------------------------------------------------

/// Longest notification payload retained per slot in [`TerminalSignals`].
///
/// Window titles are short in practice; the cap keeps a misbehaving child from
/// pinning an arbitrarily large payload in the session runtime for the rest of
/// its life.
const MAX_RETAINED_SIGNAL_PAYLOAD_BYTES: usize = 1024;

/// The one-way terminal notifications a session has most recently emitted.
///
/// The rendered screen state used for attach restore models only the character
/// grid, cursor and input modes — Operating System Commands are dropped by the
/// terminal parser entirely. Without this, a client that attaches shows the
/// correct screen while the window title and progress/busy indicator silently
/// keep whatever values the user's own shell left behind. Retaining the last
/// value of each slot lets the daemon replay them alongside the screen
/// snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TerminalSignals {
    /// Payload of the most recent icon-title notification (OSC 0 or OSC 1).
    icon_title: Option<Vec<u8>>,
    /// Payload of the most recent window-title notification (OSC 0 or OSC 2).
    window_title: Option<Vec<u8>>,
    /// Payload of the most recent progress notification (OSC 9;4), if the
    /// indicator is currently showing.
    progress: Option<Vec<u8>>,
    /// Parameters of the most recent cursor-style sequence (DECSCUSR), if the
    /// session selected a non-default shape.
    cursor_style: Option<Vec<u8>>,
    /// Payload of the most recent default-foreground-colour set (OSC 10).
    ///
    /// Unlike titles and progress these are *not* replayed to attached
    /// clients: a session's colour scheme is host-specific state that must
    /// not follow it onto another user's terminal. They are retained purely
    /// as session metadata.
    foreground: Option<Vec<u8>>,
    /// Payload of the most recent default-background-colour set (OSC 11).
    background: Option<Vec<u8>>,
}

impl TerminalSignals {
    /// Record one passthrough Operating System Command.
    ///
    /// Returns `true` when the retained state actually changed, so the caller
    /// can skip republishing an identical snapshot.
    pub(crate) fn record_osc(&mut self, ps: &[u8], payload: &[u8]) -> bool {
        if payload.len() > MAX_RETAINED_SIGNAL_PAYLOAD_BYTES {
            return false;
        }

        match ps {
            b"0" => {
                let changed = self.icon_title.as_deref() != Some(payload)
                    || self.window_title.as_deref() != Some(payload);
                if changed {
                    self.icon_title = Some(payload.to_vec());
                    self.window_title = Some(payload.to_vec());
                }
                changed
            }
            b"1" => replace_slot(&mut self.icon_title, Some(payload)),
            b"2" => replace_slot(&mut self.window_title, Some(payload)),
            // An empty payload resets the colour to the terminal default, so
            // drop the slot rather than retaining an empty spec.
            b"10" => replace_slot(
                &mut self.foreground,
                (!payload.is_empty()).then_some(payload),
            ),
            b"11" => replace_slot(
                &mut self.background,
                (!payload.is_empty()).then_some(payload),
            ),
            b"9" if payload.starts_with(b"4;") => {
                // OSC 9;4;0 removes the indicator, so drop the slot rather than
                // replaying a clear on every future attach.
                let state = payload[2..]
                    .split(|&byte| byte == b';')
                    .next()
                    .unwrap_or_default();
                let next = if state == b"0" { None } else { Some(payload) };
                replace_slot(&mut self.progress, next)
            }
            _ => false,
        }
    }

    /// The most recently emitted default-foreground-colour payload (OSC 10),
    /// if the session currently has one set.
    pub fn foreground_color(&self) -> Option<&[u8]> {
        self.foreground.as_deref()
    }

    /// The most recently emitted default-background-colour payload (OSC 11),
    /// if the session currently has one set.
    pub fn background_color(&self) -> Option<&[u8]> {
        self.background.as_deref()
    }

    /// The most recently emitted window/icon title payload, if any.
    ///
    /// The window title is preferred; OSC 1 (icon-only) is the fallback for
    /// children that never set a window title. OSC 0 sets both slots, so it is
    /// covered by either branch.
    pub fn title(&self) -> Option<&[u8]> {
        self.window_title.as_deref().or(self.icon_title.as_deref())
    }

    /// Record the parameters of a cursor-style sequence, or `None` to restore
    /// the terminal default. Returns `true` when the retained state changed.
    pub(crate) fn set_cursor_style(&mut self, params: Option<Vec<u8>>) -> bool {
        if self.cursor_style == params {
            return false;
        }
        self.cursor_style = params;
        true
    }

    /// Escape sequences that reproduce the retained notifications on a freshly
    /// attached terminal. Empty when the session never emitted any.
    pub fn restore_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        match (self.icon_title.as_deref(), self.window_title.as_deref()) {
            (Some(icon), Some(window)) if icon == window => push_osc(&mut bytes, b"0", icon),
            (icon, window) => {
                if let Some(icon) = icon {
                    push_osc(&mut bytes, b"1", icon);
                }
                if let Some(window) = window {
                    push_osc(&mut bytes, b"2", window);
                }
            }
        }

        if let Some(progress) = self.progress.as_deref() {
            push_osc(&mut bytes, b"9", progress);
        }

        if let Some(cursor_style) = self.cursor_style.as_deref() {
            bytes.extend_from_slice(b"\x1b[");
            bytes.extend_from_slice(cursor_style);
            bytes.extend_from_slice(b" q");
        }

        bytes
    }
}

fn replace_slot(slot: &mut Option<Vec<u8>>, next: Option<&[u8]>) -> bool {
    if slot.as_deref() == next {
        return false;
    }
    *slot = next.map(<[u8]>::to_vec);
    true
}

fn push_osc(out: &mut Vec<u8>, ps: &[u8], payload: &[u8]) {
    out.extend_from_slice(b"\x1b]");
    out.extend_from_slice(ps);
    out.push(b';');
    out.extend_from_slice(payload);
    out.push(0x07);
}

/// Concatenate replay chunks into a single byte buffer.
///
/// Replay chunks already reflect the canonical filtered stream retained by the
/// runtime, so stream consumers do not need an additional filter pass here.
pub fn collect_chunk_bytes(chunks: &[(u64, bytes::Bytes)]) -> Vec<u8> {
    let mut filtered = Vec::with_capacity(chunks.iter().map(|(_, chunk)| chunk.len()).sum());
    for (_, chunk) in chunks {
        filtered.extend_from_slice(chunk);
    }
    filtered
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn signalling_a_nonexistent_process_group_is_quietly_false() {
        // PGID 0x3FFFFFFF is not a live process group in any test harness.
        assert!(!super::signal_process_group(0x3FFF_FFFF, super::SIGTERM));
        assert!(!super::signal_process_group(0x3FFF_FFFF, 0));
    }

    use super::*;

    // The reply formats themselves (CPR/DSR/OSC colours) are generated by the
    // session engine since M2 and are pinned in `terminal::tests`.

    #[test]
    fn report_rgb_falls_back_to_white_on_black() {
        // COLORFGBG is not set in the test environment (and the fallback must
        // not depend on it), so the conservative pair is expected.
        let (fg, bg) = terminal_report_rgb();
        if std::env::var_os("COLORFGBG").is_none() {
            assert_eq!(fg, (0xff, 0xff, 0xff));
            assert_eq!(bg, (0x00, 0x00, 0x00));
        }
    }

    #[test]
    fn xterm_palette_maps_cube_and_grayscale_entries() {
        assert_eq!(xterm_color_to_rgb(16), (0x00, 0x00, 0x00));
        assert_eq!(xterm_color_to_rgb(21), (0x00, 0x00, 0xff));
        assert_eq!(xterm_color_to_rgb(232), (0x08, 0x08, 0x08));
        assert_eq!(xterm_color_to_rgb(255), (0xee, 0xee, 0xee));
    }

    #[test]
    fn title_prefers_the_window_title_and_falls_back_to_the_icon_title() {
        let mut signals = TerminalSignals::default();
        assert_eq!(signals.title(), None);

        signals.record_osc(b"1", b"icon");
        assert_eq!(signals.title(), Some(b"icon".as_slice()));

        signals.record_osc(b"2", b"window");
        assert_eq!(signals.title(), Some(b"window".as_slice()));

        // OSC 0 sets both slots to the same payload.
        signals.record_osc(b"0", b"both");
        assert_eq!(signals.title(), Some(b"both".as_slice()));
    }

    #[test]
    fn colour_slots_track_sets_and_resets() {
        let mut signals = TerminalSignals::default();
        assert_eq!(signals.foreground_color(), None);
        assert_eq!(signals.background_color(), None);

        assert!(signals.record_osc(b"10", b"rgb:ffff/ffff/ffff"));
        assert!(signals.record_osc(b"11", b"#1e1e1e"));
        assert_eq!(
            signals.foreground_color(),
            Some(b"rgb:ffff/ffff/ffff".as_slice())
        );
        assert_eq!(signals.background_color(), Some(b"#1e1e1e".as_slice()));

        // Re-setting the same value is not a change.
        assert!(!signals.record_osc(b"10", b"rgb:ffff/ffff/ffff"));

        // An empty payload resets to the terminal default.
        assert!(signals.record_osc(b"10", b""));
        assert_eq!(signals.foreground_color(), None);

        // Colours are session metadata only: they are never replayed onto an
        // attaching terminal.
        assert!(signals.restore_bytes().is_empty());
    }

    #[test]
    fn signals_restore_title_and_progress() {
        let mut signals = TerminalSignals::default();
        assert!(signals.record_osc(b"0", b"relay build"));
        assert!(signals.record_osc(b"9", b"4;3;0"));

        assert_eq!(
            signals.restore_bytes(),
            b"\x1b]0;relay build\x07\x1b]9;4;3;0\x07".to_vec()
        );
    }

    #[test]
    fn signals_keep_the_latest_title_and_a_distinct_icon_title() {
        let mut signals = TerminalSignals::default();
        signals.record_osc(b"0", b"first");
        signals.record_osc(b"2", b"window");

        assert_eq!(
            signals.restore_bytes(),
            b"\x1b]1;first\x07\x1b]2;window\x07".to_vec()
        );
    }

    #[test]
    fn signals_clear_progress_when_the_indicator_is_removed() {
        let mut signals = TerminalSignals::default();
        signals.record_osc(b"9", b"4;3;0");
        assert!(signals.record_osc(b"9", b"4;0;0"));

        assert!(signals.restore_bytes().is_empty());
    }

    #[test]
    fn signals_report_no_change_for_a_repeated_value() {
        let mut signals = TerminalSignals::default();
        assert!(signals.record_osc(b"2", b"same"));
        assert!(!signals.record_osc(b"2", b"same"));
    }

    #[test]
    fn signals_restore_cursor_style() {
        let mut signals = TerminalSignals::default();
        assert!(signals.set_cursor_style(Some(b"6".to_vec())));
        assert_eq!(signals.restore_bytes(), b"\x1b[6 q".to_vec());

        assert!(signals.set_cursor_style(None));
        assert!(signals.restore_bytes().is_empty());
    }

    #[test]
    fn signals_ignore_oversized_payloads() {
        let mut signals = TerminalSignals::default();
        let payload = vec![b'x'; MAX_RETAINED_SIGNAL_PAYLOAD_BYTES + 1];

        assert!(!signals.record_osc(b"2", &payload));
        assert!(signals.restore_bytes().is_empty());
    }
}
