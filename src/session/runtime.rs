use std::{
    io::{ErrorKind, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, trace};
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    utils::extract_query_responses_no_client,
};

use super::{
    SessionMeta, SessionStatus,
    persist::{append_event, append_output_raw, append_resize_event},
    ring::RingBuffer,
};

// ---------------------------------------------------------------------------
// RuntimeChild
// ---------------------------------------------------------------------------

pub enum RuntimeChild {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

impl RuntimeChild {
    pub fn process_id(&self) -> Option<u32> {
        match self {
            Self::Pty(child) => child.process_id(),
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Pty(child) => child.kill(),
        }
    }

    pub fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Pty(child) => child
                .try_wait()
                .map(|opt| opt.map(|status| status.exit_code() as i32)),
        }
    }
}

#[allow(dead_code)]
const ATTACH_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// SessionRuntime
// ---------------------------------------------------------------------------

pub struct SessionRuntime {
    pub meta: SessionMeta,
    /// Absolute path to the session's working directory (`sessions/<id>/`).
    pub dir: PathBuf,
    /// Byte-limited ring buffer of raw PTY output.
    pub ring: RingBuffer,
    /// Sends raw PTY output chunks to all live attach subscribers.
    /// `receiver_count()` reflects how many clients are currently attached and
    /// governs whether the daemon-side terminal-query fallback responder fires.
    pub broadcast_tx: broadcast::Sender<Arc<Bytes>>,
    /// Channel to the dedicated PTY writer thread.  Send `Vec<u8>` to write
    /// bytes to the child's stdin without blocking the caller.
    pub writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub child: RuntimeChild,
    pub completed_at: Option<Instant>,
    pub _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    /// Set to `true` once the completed state has been written to the database.
    pub persisted: bool,
    /// Timestamp of the last visible output chunk; drives the notification engine.
    pub last_output_at: Option<Instant>,
    /// Timestamp of the last input bytes forwarded to the PTY; used to suppress
    /// notifications while the user is actively typing.
    pub last_input_at: Option<Instant>,
    /// Timestamp of the last subscribe/attach action; coarse presence signal.
    pub last_attach_presence_at: Option<Instant>,
    /// Timestamp of the last interactive attach action (input/resize) that
    /// should suppress notifications for a short window.
    pub last_attach_activity_at: Option<Instant>,
    /// Timestamp of the last *successful* notification delivery for this session.
    pub last_notified_at: Option<Instant>,
    /// The value of `last_output_at` at the time the last notification was sent.
    /// Re-notification is suppressed until output advances past this epoch.
    pub notified_output_epoch: Option<Instant>,
    /// Whether the child has enabled bracketed-paste mode (`\x1b[?2004h`).
    /// Updated by `push_raw` scanning the raw PTY output stream.
    pub bracketed_paste_mode: bool,
    /// Whether the child has enabled DECCKM application cursor key mode
    /// (`\x1b[?1h`).  When active, arrow keys must be `\x1bO{A,B,C,D}`.
    pub app_cursor_keys: bool,
    /// Set once the PTY reader has reached EOF or a terminal read error.
    /// After this no further PTY output can arrive for streaming attaches.
    pub output_closed: bool,
    pub notifications_enabled: bool,
}

impl SessionRuntime {
    /// Push a raw PTY chunk: persist to disk, store in ring, update mode state,
    /// and advance the silence clock for visible content.
    ///
    /// No ESC filtering is done here — raw bytes are stored as-is so that
    /// output.log is a faithful record.  Filtering happens per-subscriber at
    /// delivery time via `EscapeFilter`.
    pub fn push_raw(&mut self, data: Bytes) {
        let text_cow = String::from_utf8_lossy(&data);

        // Track DEC private mode toggles in the raw stream.
        if text_cow.contains("\x1b[?2004h") {
            if !self.bracketed_paste_mode {
                debug!(session_id = %self.meta.id, "child enabled bracketed paste mode");
            }
            self.bracketed_paste_mode = true;
        } else if text_cow.contains("\x1b[?2004l") {
            if self.bracketed_paste_mode {
                debug!(session_id = %self.meta.id, "child disabled bracketed paste mode");
            }
            self.bracketed_paste_mode = false;
        }
        if text_cow.contains("\x1b[?1h") {
            if !self.app_cursor_keys {
                debug!(session_id = %self.meta.id, "child enabled application cursor keys");
            }
            self.app_cursor_keys = true;
        } else if text_cow.contains("\x1b[?1l") {
            if self.app_cursor_keys {
                debug!(session_id = %self.meta.id, "child disabled application cursor keys");
            }
            self.app_cursor_keys = false;
        }

        // Advance the silence clock only for chunks with visible content.
        // Pure ANSI cursor/redraw sequences must NOT reset the timer.
        if has_visible_content(&text_cow) {
            self.last_output_at = Some(Instant::now());
        }

        // Persist raw bytes to disk.
        let _ = append_output_raw(&self.dir, &data);

        // Add to in-memory ring (evicts oldest if over capacity).
        self.ring.push(data);
    }

    pub fn mark_attach_presence(&mut self) {
        trace!(session_id = %self.meta.id, "attach presence marked");
        self.last_attach_presence_at = Some(Instant::now());
    }

    pub fn mark_attach_activity(&mut self) {
        self.mark_attach_presence();
        debug!(session_id = %self.meta.id, "interactive attach activity marked");
        self.last_attach_activity_at = Some(Instant::now());
    }

    pub fn clear_attach_state(&mut self) {
        debug!(session_id = %self.meta.id, "attach presence/activity cleared");
        self.last_attach_presence_at = None;
        self.last_attach_activity_at = None;
    }

    /// Returns `true` when at least one attach subscriber is currently live.
    #[allow(dead_code)]
    pub fn has_active_attach_client(&self) -> bool {
        self.broadcast_tx.receiver_count() > 0
    }

    /// Checks child exit status and updates `meta.status`. Returns `true` if completed.
    pub fn refresh_status(&mut self) -> bool {
        if self.is_completed() {
            if self.completed_at.is_none() {
                self.completed_at = Some(Instant::now());
            }
            return true;
        }

        match self.child.try_wait_code() {
            Ok(Some(code)) => {
                debug!(session_id = %self.meta.id, exit_code = code, "child process exited");
                let status = if code == 0 {
                    SessionStatus::Stopped
                } else {
                    SessionStatus::Failed
                };
                self.mark_completed(status, Some(code));
                true
            }
            Ok(None) => {
                if !matches!(self.meta.status, SessionStatus::Stopping) {
                    self.meta.status = SessionStatus::Running;
                }
                false
            }
            Err(_) => {
                debug!(session_id = %self.meta.id, "failed to read child exit status; marking session failed");
                self.mark_completed(SessionStatus::Failed, None);
                true
            }
        }
    }

    pub fn mark_completed(&mut self, status: SessionStatus, exit_code: Option<i32>) {
        if self.meta.ended_at.is_none() {
            self.meta.ended_at = Some(chrono::Utc::now());
        }
        self.meta.status = status;
        if let Some(code) = exit_code {
            self.meta.exit_code = Some(code);
        }
        if self.completed_at.is_none() {
            self.completed_at = Some(Instant::now());
        }
        let event = match &self.meta.status {
            SessionStatus::Stopped => format!(
                "session stopped exit_code={}",
                self.meta.exit_code.unwrap_or(0)
            ),
            SessionStatus::Failed => format!(
                "session failed exit_code={}",
                self.meta.exit_code.unwrap_or(-1)
            ),
            other => format!("session ended status={}", other.as_str()),
        };
        let _ = append_event(&self.dir, &event);
    }

    pub fn is_completed(&self) -> bool {
        matches!(
            self.meta.status,
            SessionStatus::Stopped | SessionStatus::Failed
        )
    }

    pub fn resize_pty(&mut self, rows: u16, cols: u16) -> bool {
        if rows == 0 || cols == 0 {
            debug!(session_id = %self.meta.id, rows, cols, "ignoring invalid PTY resize request");
            return false;
        }
        let Some(pty_master) = self._pty_master.as_mut() else {
            debug!(session_id = %self.meta.id, rows, cols, "PTY resize requested after master was released");
            return false;
        };
        let resized = pty_master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok();
        debug!(session_id = %self.meta.id, rows, cols, resized, "PTY resize attempted");
        resized
    }
}

// ---------------------------------------------------------------------------
// Session ID generation
// ---------------------------------------------------------------------------

pub fn generate_session_id<F: Fn(&str) -> bool>(exists: F) -> String {
    loop {
        let raw = Uuid::new_v4().as_simple().to_string();
        let candidate = raw.chars().take(7).collect::<String>();
        if !exists(&candidate) {
            return candidate;
        }
    }
}

// ---------------------------------------------------------------------------
// PTY spawning
// ---------------------------------------------------------------------------

/// Spawns a PTY-backed child process and returns an `Arc<Mutex<SessionRuntime>>`.
/// Reader and writer threads are started automatically and share ownership via the Arc.
/// `session_dir` is the absolute path for the session's working files; the caller
/// is responsible for computing it (typically `sessions_dir.join(&meta.id)`).
pub fn spawn_session(
    config: &AppConfig,
    meta: &mut SessionMeta,
    session_dir: PathBuf,
    rows: u16,
    cols: u16,
    notifications_enabled: bool,
) -> Result<Arc<Mutex<SessionRuntime>>> {
    let full_dir = session_dir;
    std::fs::create_dir_all(&full_dir)?;

    let Ok(cmd) = which::which(&meta.command) else {
        return Err(AppError::Protocol(format!(
            "command not found: {}",
            meta.command
        )));
    };

    let mut cmd = CommandBuilder::new(cmd);
    cmd.args(&meta.args);
    let cwd_fallback = full_dir.to_string_lossy().into_owned();
    cmd.cwd(meta.cwd.as_ref().unwrap_or(&cwd_fallback));

    let cmd_display = format_command_for_display(&meta.command, &meta.args);
    let pty = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| {
            AppError::Protocol(format!("failed to allocate PTY for `{cmd_display}`: {err}"))
        })?;

    let child = pty.slave.spawn_command(cmd).map_err(|err| {
        AppError::Protocol(format!(
            "failed to spawn `{cmd_display}` (cwd={}): {err}",
            meta.cwd.as_deref().unwrap_or("<current>")
        ))
    })?;

    let master = pty.master;
    let reader = master.try_clone_reader().map_err(|err| {
        AppError::Protocol(format!(
            "failed to create PTY reader for `{cmd_display}`: {err}"
        ))
    })?;
    let writer = master.take_writer().map_err(|err| {
        AppError::Protocol(format!(
            "failed to create PTY writer for `{cmd_display}`: {err}"
        ))
    })?;
    let runtime_child = RuntimeChild::Pty(child);
    meta.pid = runtime_child.process_id();

    std::fs::write(full_dir.join("output.log"), b"")?;
    std::fs::write(full_dir.join("events.log"), b"session created\n")?;
    let _ = append_resize_event(&full_dir, 0, rows, cols);
    let started_pid = meta
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".to_string());
    let _ = append_event(&full_dir, &format!("session started pid={started_pid}"));

    // Broadcast channel: each live attach subscriber holds a Receiver.
    // `receiver_count()` is used in the reader thread to decide whether the
    // daemon-side terminal-query fallback should fire.
    let (broadcast_tx, _initial_rx) = broadcast::channel::<Arc<Bytes>>(256);

    // Writer channel: the dedicated write thread owns the PTY writer so that
    // sending input never blocks the tokio runtime.
    let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // PTY writer thread — drains writer_rx and forwards bytes to the child.
    std::thread::spawn(move || {
        let mut writer = writer;
        while let Some(data) = writer_rx.blocking_recv() {
            let _ = writer.write_all(&data).and_then(|_| writer.flush());
        }
    });

    let runtime = Arc::new(Mutex::new(SessionRuntime {
        meta: meta.clone(),
        dir: full_dir,
        ring: RingBuffer::new(config.ring_buffer_bytes),
        broadcast_tx: broadcast_tx.clone(),
        writer_tx: writer_tx.clone(),
        child: runtime_child,
        completed_at: None,
        _pty_master: Some(master),
        persisted: false,
        last_output_at: None,
        last_input_at: None,
        last_attach_presence_at: None,
        last_attach_activity_at: None,
        notified_output_epoch: None,
        last_notified_at: None,
        bracketed_paste_mode: false,
        app_cursor_keys: false,
        output_closed: false,
        notifications_enabled,
    }));

    // PTY reader thread: reads raw bytes, stores in ring, broadcasts to subscribers.
    let runtime_reader = runtime.clone();
    let broadcast_tx_reader = broadcast_tx;
    std::thread::spawn(move || {
        if let Ok(rt) = runtime_reader.lock() {
            let _ = append_event(&rt.dir, "pty reader started");
        }
        let mut buf = [0u8; 4096];
        let mut reader = reader;
        let mut query_tail = String::new();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    if let Ok(mut rt) = runtime_reader.lock() {
                        rt.output_closed = true;
                        let _ = append_event(&rt.dir, "pty reader reached EOF");
                    }
                    break;
                }
                Ok(n) => {
                    let data = Bytes::copy_from_slice(&buf[..n]);

                    // Always let the daemon answer terminal queries that need
                    // a shared, session-global answer (currently CPR/DSR).
                    // Multiple attached clients cannot safely
                    // answer independently because each viewer may have a
                    // different local cursor position; the child PTY only has
                    // one shared terminal state.  Centralising replies here
                    // prevents passive viewers from sending conflicting CPRs.
                    for resp in extract_query_responses_no_client(&data, &mut query_tail) {
                        let _ = writer_tx.send(resp);
                    }

                    // Update in-memory ring + disk (brief lock, no I/O inside).
                    match runtime_reader.lock() {
                        Ok(mut rt) => rt.push_raw(data.clone()),
                        Err(_) => break,
                    }

                    // Broadcast to all live subscribers (non-blocking; lagged
                    // receivers will re-sync from the ring on the next tick).
                    let _ = broadcast_tx_reader.send(Arc::new(data));
                }
                Err(err)
                    if matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    if let Ok(mut rt) = runtime_reader.lock() {
                        rt.output_closed = true;
                        let _ = append_event(&rt.dir, &format!("pty reader error: {err}"));
                    }
                    break;
                }
            }
        }
    });

    Ok(runtime)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn format_command_for_display(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return command.to_string();
    }
    format!("{} {}", command, args.join(" "))
}

/// Returns `true` when `text` contains at least one character that is
/// visually rendered (i.e. not whitespace and not part of an ANSI/VT escape
/// sequence). Used to avoid advancing the silence clock on chunks that
/// contain only cursor-movement or redraw escape sequences.
pub(crate) fn has_visible_content(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip CSI/OSC/other escape sequences.
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if c == '\x07' {
                            break;
                        }
                        if prev == '\x1b' && c == '\\' {
                            break;
                        }
                        prev = c;
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else if !ch.is_whitespace() && !ch.is_control() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_generate_session_id_is_7_chars() {
        let id = generate_session_id(|_| false);
        assert_eq!(id.len(), 7, "session id must be exactly 7 characters");
    }

    #[test]
    fn test_generate_session_id_is_alphanumeric() {
        let id = generate_session_id(|_| false);
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric()),
            "session id must be alphanumeric, got: {id}"
        );
    }

    #[test]
    fn test_generate_session_id_avoids_collision() {
        // Force first two attempts to collide, accept the third.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let call_count = AtomicUsize::new(0);
        let id = generate_session_id(|_| {
            let n = call_count.fetch_add(1, Ordering::Relaxed);
            n < 2
        });
        assert_eq!(id.len(), 7);
        assert!(call_count.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn test_generate_session_id_unique_across_many() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let id = generate_session_id(|c| seen.contains(c));
            assert!(seen.insert(id.clone()), "duplicate id: {id}");
        }
    }

    // -----------------------------------------------------------------------
    // Helpers for SessionRuntime unit tests
    // -----------------------------------------------------------------------

    fn make_test_child() -> RuntimeChild {
        #[cfg(target_os = "windows")]
        let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
        #[cfg(target_os = "windows")]
        cmd.args(["/c", "exit", "0"]);
        #[cfg(not(target_os = "windows"))]
        let mut cmd = portable_pty::CommandBuilder::new("true");

        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty in test");
        let child = pty.slave.spawn_command(cmd).expect("spawn in test");
        RuntimeChild::Pty(child)
    }

    fn new_runtime() -> SessionRuntime {
        use crate::session::{SessionMeta, SessionStatus};

        let meta = SessionMeta {
            id: "rt_tst01".to_string(),
            title: None,
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            ended_at: None,
            status: SessionStatus::Running,
            pid: None,
            exit_code: None,
        };
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel(4);
        let (writer_tx, _wrx) = tokio::sync::mpsc::unbounded_channel();
        SessionRuntime {
            meta,
            dir: std::env::temp_dir().join("oly_runtime_unit_tests"),
            ring: RingBuffer::new(4096), // small capacity for tests
            broadcast_tx,
            writer_tx,
            child: make_test_child(),
            completed_at: None,
            _pty_master: None,
            persisted: false,
            last_output_at: None,
            last_input_at: None,
            last_attach_presence_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            bracketed_paste_mode: false,
            app_cursor_keys: false,
            output_closed: false,
            notifications_enabled: true,
        }
    }

    // -----------------------------------------------------------------------
    // push_raw — mode tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_raw_enables_bracketed_paste() {
        let mut rt = new_runtime();
        assert!(!rt.bracketed_paste_mode);
        rt.push_raw(bytes::Bytes::from("text \x1b[?2004h more"));
        assert!(
            rt.bracketed_paste_mode,
            "bracketed_paste_mode should be set after \\x1b[?2004h"
        );
    }

    #[test]
    fn test_push_raw_disables_bracketed_paste() {
        let mut rt = new_runtime();
        rt.bracketed_paste_mode = true;
        rt.push_raw(bytes::Bytes::from("\x1b[?2004l"));
        assert!(
            !rt.bracketed_paste_mode,
            "bracketed_paste_mode should be cleared after \\x1b[?2004l"
        );
    }

    #[test]
    fn test_push_raw_enables_app_cursor_keys() {
        let mut rt = new_runtime();
        assert!(!rt.app_cursor_keys);
        rt.push_raw(bytes::Bytes::from("\x1b[?1h"));
        assert!(
            rt.app_cursor_keys,
            "app_cursor_keys should be set after DECCKM enable"
        );
    }

    #[test]
    fn test_push_raw_disables_app_cursor_keys() {
        let mut rt = new_runtime();
        rt.app_cursor_keys = true;
        rt.push_raw(bytes::Bytes::from("\x1b[?1l"));
        assert!(
            !rt.app_cursor_keys,
            "app_cursor_keys should be cleared after DECCKM disable"
        );
    }

    // -----------------------------------------------------------------------
    // push_raw — ring buffer eviction
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_raw_ring_evicts_oldest_when_over_capacity() {
        let mut rt = new_runtime(); // capacity = 4096 bytes
        // Push chunks that together exceed 4096 bytes so oldest are evicted.
        let chunk = bytes::Bytes::from(vec![b'x'; 1500]);
        rt.push_raw(chunk.clone());
        rt.push_raw(chunk.clone());
        rt.push_raw(chunk.clone()); // total so far: 4500 — first evicted
        // start_offset must have advanced past 0
        assert!(
            rt.ring.start_offset() > 0,
            "oldest chunks should be evicted once capacity is exceeded"
        );
    }

    // -----------------------------------------------------------------------
    // push_raw — last_output_at tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_raw_visible_content_advances_last_output_at() {
        let mut rt = new_runtime();
        assert!(rt.last_output_at.is_none());
        rt.push_raw(bytes::Bytes::from("hello world\n"));
        assert!(
            rt.last_output_at.is_some(),
            "visible output should set last_output_at"
        );
    }

    #[test]
    fn test_push_raw_pure_ansi_does_not_advance_last_output_at() {
        let mut rt = new_runtime();
        rt.push_raw(bytes::Bytes::from("\x1b[1A\x1b[2K\x1b[H"));
        assert!(
            rt.last_output_at.is_none(),
            "pure ANSI sequences should not advance last_output_at"
        );
    }

    // -----------------------------------------------------------------------
    // has_active_attach_client
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_active_attach_client_false_with_no_receivers() {
        let rt = new_runtime();
        assert!(
            !rt.has_active_attach_client(),
            "no receiver subscribed → should report no active client"
        );
    }

    #[test]
    fn test_has_active_attach_client_true_with_receiver() {
        let rt = new_runtime();
        let _rx = rt.broadcast_tx.subscribe();
        assert!(
            rt.has_active_attach_client(),
            "one receiver subscribed → should report active client"
        );
    }

    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------
    // has_visible_content
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_visible_content_plain_text() {
        assert!(has_visible_content("hello"));
        assert!(has_visible_content("$ "));
        assert!(has_visible_content("Do you want to continue?"));
    }

    #[test]
    fn test_has_visible_content_empty_and_whitespace() {
        assert!(!has_visible_content(""));
        assert!(!has_visible_content("   "));
        assert!(!has_visible_content("\t\n\r"));
    }

    #[test]
    fn test_has_visible_content_pure_ansi_csi() {
        // Cursor movement escape sequences — no visible characters.
        assert!(!has_visible_content("\x1b[2J"));
        assert!(!has_visible_content("\x1b[1A\x1b[1A\x1b[2K"));
        assert!(!has_visible_content("\x1b[H\x1b[2J\x1b[?25l"));
    }

    #[test]
    fn test_has_visible_content_ansi_with_text() {
        // ANSI wrapping around visible text → true.
        assert!(has_visible_content("\x1b[32mOK\x1b[0m"));
        assert!(has_visible_content("\x1b[1;31mError\x1b[0m"));
    }

    #[test]
    fn test_has_visible_content_osc_sequences() {
        // OSC title sequence — no visible characters.
        assert!(!has_visible_content("\x1b]0;my title\x07"));
        assert!(!has_visible_content("\x1b]2;Terminal\x1b\\"));
    }

    // -----------------------------------------------------------------------
    // is_completed / mark_completed
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_completed_running_returns_false() {
        let rt = new_runtime();
        assert!(
            !rt.is_completed(),
            "running session should not be completed"
        );
    }

    #[test]
    fn test_mark_completed_stopped() {
        use crate::session::SessionStatus;
        let mut rt = new_runtime();
        rt.mark_completed(SessionStatus::Stopped, Some(0));
        assert!(rt.is_completed());
        assert_eq!(rt.meta.exit_code, Some(0));
        assert!(rt.meta.ended_at.is_some());
        assert!(rt.completed_at.is_some());
    }

    #[test]
    fn test_mark_completed_failed_with_nonzero_exit() {
        use crate::session::SessionStatus;
        let mut rt = new_runtime();
        rt.mark_completed(SessionStatus::Failed, Some(1));
        assert!(rt.is_completed());
        assert_eq!(rt.meta.exit_code, Some(1));
    }

    #[test]
    fn test_mark_completed_is_idempotent() {
        use crate::session::SessionStatus;
        let mut rt = new_runtime();
        rt.mark_completed(SessionStatus::Stopped, Some(0));
        let first_ended_at = rt.meta.ended_at;
        // Second call should not overwrite ended_at.
        rt.mark_completed(SessionStatus::Stopped, Some(0));
        assert_eq!(
            rt.meta.ended_at, first_ended_at,
            "mark_completed should not overwrite ended_at on second call"
        );
    }
}
