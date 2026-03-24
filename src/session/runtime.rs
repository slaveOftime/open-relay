use std::{
    io::{ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self as std_mpsc, RecvTimeoutError},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    protocol::LogResize,
    session::persist::append_output,
};

use super::pty::{
    EscapeFilter, PtyHandle, RuntimeChild, extract_query_responses_no_client, has_visible_content,
};

use super::{
    SessionMeta, SessionStatus,
    cursor_tracker::CursorTracker,
    mode_tracker::{ModeSnapshot, ModeTracker},
    persist::{append_event, append_output_raw, append_resize_event},
    ring::RingBuffer,
};

// ---------------------------------------------------------------------------
// SessionRuntime
// ---------------------------------------------------------------------------

pub struct SessionRuntime {
    pub meta: SessionMeta,
    /// Absolute path to the session's working directory (`sessions/<id>/`).
    pub dir: PathBuf,
    /// Byte-limited ring buffer of canonical filtered PTY output.
    pub ring: RingBuffer,
    /// Total length of canonical filtered PTY output bytes.
    pub total_bytes: u64,
    /// Sends canonical filtered PTY output chunks to all live attach subscribers.
    pub broadcast_tx: broadcast::Sender<Arc<Bytes>>,
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
    /// Timestamp of the last visible output chunk; drives the notification engine.
    pub last_output_at: Option<Instant>,
    /// Timestamp of the last input bytes forwarded to the PTY.
    pub last_input_at: Option<Instant>,
    /// Timestamp of the last subscribe/attach action; coarse presence signal.
    pub last_attach_presence_at: Option<Instant>,
    /// Timestamp of the last interactive attach action (input/resize).
    pub last_attach_activity_at: Option<Instant>,
    /// Number of currently connected local clients for this session.
    pub attach_count: usize,
    /// Timestamp of the last *successful* notification delivery for this session.
    pub last_notified_at: Option<Instant>,
    /// The value of `last_output_at` at the time the last notification was sent.
    pub notified_output_epoch: Option<Instant>,
    /// Byte-level state machine for DEC private mode tracking.
    pub mode_tracker: ModeTracker,
    /// Set once the PTY reader has reached EOF or a terminal read error.
    pub output_closed: bool,
    pub notifications_enabled: bool,
}

const PTY_WRITER_QUEUE_CAPACITY: usize = 256;
const PTY_COALESCE_WINDOW: Duration = Duration::from_millis(15);
const PTY_COALESCE_MAX_BYTES: usize = 20 * 1024;

enum PtyOutputProcessorEvent {
    Chunk(Bytes),
    Close,
}

fn try_coalesce_pending_output(pending: &mut Vec<u8>, next: &[u8]) -> bool {
    if pending.len().saturating_add(next.len()) <= PTY_COALESCE_MAX_BYTES {
        pending.extend_from_slice(next);
        return true;
    }

    false
}

fn flush_pending_output(
    runtime: &Arc<Mutex<SessionRuntime>>,
    dir: &Path,
    session_id: &str,
    broadcast_tx: &broadcast::Sender<Arc<Bytes>>,
    pending: &mut Vec<u8>,
) -> bool {
    if pending.is_empty() {
        return true;
    }

    let filtered_data = Bytes::copy_from_slice(pending.as_slice());
    pending.clear();
    flush_filtered_output(runtime, dir, session_id, broadcast_tx, filtered_data)
}

fn flush_filtered_output(
    runtime: &Arc<Mutex<SessionRuntime>>,
    dir: &Path,
    session_id: &str,
    broadcast_tx: &broadcast::Sender<Arc<Bytes>>,
    filtered_data: Bytes,
) -> bool {
    match runtime.lock() {
        Ok(mut rt) => rt.retain_filtered_output(filtered_data.clone()),
        Err(_) => {
            warn!(
                session_id = %session_id,
                "failed to lock runtime for coalesced PTY output flush"
            );
            return false;
        }
    }

    if let Err(err) = append_output_raw(dir, &filtered_data) {
        warn!(
            session_id = %session_id,
            %err,
            "failed to persist coalesced PTY output chunk"
        );
    }

    if let Ok(receiver_count) = broadcast_tx.send(Arc::new(filtered_data)) {
        trace!(
            session_id = %session_id,
            receiver_count,
            "broadcast coalesced PTY output chunk to live subscribers"
        );
    }

    true
}

fn run_output_processor(
    runtime: Arc<Mutex<SessionRuntime>>,
    dir: PathBuf,
    session_id: String,
    broadcast_tx: broadcast::Sender<Arc<Bytes>>,
    output_rx: std_mpsc::Receiver<PtyOutputProcessorEvent>,
) {
    let mut pending = Vec::new();

    loop {
        let event = if pending.is_empty() {
            match output_rx.recv() {
                Ok(event) => Some(event),
                Err(_) => break,
            }
        } else {
            match output_rx.recv_timeout(PTY_COALESCE_WINDOW) {
                Ok(event) => Some(event),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    if !flush_pending_output(
                        &runtime,
                        &dir,
                        &session_id,
                        &broadcast_tx,
                        &mut pending,
                    ) {
                        break;
                    }
                    break;
                }
            }
        };

        match event {
            Some(PtyOutputProcessorEvent::Chunk(filtered_data)) => {
                if try_coalesce_pending_output(&mut pending, filtered_data.as_ref()) {
                    trace!(
                        session_id = %session_id,
                        pending_bytes = pending.len(),
                        "coalesced adjacent PTY output chunk"
                    );
                    continue;
                }

                if !flush_pending_output(&runtime, &dir, &session_id, &broadcast_tx, &mut pending) {
                    break;
                }

                if !try_coalesce_pending_output(&mut pending, filtered_data.as_ref())
                    && !flush_filtered_output(
                        &runtime,
                        &dir,
                        &session_id,
                        &broadcast_tx,
                        filtered_data,
                    )
                {
                    break;
                }
            }
            Some(PtyOutputProcessorEvent::Close) => {
                let _ =
                    flush_pending_output(&runtime, &dir, &session_id, &broadcast_tx, &mut pending);
                break;
            }
            None => {
                if !flush_pending_output(&runtime, &dir, &session_id, &broadcast_tx, &mut pending) {
                    break;
                }
            }
        }
    }
}

impl SessionRuntime {
    /// Track terminal modes from the raw PTY stream and advance the visible-output
    /// timestamp without mutating retained filtered output.
    pub fn track_raw_output(
        &mut self,
        raw_data: &Bytes,
        has_visible_output: bool,
    ) -> Option<ModeSnapshot> {
        // Track DEC private mode toggles via byte-level state machine.
        let mode_change = self.mode_tracker.process(raw_data);
        if let Some(ref snap) = mode_change {
            debug!(
                session_id = %self.meta.id,
                app_cursor_keys = snap.app_cursor_keys,
                bracketed_paste_mode = snap.bracketed_paste_mode,
                "terminal mode changed"
            );
        }

        // Advance the silence clock only for chunks with visible content.
        if has_visible_output {
            self.last_output_at = Some(Instant::now());
        }

        mode_change
    }

    /// Add canonical filtered bytes to the retained session stream.
    pub fn retain_filtered_output(&mut self, filtered_data: Bytes) {
        if !filtered_data.is_empty() {
            self.total_bytes = self.total_bytes.saturating_add(filtered_data.len() as u64);
            self.ring.push(filtered_data);
        }
    }

    /// Current terminal mode snapshot (DECCKM, bracketed paste).
    pub fn mode_snapshot(&self) -> ModeSnapshot {
        self.mode_tracker.snapshot()
    }

    /// Push a filtered PTY chunk into the canonical retained stream, update mode
    /// state from the matching raw chunk, and advance the silence clock for
    /// visible content.
    ///
    /// Returns `Some(ModeSnapshot)` if tracked terminal modes changed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn push_output(
        &mut self,
        raw_data: &Bytes,
        filtered_data: Bytes,
        has_visible_output: bool,
    ) -> Option<ModeSnapshot> {
        let mode_change = self.track_raw_output(raw_data, has_visible_output);
        self.retain_filtered_output(filtered_data);
        mode_change
    }

    pub fn register_attach_client(&mut self) {
        self.attach_count = self.attach_count.saturating_add(1);
        trace!(
            session_id = %self.meta.id,
            attach_count = self.attach_count,
            "attach client registered"
        );
        self.last_attach_presence_at = Some(Instant::now());
    }

    pub fn mark_attach_activity(&mut self) {
        self.last_attach_presence_at = Some(Instant::now());
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
        debug!(session_id = %self.meta.id, "attach presence/activity cleared");
        self.attach_count = 0;
        self.last_attach_presence_at = None;
        self.last_attach_activity_at = None;
    }

    pub fn input_needed(&self) -> bool {
        matches!(self.meta.status, SessionStatus::Running)
            && self.notified_output_epoch.is_some()
            && self.notified_output_epoch == self.last_output_at
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
            self.resize_history.push(LogResize {
                offset: self.ring.end_offset(),
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

    append_output(&full_dir, "")?;
    append_event(&full_dir, "session created")?;
    append_resize_event(&full_dir, 0, rows, cols)?;

    let started_pid = meta
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".to_string());
    append_event(&full_dir, &format!("session started pid={started_pid}"))?;

    // Broadcast channel: each live attach subscriber holds a Receiver.
    let (broadcast_tx, _initial_rx) = broadcast::channel::<Arc<Bytes>>(256);

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
        pty_master: Some(master),
    };

    let runtime = Arc::new(Mutex::new(SessionRuntime {
        meta: meta.clone(),
        dir: full_dir,
        ring: RingBuffer::new(config.ring_buffer_bytes),
        total_bytes: 0,
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
        last_output_at: None,
        last_input_at: None,
        last_attach_presence_at: None,
        last_attach_activity_at: None,
        attach_count: 0,
        notified_output_epoch: None,
        last_notified_at: None,
        mode_tracker: ModeTracker::new(),
        output_closed: false,
        notifications_enabled,
    }));

    // PTY output processor thread: waits briefly for short output bursts, then persists
    // and broadcasts the canonical filtered output stream.
    let (output_tx, output_rx) = std_mpsc::channel::<PtyOutputProcessorEvent>();
    let runtime_output = runtime.clone();
    let output_dir = reader_dir.clone();
    let output_session_id = meta.id.clone();
    let output_broadcast_tx = broadcast_tx.clone();
    let output_processor = std::thread::spawn(move || {
        debug!(
            session_id = %output_session_id,
            coalesce_window_ms = PTY_COALESCE_WINDOW.as_millis(),
            coalesce_max_bytes = PTY_COALESCE_MAX_BYTES,
            "PTY output processor thread started"
        );
        run_output_processor(
            runtime_output,
            output_dir,
            output_session_id.clone(),
            output_broadcast_tx,
            output_rx,
        );
        debug!(
            session_id = %output_session_id,
            "PTY output processor thread stopped"
        );
    });

    // PTY reader thread: reads raw bytes, answers shared terminal queries, and
    // forwards canonical filtered output through the coalescing processor.
    let runtime_reader = runtime.clone();
    let reader_session_id = meta.id.clone();
    std::thread::spawn(move || {
        debug!(session_id = %reader_session_id, "PTY reader thread started");
        if let Err(err) = append_event(&reader_dir, "pty reader started") {
            warn!(session_id = %reader_session_id, %err, "failed to persist PTY reader start event");
        }
        let mut buf = [0u8; 4096];
        let mut reader = reader;
        let mut query_tail = Vec::new();
        let mut cursor_tracker = CursorTracker::new(rows, cols);
        let mut stream_filter = EscapeFilter::new();
        let close_event = loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    break "pty reader reached EOF".to_string();
                }
                Ok(n) => {
                    let data = Bytes::copy_from_slice(&buf[..n]);
                    trace!(session_id = %reader_session_id, bytes = n, "read PTY output chunk");

                    // Update cursor position tracking before answering queries
                    // so CPR responses reflect the actual cursor position.
                    cursor_tracker.process(&data);

                    // Always let the daemon answer terminal queries that need
                    // a shared, session-global answer (currently CPR/DSR).
                    for resp in extract_query_responses_no_client(
                        &data,
                        &mut query_tail,
                        cursor_tracker.position(),
                    ) {
                        trace!(
                            session_id = %reader_session_id,
                            bytes = resp.len(),
                            "responding to detached terminal capability query"
                        );
                        if writer_tx.blocking_send(resp).is_err() {
                            warn!(
                                session_id = %reader_session_id,
                                "failed to queue detached terminal query response because PTY writer closed"
                            );
                            break;
                        }
                    }

                    let has_visible_output = has_visible_content(&data);
                    let filtered_data = Bytes::from(stream_filter.filter(&data));

                    // Update mode tracking and visible-output bookkeeping with a
                    // brief lock, while leaving filtered-stream retention to the
                    // output coalescer thread.
                    match runtime_reader.lock() {
                        Ok(mut rt) => {
                            let _mode_change = rt.track_raw_output(&data, has_visible_output);
                            // Sync cursor tracker to current PTY dimensions
                            // so CPR responses reflect the correct size.
                            if let Some((r, c)) = rt.pty_size {
                                cursor_tracker.set_size(r, c);
                            }
                        }
                        Err(_) => {
                            warn!(session_id = %reader_session_id, "failed to lock runtime for PTY output processing");
                            break "failed to lock runtime for PTY output processing".to_string();
                        }
                    }

                    if !filtered_data.is_empty()
                        && output_tx
                            .send(PtyOutputProcessorEvent::Chunk(filtered_data))
                            .is_err()
                    {
                        warn!(
                            session_id = %reader_session_id,
                            "failed to queue PTY output chunk for coalescing"
                        );
                        break "pty output processor closed".to_string();
                    }
                }
                Err(err)
                    if matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    trace!(session_id = %reader_session_id, kind = ?err.kind(), "PTY reader retrying after transient read condition");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    warn!(session_id = %reader_session_id, %err, "PTY reader thread failed");
                    break format!("pty reader error: {err}");
                }
            }
        };

        let _ = output_tx.send(PtyOutputProcessorEvent::Close);
        drop(output_tx);
        if output_processor.join().is_err() {
            warn!(
                session_id = %reader_session_id,
                "PTY output processor thread panicked"
            );
        }
        if let Ok(mut rt) = runtime_reader.lock() {
            rt.output_closed = true;
        }
        debug!(session_id = %reader_session_id, event = %close_event, "PTY reader thread finished");
        if let Err(err) = append_event(&reader_dir, &close_event) {
            warn!(
                session_id = %reader_session_id,
                %err,
                "failed to persist PTY reader completion event"
            );
        }
        debug!(session_id = %reader_session_id, "PTY reader thread stopped");
    });

    info!(
        session_id = %meta.id,
        pid = ?meta.pid,
        ring_buffer_bytes = config.ring_buffer_bytes,
        writer_queue_capacity = PTY_WRITER_QUEUE_CAPACITY,
        "PTY session runtime spawned"
    );
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
            ring: RingBuffer::new(4096), // small capacity for tests
            total_bytes: 0,
            broadcast_tx,
            resize_tx,
            pty: PtyHandle {
                child: make_test_child_with_exit_code(exit_code),
                writer_tx,
                pty_master: None,
            },
            pty_size: None,
            resize_history: Vec::new(),
            completed_at: None,
            persisted: false,
            requested_final_status: None,
            last_output_at: None,
            last_input_at: None,
            last_attach_presence_at: None,
            last_attach_activity_at: None,
            attach_count: 0,
            last_notified_at: None,
            notified_output_epoch: None,
            mode_tracker: ModeTracker::new(),
            output_closed: false,
            notifications_enabled: true,
        }
    }

    fn new_runtime() -> SessionRuntime {
        new_runtime_with(SessionStatus::Running, 0)
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
        rt.push_output(
            &bytes::Bytes::from("text \x1b[?2004h more"),
            bytes::Bytes::from("text \x1b[?2004h more"),
            true,
        );
        assert!(
            rt.mode_snapshot().bracketed_paste_mode,
            "bracketed_paste_mode should be set after \\x1b[?2004h"
        );
    }

    #[test]
    fn test_push_output_disables_bracketed_paste() {
        let mut rt = new_runtime();
        rt.push_output(
            &bytes::Bytes::from("\x1b[?2004h"),
            bytes::Bytes::from("\x1b[?2004h"),
            false,
        );
        assert!(rt.mode_snapshot().bracketed_paste_mode);
        rt.push_output(
            &bytes::Bytes::from("\x1b[?2004l"),
            bytes::Bytes::from("\x1b[?2004l"),
            false,
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
        rt.push_output(
            &bytes::Bytes::from("\x1b[?1h"),
            bytes::Bytes::from("\x1b[?1h"),
            false,
        );
        assert!(
            rt.mode_snapshot().app_cursor_keys,
            "app_cursor_keys should be set after DECCKM enable"
        );
    }

    #[test]
    fn test_push_output_disables_app_cursor_keys() {
        let mut rt = new_runtime();
        rt.push_output(
            &bytes::Bytes::from("\x1b[?1h"),
            bytes::Bytes::from("\x1b[?1h"),
            false,
        );
        assert!(rt.mode_snapshot().app_cursor_keys);
        rt.push_output(
            &bytes::Bytes::from("\x1b[?1l"),
            bytes::Bytes::from("\x1b[?1l"),
            false,
        );
        assert!(
            !rt.mode_snapshot().app_cursor_keys,
            "app_cursor_keys should be cleared after DECCKM disable"
        );
    }

    // -----------------------------------------------------------------------
    // push_output — ring buffer eviction
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_output_ring_evicts_oldest_when_over_capacity() {
        let mut rt = new_runtime(); // capacity = 4096 bytes
        // Push chunks that together exceed 4096 bytes so oldest are evicted.
        let chunk = bytes::Bytes::from(vec![b'x'; 1500]);
        rt.push_output(&chunk, chunk.clone(), true);
        rt.push_output(&chunk, chunk.clone(), true);
        rt.push_output(&chunk, chunk.clone(), true); // total so far: 4500 — first evicted
        // start_offset must have advanced past 0
        assert!(
            rt.ring.start_offset() > 0,
            "oldest chunks should be evicted once capacity is exceeded"
        );
    }

    // -----------------------------------------------------------------------
    // push_output — last_output_at tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_output_visible_content_advances_last_output_at() {
        let mut rt = new_runtime();
        assert!(rt.last_output_at.is_none());
        rt.push_output(
            &bytes::Bytes::from("hello world\n"),
            bytes::Bytes::from("hello world\n"),
            true,
        );
        assert!(
            rt.last_output_at.is_some(),
            "visible output should set last_output_at"
        );
    }

    #[test]
    fn test_push_output_pure_ansi_does_not_advance_last_output_at() {
        let mut rt = new_runtime();
        rt.push_output(
            &bytes::Bytes::from("\x1b[1A\x1b[2K\x1b[H"),
            bytes::Bytes::from("\x1b[1A\x1b[2K\x1b[H"),
            false,
        );
        assert!(
            rt.last_output_at.is_none(),
            "pure ANSI sequences should not advance last_output_at"
        );
    }

    #[test]
    fn test_push_output_uses_filtered_bytes_for_ring_and_offsets() {
        let mut rt = new_runtime();
        let raw = bytes::Bytes::from_static(b"before\x1b[6nafter");
        let filtered = bytes::Bytes::from_static(b"beforeafter");

        rt.push_output(&raw, filtered, true);

        let (chunks, end_offset) = rt.ring.read_from(0);
        let combined: Vec<u8> = chunks.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(combined, b"beforeafter");
        assert_eq!(end_offset, 11);
    }

    #[test]
    fn test_push_output_tracks_total_bytes_from_filtered_stream() {
        let mut rt = new_runtime();
        let raw = bytes::Bytes::from_static(b"before\x1b[6nafter");
        let filtered = bytes::Bytes::from_static(b"beforeafter");

        rt.push_output(&raw, filtered, true);

        assert_eq!(rt.total_bytes, 11);
    }

    #[test]
    fn test_push_output_drops_fully_stripped_chunks_from_ring() {
        let mut rt = new_runtime();
        let raw = bytes::Bytes::from_static(b"\x1b[6n");

        rt.push_output(&raw, bytes::Bytes::new(), false);

        let (chunks, end_offset) = rt.ring.read_from(0);
        assert!(chunks.is_empty());
        assert_eq!(end_offset, 0);
        assert_eq!(rt.total_bytes, 0);
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
            ring: RingBuffer::new(4096),
            total_bytes: 0,
            broadcast_tx,
            resize_tx,
            pty: PtyHandle {
                child: make_test_child_with_exit_code(0),
                writer_tx,
                pty_master: None,
            },
            pty_size: None,
            resize_history: Vec::new(),
            completed_at: None,
            persisted: false,
            requested_final_status: None,
            last_output_at: None,
            last_input_at: None,
            last_attach_presence_at: None,
            last_attach_activity_at: None,
            attach_count: 0,
            last_notified_at: None,
            notified_output_epoch: None,
            mode_tracker: ModeTracker::new(),
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
