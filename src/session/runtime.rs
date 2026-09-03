//! One live session: the PTY, its reader/writer threads and the rendered
//! screen state that attach clients replay from.
//!
//! [`SessionRuntime`] is the in-memory half of a session; [`super::store`]
//! owns the registry of them and [`super::persist`] the on-disk half.

use std::{
    ffi::{OsStr, OsString},
    io::{ErrorKind, Read, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

use crate::{
    error::{AppError, Result},
    protocol::{LogResize, SessionSummary},
    session::persist::create_output_log,
};

use super::pty::{PtyHandle, RuntimeChild, TerminalSignals};
use super::scan::{PtyScanner, ScanOut};

use super::{
    SessionMeta, SessionStatus,
    persist::{OutputLog, append_event, append_resize_event},
    screen::safe_resize_parser,
};

// ---------------------------------------------------------------------------
// SessionRuntime
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSnapshot {
    pub app_cursor_keys: bool,
    pub bracketed_paste_mode: bool,
}

/// Lock-free publication of the session's input-affecting terminal modes.
///
/// Every attach relay has to know whether the child currently has application
/// cursor keys and bracketed paste enabled, and has to notice the moment either
/// flips. Deriving that from the rendered screen requires the session's
/// `RwLock`, and doing it once per output chunk turned the reader thread's lock
/// into a contention point under load. The reader publishes the two bits into
/// this atomic instead, so relays can poll them for free.
#[derive(Debug, Default)]
pub struct SharedModes(std::sync::atomic::AtomicU8);

const MODE_BIT_APP_CURSOR_KEYS: u8 = 1 << 0;
const MODE_BIT_BRACKETED_PASTE: u8 = 1 << 1;

impl SharedModes {
    pub fn load(&self) -> ModeSnapshot {
        let bits = self.0.load(std::sync::atomic::Ordering::Relaxed);
        ModeSnapshot {
            app_cursor_keys: bits & MODE_BIT_APP_CURSOR_KEYS != 0,
            bracketed_paste_mode: bits & MODE_BIT_BRACKETED_PASTE != 0,
        }
    }

    fn store(&self, modes: ModeSnapshot) {
        let mut bits = 0;
        if modes.app_cursor_keys {
            bits |= MODE_BIT_APP_CURSOR_KEYS;
        }
        if modes.bracketed_paste_mode {
            bits |= MODE_BIT_BRACKETED_PASTE;
        }
        self.0.store(bits, std::sync::atomic::Ordering::Relaxed);
    }
}

pub struct SessionRuntime {
    pub meta: SessionMeta,
    /// Absolute path to the session's working directory (`sessions/<id>/`).
    pub dir: PathBuf,
    /// Sends canonical filtered PTY output chunks to all live attach
    /// subscribers. `Bytes` is already reference counted, so subscribers share
    /// one allocation without an extra `Arc` indirection.
    pub broadcast_tx: broadcast::Sender<Bytes>,
    /// Broadcasts PTY resize events (rows, cols) to all attach subscribers.
    pub resize_tx: broadcast::Sender<(u16, u16)>,
    /// PTY ownership: master fd, writer channel, child process.
    pub pty: PtyHandle,
    /// Current PTY dimensions, updated on every successful resize.
    pub pty_size: Option<(u16, u16)>,
    /// Canonical filtered-stream resize history for log replay.
    pub resize_history: Vec<LogResize>,
    pub completed_at: Option<Instant>,
    /// Set to `true` once the completed state has been written to the database.
    pub persisted: bool,
    pub requested_final_status: Option<SessionStatus>,
    /// Total length of the canonical filtered PTY output stream for persistence and replay.
    pub raw_total_bytes: u64,
    /// Total length of meaningful PTY output bytes that changed the terminal state.
    pub last_total_bytes: u64,
    /// Timestamp the runtime was created (PTY spawned). Used as a fallback
    /// silence anchor for sessions that have never produced any meaningful
    /// output, so they still surface via `silent_candidates` after a grace
    /// period instead of being silently ignored forever.
    pub spawned_at: Instant,
    /// Timestamp of the last meaningful output chunk.
    pub last_output_epoch: Option<Instant>,
    /// Timestamp of the last input bytes forwarded to the PTY.
    pub last_input_at: Option<Instant>,
    /// Timestamp of the last interactive attach action (input/resize).
    pub last_attach_activity_at: Option<Instant>,
    /// Number of currently connected local clients for this session.
    pub attach_count: usize,
    /// Timestamp of the last *successful* notification delivery for this session.
    pub last_notified_at: Option<Instant>,
    /// The value of `last_output_at` at the time the last notification was sent.
    pub notified_output_epoch: Option<Instant>,
    /// Live rendered terminal state for attach snapshot restoration.
    pub screen_parser: vt100::Parser,
    /// How many scrolled-off rows `screen_parser` retains; applied at spawn
    /// and on every resize rebuild.  From `AppConfig::screen_scrollback_rows`.
    pub screen_scrollback_rows: usize,
    /// Window/icon title, progress and cursor-shape notifications the session
    /// last emitted. The screen parser does not model these, so the reader
    /// thread's scanner tracks them and publishes them here for attach
    /// snapshot restoration.
    pub terminal_signals: TerminalSignals,
    /// Lock-free mirror of `mode_snapshot()` for the attach relays.
    pub shared_modes: Arc<SharedModes>,
    /// Set once the PTY reader has reached EOF or a terminal read error.
    pub output_closed: bool,
    pub notifications_enabled: bool,
}

/// Capacity of the queue between attach input and the PTY writer thread.
///
/// Sized for pastes: a bracketed paste arrives as a burst of frames, and a full
/// queue makes the daemon reject input with `SessionError::Busy`, which the
/// client surfaces as dropped keystrokes. The writer thread drains the queue at
/// PTY speed, so a deep queue costs only memory.
const PTY_WRITER_QUEUE_CAPACITY: usize = 4096;

/// Size of the PTY reader thread's read buffer.
///
/// Read syscalls dominate the per-chunk cost once scanning is cheap, so the
/// buffer is sized to swallow a burst of child output in as few reads — and
/// therefore as few downstream chunks, broadcasts and IPC frames — as possible.
const PTY_READ_BUFFER_BYTES: usize = 64 * 1024;

impl SessionRuntime {
    /// Current terminal mode snapshot (DECCKM, bracketed paste).
    pub fn mode_snapshot(&self) -> ModeSnapshot {
        let screen = self.screen_parser.screen();
        ModeSnapshot {
            app_cursor_keys: screen.application_cursor(),
            bracketed_paste_mode: screen.bracketed_paste(),
        }
    }

    /// Push a filtered PTY chunk into the canonical retained stream.
    ///
    /// `meaningful_len` is the subset of `filtered_data` that changed what the
    /// user can see, as measured by the reader thread's scanner during the same
    /// pass that produced `filtered_data`.
    ///
    /// Returns the current cursor position so the caller can answer terminal
    /// queries without re-locking.
    pub fn push_output(&mut self, filtered_data: &[u8], meaningful_len: usize) -> (u16, u16) {
        if !filtered_data.is_empty() {
            self.raw_total_bytes = self
                .raw_total_bytes
                .saturating_add(filtered_data.len() as u64);
            self.screen_parser.process(filtered_data);
            if meaningful_len > 0 {
                self.last_total_bytes = self.last_total_bytes.saturating_add(meaningful_len as u64);
                self.last_output_epoch = Some(Instant::now());
            }
            self.shared_modes.store(self.mode_snapshot());
        }

        self.screen_parser.screen().cursor_position()
    }

    /// The output epoch used for silence/notification bookkeeping.
    ///
    /// Falls back to `spawned_at` when the session has never produced any
    /// meaningful output, so a process that starts up and then silently
    /// blocks (e.g. waiting on a password prompt before printing anything)
    /// is still treated as having a well-defined "since when has this been
    /// silent" anchor instead of being permanently invisible to silence
    /// detection.
    pub fn effective_output_epoch(&self) -> Instant {
        self.last_output_epoch.unwrap_or(self.spawned_at)
    }

    /// Timestamp of the most recent user-driven activity of any kind:
    /// text input, mouse clicks/hover (delivered as input bytes), resizes,
    /// attach heartbeats and attaches themselves. Notification suppression
    /// treats output that closely follows this epoch as a reaction to the
    /// user rather than the program asking for attention.
    pub fn user_activity_epoch(&self) -> Option<Instant> {
        match (self.last_input_at, self.last_attach_activity_at) {
            (Some(input), Some(attach)) => Some(input.max(attach)),
            (input, attach) => input.or(attach),
        }
    }

    /// Build a `SessionSummary` snapshot from the current runtime state.
    pub fn to_summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.meta.id.clone(),
            title: self.meta.title.clone(),
            tags: self.meta.tags.clone(),
            command: self.meta.command.clone(),
            args: self.meta.args.clone(),
            pid: self.meta.pid,
            status: self.meta.status.as_str().to_string(),
            created_at: self.meta.created_at,
            started_at: self.meta.started_at,
            ended_at: self.meta.ended_at,
            cwd: self.meta.cwd.clone(),
            input_needed: self.input_needed(),
            notifications_enabled: self.notifications_enabled,
            node: None,
            last_total_bytes: self.last_total_bytes,
            last_output_epoch: self.last_output_epoch.and_then(instant_to_utc),
            rows: self.pty_size.map(|(rows, _)| rows),
            cols: self.pty_size.map(|(_, cols)| cols),
            attach_count: self.attach_count,
        }
    }

    /// Bytes that restore the session's visible terminal state on a freshly
    /// attached client: the rendered screen plus the window/icon title and
    /// progress notifications the terminal parser does not model.
    pub fn attach_snapshot_bytes(&self) -> Vec<u8> {
        let mut snapshot = self.screen_parser.screen().state_formatted();
        snapshot.extend_from_slice(&self.terminal_signals.restore_bytes());
        snapshot
    }

    pub fn render_logs(&self, tail: usize, keep_color: bool, term_cols: u16) -> Vec<u8> {
        super::logs::render_screen(&self.screen_parser, tail, keep_color, term_cols)
    }

    pub fn register_attach_client(&mut self) {
        self.attach_count = self.attach_count.saturating_add(1);
        // Attaching is itself user activity: someone just opened this
        // session and saw its current state.
        self.last_attach_activity_at = Some(Instant::now());
        trace!(
            session_id = %self.meta.id,
            attach_count = self.attach_count,
            "attach client registered"
        );
    }

    pub fn mark_attach_activity(&mut self) {
        debug!(
            session_id = %self.meta.id,
            attach_count = self.attach_count,
            "interactive attach activity marked"
        );
        self.last_attach_activity_at = Some(Instant::now());
    }

    pub fn detach_attach_client(&mut self) {
        self.attach_count = self.attach_count.saturating_sub(1);
        debug!(
            session_id = %self.meta.id,
            attach_count = self.attach_count,
            "attach client detached"
        );
        if self.attach_count == 0 {
            self.clear_attach_state();
        }
    }

    pub fn clear_attach_state(&mut self) {
        debug!(session_id = %self.meta.id, "attach presence cleared");
        self.attach_count = 0;
        // `last_attach_activity_at` deliberately survives detach: it records
        // when a user last *saw and touched* the session, which stays true
        // after they disconnect. Clearing it would make a just-detached
        // session immediately eligible for a notification about the very
        // output the user had just read.
    }

    pub fn input_needed(&self) -> bool {
        matches!(self.meta.status, SessionStatus::Running)
            && self.notified_output_epoch.is_some()
            && self.notified_output_epoch == Some(self.effective_output_epoch())
    }

    pub fn set_notifications_enabled(&mut self, enabled: bool) {
        if self.notifications_enabled == enabled {
            return;
        }
        self.notifications_enabled = enabled;
        info!(
            session_id = %self.meta.id,
            notifications_enabled = enabled,
            "session notification setting updated"
        );
        let event = if enabled {
            "notifications enabled"
        } else {
            "notifications disabled"
        };
        if let Err(err) = append_event(&self.dir, event) {
            warn!(
                session_id = %self.meta.id,
                %err,
                "failed to persist notification-setting event"
            );
        }
    }

    /// Returns `true` when at least one attach subscriber is currently live.
    #[allow(dead_code)]
    pub fn has_active_attach_client(&self) -> bool {
        self.attach_count > 0
    }

    /// Checks child exit status and updates `meta.status`. Returns `true` if completed.
    pub fn refresh_status(&mut self) -> bool {
        if self.is_completed() {
            if self.completed_at.is_none() {
                self.completed_at = Some(Instant::now());
            }
            return true;
        }

        match self.pty.try_wait() {
            Ok(Some(code)) => {
                debug!(session_id = %self.meta.id, exit_code = code, "child process exited");
                let status = self.requested_final_status.unwrap_or_else(|| {
                    if code == 0 {
                        SessionStatus::Stopped
                    } else {
                        SessionStatus::Failed
                    }
                });
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
        self.requested_final_status = None;
        if let Some(code) = exit_code {
            self.meta.exit_code = Some(code);
        }
        if self.completed_at.is_none() {
            self.completed_at = Some(Instant::now());
        }
        info!(
            session_id = %self.meta.id,
            status = status.as_str(),
            exit_code = ?exit_code,
            "marking PTY session as completed"
        );
        self.pty.release_resources();
        let event = match &self.meta.status {
            SessionStatus::Stopped => format!(
                "session stopped exit_code={}",
                self.meta.exit_code.unwrap_or(0)
            ),
            SessionStatus::Killed => format!(
                "session killed exit_code={}",
                self.meta.exit_code.unwrap_or(-1)
            ),
            SessionStatus::Failed => format!(
                "session failed exit_code={}",
                self.meta.exit_code.unwrap_or(-1)
            ),
            other => format!("session ended status={}", other.as_str()),
        };
        if let Err(err) = append_event(&self.dir, &event) {
            warn!(session_id = %self.meta.id, %err, "failed to persist PTY session completion event");
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(
            self.meta.status,
            SessionStatus::Stopped | SessionStatus::Killed | SessionStatus::Failed
        )
    }

    pub fn resize_pty(&mut self, rows: u16, cols: u16) -> bool {
        if rows == 0 || cols == 0 {
            debug!(session_id = %self.meta.id, rows, cols, "ignoring invalid PTY resize request");
            return false;
        }
        // Skip resize if the PTY is already at the requested size.
        if self.pty_size == Some((rows, cols)) {
            debug!(session_id = %self.meta.id, rows, cols, "PTY already at requested size, skipping resize");
            return true;
        }
        let resized = self.pty.resize(rows, cols);
        debug!(session_id = %self.meta.id, rows, cols, resized, "PTY resize attempted");
        if resized {
            self.pty_size = Some((rows, cols));
            safe_resize_parser(
                &mut self.screen_parser,
                rows,
                cols,
                self.screen_scrollback_rows,
            );
            self.resize_history.push(LogResize {
                offset: self.raw_total_bytes,
                rows,
                cols,
            });
            // Notify all attached clients about the new size.
            let _ = self.resize_tx.send((rows, cols));
        }
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

/// Spawns a PTY-backed child process and returns an `Arc<RwLock<SessionRuntime>>`.
/// Reader and writer threads are started automatically and share ownership via the Arc.
/// `session_dir` is the absolute path for the session's working files; the caller
/// is responsible for computing it (typically `sessions_dir.join(&meta.id)`).
pub fn spawn_session(
    meta: &mut SessionMeta,
    session_dir: PathBuf,
    rows: u16,
    cols: u16,
    notifications_enabled: bool,
    screen_scrollback_rows: usize,
) -> Result<Arc<RwLock<SessionRuntime>>> {
    let full_dir = session_dir;
    let reader_dir = full_dir.clone();
    info!(
        session_id = %meta.id,
        command = %meta.command,
        args = ?meta.args,
        cwd = ?meta.cwd,
        rows,
        cols,
        notifications_enabled,
        "spawning PTY session runtime"
    );
    std::fs::create_dir_all(&full_dir)?;

    let spawn_env = load_spawn_environment();
    let command_cwd = meta
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| full_dir.clone());
    let search_path = spawn_env
        .iter()
        .find(|(key, _)| env_key_eq(key, OsStr::new("PATH")))
        .map(|(_, value)| value);
    let Ok(cmd) = which::which_in(&meta.command, search_path, &command_cwd) else {
        return Err(AppError::Protocol(format!(
            "command not found: {}",
            meta.command
        )));
    };

    let mut cmd = CommandBuilder::new(cmd);
    cmd.env_clear();
    for (key, value) in spawn_env {
        cmd.env(key, value);
    }
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

    create_output_log(&full_dir)?;
    append_event(&full_dir, "session created")?;
    append_resize_event(&full_dir, 0, rows, cols)?;

    let started_pid = meta
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".to_string());
    append_event(&full_dir, &format!("session started pid={started_pid}"))?;

    // Broadcast channel: each live attach subscriber holds a Receiver.
    let (broadcast_tx, _initial_rx) = broadcast::channel::<Bytes>(256);

    // Resize broadcast channel: notifies all attached clients of PTY resize.
    let (resize_tx, _initial_resize_rx) = broadcast::channel::<(u16, u16)>(16);

    // Writer channel: the dedicated write thread owns the PTY writer so that
    // sending input never blocks the tokio runtime.
    let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(PTY_WRITER_QUEUE_CAPACITY);

    // PTY writer thread — drains writer_rx and forwards bytes to the child.
    let writer_session_id = meta.id.clone();
    std::thread::spawn(move || {
        debug!(session_id = %writer_session_id, "PTY writer thread started");
        let mut writer = writer;
        while let Some(data) = writer_rx.blocking_recv() {
            trace!(session_id = %writer_session_id, bytes = data.len(), "forwarding PTY stdin bytes");
            if let Err(err) = writer.write_all(&data).and_then(|_| writer.flush()) {
                warn!(session_id = %writer_session_id, %err, "PTY writer thread failed");
                break;
            }
        }
        debug!(session_id = %writer_session_id, "PTY writer thread stopped");
    });

    let pty_handle = PtyHandle {
        child: runtime_child,
        writer_tx: writer_tx.clone(),
        pty_master: parking_lot::Mutex::new(Some(master)),
    };

    let runtime = Arc::new(RwLock::new(SessionRuntime {
        meta: meta.clone(),
        dir: full_dir,
        last_total_bytes: 0,
        raw_total_bytes: 0,
        broadcast_tx: broadcast_tx.clone(),
        resize_tx,
        pty: pty_handle,
        pty_size: Some((rows, cols)),
        resize_history: vec![LogResize {
            offset: 0,
            rows,
            cols,
        }],
        completed_at: None,
        persisted: false,
        requested_final_status: None,
        spawned_at: Instant::now(),
        last_output_epoch: None,
        last_input_at: None,
        last_attach_activity_at: None,
        attach_count: 0,
        notified_output_epoch: None,
        last_notified_at: None,
        screen_parser: vt100::Parser::new(rows, cols, screen_scrollback_rows),
        screen_scrollback_rows,
        terminal_signals: TerminalSignals::default(),
        shared_modes: Arc::new(SharedModes::default()),
        output_closed: false,
        notifications_enabled,
    }));

    // PTY reader thread: reads raw bytes, derives one canonical filtered stream,
    // and retains/broadcasts only that filtered stream.
    let runtime_reader = runtime.clone();
    let broadcast_tx_reader = broadcast_tx;
    let reader_session_id = meta.id.clone();
    std::thread::spawn(move || {
        debug!(session_id = %reader_session_id, "PTY reader thread started");
        if let Err(err) = append_event(&reader_dir, "pty reader started") {
            warn!(session_id = %reader_session_id, %err, "failed to persist PTY reader start event");
        }
        let mut buf = vec![0u8; PTY_READ_BUFFER_BYTES];
        let mut reader = reader;
        let mut scanner = PtyScanner::new();
        let mut scan_out = ScanOut::default();
        let mut output_log = OutputLog::open(&reader_dir);
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    runtime_reader.write().output_closed = true;
                    debug!(session_id = %reader_session_id, "PTY reader thread reached EOF");
                    if let Err(err) = append_event(&reader_dir, "pty reader reached EOF") {
                        warn!(session_id = %reader_session_id, %err, "failed to persist PTY reader EOF event");
                    }
                    break;
                }
                Ok(n) => {
                    trace!(session_id = %reader_session_id, bytes = n, "read PTY output chunk");

                    // One pass over the chunk produces everything downstream
                    // needs: the canonical filtered stream, the probes that
                    // need a reply, the retained one-way notifications, and the
                    // meaningful-activity byte count.
                    scanner.scan(&buf[..n], &mut scan_out);

                    if scan_out.filtered.is_empty() && scan_out.queries.is_empty() {
                        continue;
                    }

                    let filtered = Bytes::copy_from_slice(&scan_out.filtered);
                    let meaningful_len = scan_out.meaningful_bytes();
                    let changed_signals = scanner.take_changed_signals();

                    // Single write lock: advance the rendered screen and the
                    // stream counters, publish any changed notifications, and
                    // read back the cursor position for query replies.
                    let cursor_position = {
                        let mut rt = runtime_reader.write();
                        if let Some(signals) = changed_signals {
                            rt.terminal_signals = signals;
                        }
                        rt.push_output(&filtered, meaningful_len)
                    };

                    if let Err(err) = output_log.append(&filtered) {
                        warn!(session_id = %reader_session_id, %err, "failed to persist PTY output chunk");
                    }

                    // Broadcast canonical filtered output to all live
                    // subscribers (non-blocking; lagged receivers re-sync from
                    // the persisted log on the next tick).
                    if !filtered.is_empty()
                        && let Ok(receiver_count) = broadcast_tx_reader.send(filtered)
                    {
                        trace!(
                            session_id = %reader_session_id,
                            receiver_count,
                            "broadcast filtered PTY output chunk to live subscribers"
                        );
                    }

                    // Answer the capability probes that have a session-global
                    // reply (CPR/DSR and the OSC colour queries).
                    let mut writer_closed = false;
                    for query in scan_out.queries.drain(..) {
                        let resp = query.response(cursor_position);
                        trace!(
                            session_id = %reader_session_id,
                            ?query,
                            bytes = resp.len(),
                            "responding to detached terminal capability query"
                        );
                        if writer_tx.blocking_send(resp).is_err() {
                            warn!(
                                session_id = %reader_session_id,
                                "failed to queue detached terminal query response because PTY writer closed"
                            );
                            writer_closed = true;
                            break;
                        }
                    }
                    if writer_closed {
                        break;
                    }
                }
                Err(err)
                    if matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    trace!(session_id = %reader_session_id, kind = ?err.kind(), "PTY reader retrying after transient read condition");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    runtime_reader.write().output_closed = true;
                    warn!(session_id = %reader_session_id, %err, "PTY reader thread failed");
                    if let Err(append_err) =
                        append_event(&reader_dir, &format!("pty reader error: {err}"))
                    {
                        warn!(session_id = %reader_session_id, %append_err, "failed to persist PTY reader error event");
                    }
                    break;
                }
            }
        }
        debug!(session_id = %reader_session_id, "PTY reader thread stopped");
    });

    info!(
        session_id = %meta.id,
        pid = ?meta.pid,
        writer_queue_capacity = PTY_WRITER_QUEUE_CAPACITY,
        "PTY session runtime spawned"
    );
    Ok(runtime)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_spawn_environment() -> Vec<(OsString, OsString)> {
    let inherited = std::env::vars_os().collect::<Vec<_>>();

    #[cfg(windows)]
    {
        match load_windows_user_environment() {
            Ok(refreshed) => merge_spawn_environment(inherited, refreshed),
            Err(err) => {
                warn!(%err, "failed to refresh Windows user environment; using daemon environment");
                inherited
            }
        }
    }

    #[cfg(not(windows))]
    inherited
}

fn merge_spawn_environment(
    mut inherited: Vec<(OsString, OsString)>,
    refreshed: Vec<(OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    for (key, value) in refreshed {
        inherited.retain(|(existing, _)| !env_key_eq(existing, &key));
        inherited.push((key, value));
    }
    inherited
}

#[cfg(windows)]
fn env_key_eq(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

#[cfg(not(windows))]
fn env_key_eq(left: &OsStr, right: &OsStr) -> bool {
    left == right
}

#[cfg(windows)]
fn load_windows_user_environment() -> std::io::Result<Vec<(OsString, OsString)>> {
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Security::TOKEN_QUERY,
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };

    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut environment = null_mut();
        let created = CreateEnvironmentBlock(&mut environment, token, 0);
        let create_error = std::io::Error::last_os_error();
        let _ = CloseHandle(token);
        if created == 0 {
            return Err(create_error);
        }

        let mut block = Vec::new();
        let mut cursor = environment.cast::<u16>();
        loop {
            let current = *cursor;
            block.push(current);
            cursor = cursor.add(1);
            if current == 0 && *cursor == 0 {
                block.push(0);
                break;
            }
        }

        let _ = DestroyEnvironmentBlock(environment.cast_const());
        Ok(parse_windows_environment_block(&block))
    }
}

#[cfg(windows)]
fn parse_windows_environment_block(block: &[u16]) -> Vec<(OsString, OsString)> {
    use std::os::windows::ffi::OsStringExt;

    block
        .split(|unit| *unit == 0)
        .take_while(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let separator = entry.iter().position(|unit| *unit == b'=' as u16)?;
            if separator == 0 {
                return None;
            }
            Some((
                OsString::from_wide(&entry[..separator]),
                OsString::from_wide(&entry[separator + 1..]),
            ))
        })
        .collect()
}

pub(crate) fn instant_to_utc(instant: Instant) -> Option<DateTime<Utc>> {
    let elapsed = chrono::TimeDelta::from_std(instant.elapsed()).ok()?;
    Utc::now().checked_sub_signed(elapsed)
}

fn format_command_for_display(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return command.to_string();
    }
    format!("{} {}", command, args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn refreshed_environment_overrides_inherited_values() {
        let merged = merge_spawn_environment(
            vec![
                (OsString::from("PATH"), OsString::from("stale")),
                (OsString::from("DAEMON_ONLY"), OsString::from("kept")),
            ],
            vec![
                (OsString::from("PATH"), OsString::from("fresh")),
                (OsString::from("NEW_VALUE"), OsString::from("added")),
            ],
        );

        assert_eq!(
            merged
                .iter()
                .find(|(key, _)| env_key_eq(key, OsStr::new("PATH")))
                .map(|(_, value)| value.as_os_str()),
            Some(OsStr::new("fresh"))
        );
        assert!(merged.iter().any(|(key, value)| {
            key == OsStr::new("DAEMON_ONLY") && value == OsStr::new("kept")
        }));
        assert!(merged.iter().any(|(key, value)| {
            key == OsStr::new("NEW_VALUE") && value == OsStr::new("added")
        }));
    }

    #[cfg(windows)]
    #[test]
    fn refreshed_environment_keys_are_case_insensitive_on_windows() {
        let merged = merge_spawn_environment(
            vec![(OsString::from("Path"), OsString::from("stale"))],
            vec![(OsString::from("PATH"), OsString::from("fresh"))],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].1, OsString::from("fresh"));
    }

    #[cfg(windows)]
    #[test]
    fn parses_windows_environment_block() {
        let mut block = "PATH=C:\\Tools\0EMPTY=\0BROKEN\0=C:=C:\\Work\0"
            .encode_utf16()
            .collect::<Vec<_>>();
        block.push(0);

        let parsed = parse_windows_environment_block(&block);

        assert_eq!(
            parsed,
            vec![
                (OsString::from("PATH"), OsString::from("C:\\Tools")),
                (OsString::from("EMPTY"), OsString::new()),
            ]
        );
    }

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

    fn make_test_child_with_exit_code(exit_code: i32) -> RuntimeChild {
        RuntimeChild::Mock {
            exit_code: Some(exit_code),
        }
    }

    fn new_runtime_with(status: SessionStatus, exit_code: i32) -> SessionRuntime {
        use crate::session::SessionMeta;
        let meta = SessionMeta {
            id: "rt_tst01".to_string(),
            title: None,
            tags: vec![],
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            ended_at: None,
            status,
            pid: None,
            exit_code: None,
        };
        let (broadcast_tx, _rx) = tokio::sync::broadcast::channel(4);
        let (resize_tx, _resize_rx) = tokio::sync::broadcast::channel(4);
        let (writer_tx, _wrx) = tokio::sync::mpsc::channel(8);
        SessionRuntime {
            meta,
            dir: std::env::temp_dir().join("oly_runtime_unit_tests"),
            last_total_bytes: 0,
            raw_total_bytes: 0,
            broadcast_tx,
            resize_tx,
            pty: PtyHandle {
                child: make_test_child_with_exit_code(exit_code),
                writer_tx,
                pty_master: parking_lot::Mutex::new(None),
            },
            pty_size: None,
            resize_history: Vec::new(),
            completed_at: None,
            persisted: false,
            requested_final_status: None,
            spawned_at: Instant::now(),
            last_output_epoch: None,
            last_input_at: None,
            last_attach_activity_at: None,
            attach_count: 0,
            last_notified_at: None,
            notified_output_epoch: None,
            screen_parser: vt100::Parser::new(24, 80, 1000),
            screen_scrollback_rows: 1000,
            terminal_signals: Default::default(),
            shared_modes: Default::default(),
            output_closed: false,
            notifications_enabled: true,
        }
    }

    fn new_runtime() -> SessionRuntime {
        new_runtime_with(SessionStatus::Running, 0)
    }

    /// Drive one raw chunk through the reader thread's pipeline: scan it, then
    /// push the filtered result into the runtime the way the reader does.
    fn push_scanned(rt: &mut SessionRuntime, raw: &[u8]) -> (u16, u16) {
        let mut scanner = PtyScanner::new();
        let mut out = ScanOut::default();
        scanner.scan(raw, &mut out);
        if let Some(signals) = scanner.take_changed_signals() {
            rt.terminal_signals = signals;
        }
        let meaningful = out.meaningful_bytes();
        rt.push_output(&out.filtered, meaningful)
    }

    fn refresh_until_completed(rt: &mut SessionRuntime) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if rt.refresh_status() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("session did not complete within the expected refresh window");
    }

    #[test]
    fn test_push_output_enables_bracketed_paste() {
        let mut rt = new_runtime();
        assert!(!rt.mode_snapshot().bracketed_paste_mode);
        push_scanned(&mut rt, b"text \x1b[?2004h more");
        assert_eq!(
            rt.mode_snapshot(),
            ModeSnapshot {
                app_cursor_keys: false,
                bracketed_paste_mode: true,
            }
        );
        assert!(
            rt.mode_snapshot().bracketed_paste_mode,
            "bracketed_paste_mode should be set after \\x1b[?2004h"
        );
    }

    #[test]
    fn test_push_output_disables_bracketed_paste() {
        let mut rt = new_runtime();
        push_scanned(&mut rt, b"\x1b[?2004h");
        assert!(rt.mode_snapshot().bracketed_paste_mode);
        push_scanned(&mut rt, b"\x1b[?2004l");
        assert_eq!(
            rt.mode_snapshot(),
            ModeSnapshot {
                app_cursor_keys: false,
                bracketed_paste_mode: false,
            }
        );
        assert!(
            !rt.mode_snapshot().bracketed_paste_mode,
            "bracketed_paste_mode should be cleared after \\x1b[?2004l"
        );
    }

    #[test]
    fn test_push_output_enables_app_cursor_keys() {
        let mut rt = new_runtime();
        assert!(!rt.mode_snapshot().app_cursor_keys);
        push_scanned(&mut rt, b"\x1b[?1h");
        assert_eq!(
            rt.mode_snapshot(),
            ModeSnapshot {
                app_cursor_keys: true,
                bracketed_paste_mode: false,
            }
        );
        assert!(
            rt.mode_snapshot().app_cursor_keys,
            "app_cursor_keys should be set after DECCKM enable"
        );
    }

    #[test]
    fn test_push_output_disables_app_cursor_keys() {
        let mut rt = new_runtime();
        push_scanned(&mut rt, b"\x1b[?1h");
        assert!(rt.mode_snapshot().app_cursor_keys);
        push_scanned(&mut rt, b"\x1b[?1l");
        assert_eq!(
            rt.mode_snapshot(),
            ModeSnapshot {
                app_cursor_keys: false,
                bracketed_paste_mode: false,
            }
        );
        assert!(
            !rt.mode_snapshot().app_cursor_keys,
            "app_cursor_keys should be cleared after DECCKM disable"
        );
    }

    // -----------------------------------------------------------------------
    // push_output — last_output_epoch tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_output_non_empty_advances_last_output_epoch() {
        let mut rt = new_runtime();
        assert!(rt.last_output_epoch.is_none());
        push_scanned(&mut rt, b"hello world\n");
        assert!(
            rt.last_output_epoch.is_some(),
            "non-empty output should set last_output_epoch"
        );
    }

    #[test]
    fn test_push_output_empty_does_not_advance_last_output_epoch() {
        let mut rt = new_runtime();
        push_scanned(&mut rt, b"");
        assert!(
            rt.last_output_epoch.is_none(),
            "empty output should not advance last_output_epoch"
        );
    }

    #[test]
    fn test_push_output_uses_filtered_bytes_for_snapshot_and_offsets() {
        let mut rt = new_runtime();
        let filtered = bytes::Bytes::from_static(b"beforeafter");

        push_scanned(&mut rt, filtered.as_ref());

        assert_eq!(
            rt.screen_parser.screen().contents().trim_end(),
            "beforeafter"
        );
        assert_eq!(rt.last_total_bytes, 11);
    }

    #[test]
    fn test_push_output_tracks_total_bytes_from_filtered_stream() {
        let mut rt = new_runtime();
        let filtered = bytes::Bytes::from_static(b"beforeafter");

        push_scanned(&mut rt, filtered.as_ref());

        assert_eq!(rt.last_total_bytes, 11);
        assert_eq!(rt.raw_total_bytes, 11);
    }

    #[test]
    fn test_push_output_drops_fully_stripped_chunks_from_snapshot() {
        let mut rt = new_runtime();
        push_scanned(&mut rt, &[]);

        assert!(rt.screen_parser.screen().contents().trim().is_empty());
        assert_eq!(rt.last_total_bytes, 0);
        assert_eq!(rt.raw_total_bytes, 0);
    }

    #[test]
    fn test_push_output_non_visible_osc_does_not_advance_meaningful_output() {
        let mut rt = new_runtime();

        push_scanned(&mut rt, b"\x1b]9;4;3;0\x07");

        assert!(rt.last_output_epoch.is_none());
        assert_eq!(rt.last_total_bytes, 0);
        assert_eq!(rt.raw_total_bytes, b"\x1b]9;4;3;0\x07".len() as u64);
    }

    #[test]
    fn test_push_output_subtracts_busy_osc_bytes_from_meaningful_total() {
        let mut rt = new_runtime();

        push_scanned(&mut rt, b"hello\x1b]9;4;3;0\x07");

        assert!(rt.last_output_epoch.is_some());
        assert_eq!(rt.last_total_bytes, 5);
        assert_eq!(rt.raw_total_bytes, b"hello\x1b]9;4;3;0\x07".len() as u64);
    }

    #[test]
    fn test_push_output_title_only_chunk_does_not_advance_activity() {
        // A chunk that only retitles the window (the classic "Action
        // Required" flip) must not reset the silence clock: it almost always
        // announces that the program is waiting for input.
        let mut rt = new_runtime();
        let chunk = b"\x1b]0;[ ! ] Action Required | build\x07";

        push_scanned(&mut rt, chunk);

        assert!(rt.last_output_epoch.is_none());
        assert_eq!(rt.last_total_bytes, 0);
        assert_eq!(rt.raw_total_bytes, chunk.len() as u64);
    }

    #[test]
    fn test_push_output_title_and_text_only_counts_text() {
        let mut rt = new_runtime();

        push_scanned(&mut rt, b"\x1b]0;build\x07done");

        assert!(rt.last_output_epoch.is_some());
        assert_eq!(rt.last_total_bytes, 4);
    }

    #[test]
    fn test_attach_snapshot_restores_title_progress_and_cursor_style() {
        // The screen parser drops Operating System Commands and does not model
        // the cursor shape, so an attaching client would otherwise keep the
        // window title, progress indicator and cursor shape of its own shell.
        let mut rt = new_runtime();

        push_scanned(
            &mut rt,
            b"\x1b]0;relay build\x07\x1b]9;4;3;0\x07\x1b[6 qbuilding",
        );

        let snapshot = rt.attach_snapshot_bytes();
        let contains = |needle: &[u8]| {
            snapshot
                .windows(needle.len())
                .any(|window| window == needle)
        };

        assert!(contains(b"\x1b]0;relay build\x07"));
        assert!(contains(b"\x1b]9;4;3;0\x07"));
        assert!(contains(b"\x1b[6 q"));
        assert!(rt.screen_parser.screen().contents().contains("building"));
    }

    #[test]
    fn test_attach_snapshot_omits_signals_for_a_silent_session() {
        let mut rt = new_runtime();

        push_scanned(&mut rt, b"plain output");

        assert_eq!(
            rt.attach_snapshot_bytes(),
            rt.screen_parser.screen().state_formatted()
        );
    }

    #[test]
    fn test_push_output_discounts_signal_bytes_in_large_chunks() {
        // The scanner accounts progress bytes exactly, so unlike the previous
        // size-limited heuristic a busy indicator is discounted no matter how
        // much other output shares its chunk.
        let mut rt = new_runtime();
        let progress = b"\x1b]9;4;3;0\x07";
        let mut chunk = vec![b'x'; 4096];
        chunk.extend_from_slice(progress);

        push_scanned(&mut rt, &chunk);

        assert_eq!(rt.last_total_bytes, 4096);
        assert_eq!(rt.raw_total_bytes, chunk.len() as u64);
    }

    // -----------------------------------------------------------------------
    // has_active_attach_client
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_active_attach_client_false_with_no_receivers() {
        let rt = new_runtime();
        assert!(
            !rt.has_active_attach_client(),
            "no registered client → should report no active client"
        );
    }

    #[test]
    fn test_has_active_attach_client_true_with_registered_client() {
        let mut rt = new_runtime();
        rt.register_attach_client();
        assert!(
            rt.has_active_attach_client(),
            "one registered client → should report active client"
        );
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
    fn test_mark_completed_releases_writer_handle() {
        use crate::session::SessionMeta;
        use tokio::sync::{broadcast, mpsc};

        let meta = SessionMeta {
            id: "rt_release".to_string(),
            title: None,
            tags: vec![],
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
        let (broadcast_tx, _rx) = broadcast::channel(4);
        let (resize_tx, _resize_rx) = broadcast::channel(4);
        let (writer_tx, mut writer_rx) = mpsc::channel(4);
        let mut rt = SessionRuntime {
            meta,
            dir: std::env::temp_dir().join("oly_runtime_release_test"),
            last_total_bytes: 0,
            raw_total_bytes: 0,
            broadcast_tx,
            resize_tx,
            pty: PtyHandle {
                child: make_test_child_with_exit_code(0),
                writer_tx,
                pty_master: parking_lot::Mutex::new(None),
            },
            pty_size: None,
            resize_history: Vec::new(),
            completed_at: None,
            persisted: false,
            requested_final_status: None,
            spawned_at: Instant::now(),
            last_output_epoch: None,
            last_input_at: None,
            last_attach_activity_at: None,
            attach_count: 0,
            last_notified_at: None,
            notified_output_epoch: None,
            screen_parser: vt100::Parser::new(24, 80, 1000),
            screen_scrollback_rows: 1000,
            terminal_signals: Default::default(),
            shared_modes: Default::default(),
            output_closed: false,
            notifications_enabled: true,
        };

        assert!(rt.pty.try_write_input(b"before".to_vec()).is_ok());
        assert_eq!(
            writer_rx
                .try_recv()
                .expect("writer should receive pre-close bytes"),
            b"before".to_vec()
        );

        rt.mark_completed(SessionStatus::Stopped, Some(0));

        assert!(
            rt.pty.try_write_input(b"after".to_vec()).is_err(),
            "completed sessions should reject further writes"
        );
    }

    #[test]
    fn test_refresh_status_marks_nonzero_exit_failed_without_stop_request() {
        let mut rt = new_runtime_with(SessionStatus::Running, 1);
        refresh_until_completed(&mut rt);
        assert!(matches!(rt.meta.status, SessionStatus::Failed));
        assert!(matches!(rt.meta.exit_code, Some(code) if code != 0));
    }

    #[test]
    fn test_refresh_status_marks_nonzero_exit_stopped_during_stop_request() {
        let mut rt = new_runtime_with(SessionStatus::Stopping, 1);
        rt.requested_final_status = Some(SessionStatus::Stopped);
        refresh_until_completed(&mut rt);
        assert!(matches!(rt.meta.status, SessionStatus::Stopped));
        assert!(matches!(rt.meta.exit_code, Some(code) if code != 0));
    }

    #[test]
    fn test_refresh_status_marks_nonzero_exit_killed_during_kill_request() {
        let mut rt = new_runtime_with(SessionStatus::Stopping, 1);
        rt.requested_final_status = Some(SessionStatus::Killed);
        refresh_until_completed(&mut rt);
        assert!(matches!(rt.meta.status, SessionStatus::Killed));
        assert!(matches!(rt.meta.exit_code, Some(code) if code != 0));
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
