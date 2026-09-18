//! The attach path: subscribing a client, forwarding input and resizes.
//!
//! This is the latency-sensitive surface. Lock scopes are deliberately narrow
//! and the input path never holds a write lock across an await.

use std::{sync::Arc, time::Instant};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc::error::TrySendError};
use tracing::{debug, warn};

use crate::session::{SessionEvent, runtime::SequencedChunk};

use super::super::{
    SessionError, journal,
    persist::{append_resize_event, read_output_from},
    replay,
};
use super::{
    ATTACH_INPUT_OUTPUT_POLL_INTERVAL, ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT, SessionHandle,
    SessionStore,
};

impl SessionStore {
    pub async fn attach_stream_status(
        &self,
        id: &str,
    ) -> std::result::Result<(bool, bool, Option<i32>), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let rt = handle.read();
        Ok((!rt.is_completed(), rt.output_closed, rt.meta.exit_code))
    }

    /// Register an identified attachment and grant control per the registry
    /// policy (M3-4, PLAN §8.1). When control is granted and a viewport is
    /// declared, the initial geometry is applied through the session
    /// sequencer before any snapshot is taken.
    pub async fn attach_register(
        &self,
        id: &str,
        kind: crate::session::registry::AttachKind,
        request: crate::session::registry::ControlRequest,
        viewport: Option<(u16, u16)>,
    ) -> std::result::Result<AttachRegistration, SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let (attachment_id, outcome, resized) = {
            let mut rt = handle.write();
            rt.register_attachment(kind, request, viewport)
        };
        if resized {
            let rt = handle.read();
            let offset = rt.filtered_stream_len();
            if let Some((rows, cols)) = viewport {
                let _ = super::super::persist::append_resize_event(&rt.dir, offset, rows, cols);
            }
        }
        debug!(
            session_id = id,
            attachment_id,
            role = outcome.role.as_str(),
            "attach client registered"
        );
        Ok(AttachRegistration {
            attachment_id,
            role: outcome.role,
        })
    }

    /// Explicit control takeover by an attached observer.
    pub async fn attach_acquire_control(
        &self,
        id: &str,
        attachment_id: u64,
    ) -> std::result::Result<crate::session::registry::ControlOutcome, SessionError> {
        let handle = self.lookup_runtime(id).await?;
        handle
            .write()
            .acquire_control(attachment_id)
            .ok_or(SessionError::StaleAttachment)
    }

    /// Subscribe to control handoffs for a session: every send carries the
    /// current controller's attachment id (`None` = lease free).
    pub fn subscribe_control(
        &self,
        id: &str,
    ) -> Option<tokio::sync::broadcast::Receiver<ControlNotice>> {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .map(|handle| handle.read().control_tx.subscribe())
    }

    /// Initialise a streaming subscription: return persisted canonical output
    /// since `from_byte_offset` (or all content if `None`), the current
    /// filtered-stream end offset, a live broadcast receiver, and the current
    /// terminal mode flags.
    pub async fn attach_subscribe_init(
        &self,
        id: &str,
        from_byte_offset: Option<u64>,
    ) -> std::result::Result<
        (
            Vec<(u64, Bytes)>,
            u64,
            broadcast::Receiver<SequencedChunk>,
            bool,
            bool,
        ),
        SessionError,
    > {
        let handle = self.lookup_runtime(id).await?;
        let (dir, rx, modes) = {
            let rt = handle.read();
            (
                rt.dir.clone(),
                rt.broadcast_tx.subscribe(),
                rt.mode_snapshot(),
            )
        };
        let offset = from_byte_offset.unwrap_or(0);
        // M3-1b: the filtered display stream is derived from the raw
        // journal; `output.log` is only a legacy fallback for sessions
        // started before the journal became always-on (removed in M6).
        let (data, end_offset) = if dir.join(journal::JOURNAL_DIR_NAME).is_dir() {
            replay::filtered_stream_from(&dir, offset).map_err(|err| {
                warn!(session_id = id, %err, "failed to derive attach output from the journal");
                SessionError::Evicted
            })?
        } else {
            read_output_from(&dir, offset).map_err(|err| {
                warn!(session_id = id, %err, "failed to read persisted attach output");
                SessionError::Evicted
            })?
        };
        let chunks = if data.is_empty() {
            Vec::new()
        } else {
            vec![(offset, Bytes::from(data))]
        };
        debug!(
            session_id = id,
            chunks = chunks.len(),
            end_offset,
            bracketed_paste_mode = modes.bracketed_paste_mode,
            app_cursor_keys = modes.app_cursor_keys,
            "attach subscribe init"
        );
        Ok((
            chunks,
            end_offset,
            rx,
            modes.bracketed_paste_mode,
            modes.app_cursor_keys,
        ))
    }

    /// Initialise an attach stream from the current rendered terminal state
    /// instead of replaying persisted PTY history from byte offset 0.
    /// Read one bounded window of the persisted filtered stream starting at
    /// `from` (I7): at most `max_bytes`, so a lagged attachment resyncs in
    /// slices instead of one unbounded allocation.
    pub async fn attach_resync_window(
        &self,
        id: &str,
        from: u64,
        max_bytes: usize,
    ) -> std::result::Result<Vec<u8>, SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let dir = { handle.read().dir.clone() };
        // Same journal/legacy split as attach_subscribe_init (removed in M6).
        let result: Result<Vec<u8>, String> =
            if dir.join(crate::session::journal::JOURNAL_DIR_NAME).is_dir() {
                crate::session::replay::filtered_stream_window(&dir, from, max_bytes)
                    .map_err(|err| err.to_string())
            } else {
                crate::session::persist::read_output_window(&dir, from, max_bytes)
                    .map_err(|err| err.to_string())
            };
        result.map_err(|err| {
            warn!(session_id = id, %err, "attach resync window read failed");
            SessionError::Evicted
        })
    }

    /// Record a client's applied-cursor credit (M3-5, I7). Stale attachment
    /// tokens and non-advancing cursors are ignored: credits are
    /// best-effort backpressure signals, not correctness gates.
    pub async fn attach_report_applied(&self, id: &str, attachment_id: u64, cursor: u64) {
        if let Ok(handle) = self.lookup_runtime(id).await {
            let mut rt = handle.write();
            rt.attachments.report_applied(attachment_id, cursor);
        }
    }

    /// Slowest applied cursor across registered attachments that have
    /// reported (backpressure signal; `None` when none have).
    #[cfg(test)]
    pub async fn attach_min_applied_cursor(&self, id: &str) -> Option<u64> {
        let handle = self.lookup_runtime(id).await.ok()?;
        let rt = handle.read();
        rt.attachments
            .attachments()
            .map(|a| a.applied_cursor)
            .filter(|&cursor| cursor > 0)
            .min()
    }

    /// In-memory filtered-stream length (M3-5): counts bytes the journal
    /// appender may not have flushed yet, so the pump's completion drain
    /// knows when the persisted tail has caught up.
    pub async fn attach_filtered_len(&self, id: &str) -> Option<u64> {
        let handle = self.lookup_runtime(id).await.ok()?;
        Some(handle.read().filtered_stream_len())
    }

    pub async fn attach_snapshot_init(
        &self,
        id: &str,
    ) -> std::result::Result<
        (
            Vec<u8>,
            u64,
            broadcast::Receiver<SequencedChunk>,
            bool,
            bool,
        ),
        SessionError,
    > {
        let handle = self.lookup_runtime(id).await?;
        let rt = handle.read();
        let snapshot = rt.attach_snapshot_bytes();
        let end_offset = rt.filtered_stream_len();
        let rx = rt.broadcast_tx.subscribe();
        let modes = rt.mode_snapshot();
        debug!(
            session_id = id,
            snapshot_bytes = snapshot.len(),
            end_offset,
            bracketed_paste_mode = modes.bracketed_paste_mode,
            app_cursor_keys = modes.app_cursor_keys,
            "attach snapshot init"
        );
        Ok((
            snapshot,
            end_offset,
            rx,
            modes.bracketed_paste_mode,
            modes.app_cursor_keys,
        ))
    }

    /// Render the session's scrolled-off rows for the client to print before
    /// the screen snapshot, so the terminal scrollbar covers pre-attach
    /// history instead of starting empty.  The seed depth is a fixed floor
    /// (see [`crate::config::DEFAULT_ATTACH_SCROLLBACK_SEED_ROWS`]), not the
    /// attaching client's screen height: reattaching to a session with a long
    /// transcript must not feel like the history was cut to one screenful.
    /// The visible screen is deliberately excluded: the snapshot already
    /// covers it, and seeding it would duplicate content.
    ///
    /// Returns `None` when there is nothing worth seeding: the session is in
    /// the alternate screen (rows that scrolled off there are not linear
    /// history) or nothing has scrolled off yet.
    pub async fn attach_scrollback_seed(&self, id: &str, rows: u16) -> Option<Vec<u8>> {
        let handle = self.lookup_runtime(id).await.ok()?;
        let rt = handle.read();
        if rt.engine.modes().alt_screen {
            return None;
        }
        // At least `DEFAULT_ATTACH_SCROLLBACK_SEED_ROWS` rows when available,
        // never more than the session retained in the first place.
        let depth = usize::from(rows)
            .max(crate::config::DEFAULT_ATTACH_SCROLLBACK_SEED_ROWS)
            .min(rt.screen_scrollback_rows);
        crate::session::logs::format_history_rows(rt.engine.styled_history_rows(depth))
    }

    /// Subscribe to resize notifications for a session.
    /// Returns a broadcast receiver for (rows, cols) events and the current PTY size.
    pub fn subscribe_resize(
        &self,
        id: &str,
    ) -> Option<(broadcast::Receiver<(u16, u16)>, Option<(u16, u16)>)> {
        let sessions = self.sessions.load();
        let handle = sessions.get(id)?;
        let rt = handle.read();
        Some((rt.resize_tx.subscribe(), rt.pty_size))
    }

    pub async fn attach_detach(
        &self,
        id: &str,
        attachment_id: u64,
    ) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        handle.write().unregister_attachment(attachment_id);
        debug!(session_id = id, attachment_id, "attach detach acknowledged");
        Ok(())
    }

    pub async fn attach_input(
        &self,
        id: &str,
        attachment_id: Option<u64>,
        data: &str,
        wait_for_change: bool,
    ) -> std::result::Result<(), SessionError> {
        // Avoid sending lose focus escape sequence which will cause other clients not able to input anything
        if data == "\x1b[O" {
            return Ok(());
        }

        let handle = self.lookup_runtime(id).await?;

        // I6: attached clients drive input only while holding the control
        // lease. `None` is the operator control plane (`oly send`, HTTP
        // input), which is not an attachment and stays ungated.
        if let Some(attachment_id) = attachment_id {
            let rt = handle.read();
            check_control(&rt, attachment_id)?;
        }

        // Read lock: gather mode flags, transform input, send to PTY channel.
        // try_write_input() is a non-blocking channel send that only needs &self.
        let (initial_total_bytes, byte_len, transformed, app_cursor_keys) = {
            let rt = handle.read();
            let initial_total_bytes = rt.filtered_total_bytes;
            let modes = rt.mode_snapshot();
            let cooked;
            let transformed = modes.app_cursor_keys
                && (data.contains("\x1b[A")
                    || data.contains("\x1b[B")
                    || data.contains("\x1b[C")
                    || data.contains("\x1b[D"));
            let bytes = if transformed {
                cooked = data
                    .replace("\x1b[A", "\x1bOA")
                    .replace("\x1b[B", "\x1bOB")
                    .replace("\x1b[C", "\x1bOC")
                    .replace("\x1b[D", "\x1bOD");
                cooked.into_bytes()
            } else {
                data.as_bytes().to_vec()
            };

            let byte_len = bytes.len();
            match rt.pty.try_write_input(bytes) {
                Ok(()) => Ok((
                    initial_total_bytes,
                    byte_len,
                    transformed,
                    modes.app_cursor_keys,
                )),
                Err(TrySendError::Full(_)) => {
                    debug!(
                        session_id = id,
                        bytes = byte_len,
                        "attach input backpressured by full PTY writer queue"
                    );
                    Err(SessionError::Busy)
                }
                Err(TrySendError::Closed(_)) => {
                    debug!(
                        session_id = id,
                        bytes = byte_len,
                        "attach input failed while writing to PTY"
                    );
                    Err(SessionError::Evicted)
                }
            }
        }?;

        // Brief write lock: only touch the two timestamp fields.
        {
            let mut rt = handle.write();
            rt.mark_attach_activity();
            rt.last_input_at = Some(Instant::now());
        }

        debug!(
            session_id = id,
            bytes = byte_len,
            transformed,
            app_cursor_keys,
            "attach input forwarded"
        );

        if wait_for_change {
            let _ = self
                .wait_for_output_change(id, &handle, initial_total_bytes)
                .await;
        }

        Ok(())
    }

    pub async fn attach_busy(&self, id: &str) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let summary = {
            let mut rt = handle.write();
            rt.mark_attach_activity();
            rt.last_output_epoch = Some(Instant::now());
            rt.to_summary()
        };

        let _ = self.event_tx.send(SessionEvent::SessionUpdated(summary));
        debug!(session_id = id, "attach busy heartbeat recorded");
        Ok(())
    }

    async fn wait_for_output_change(
        &self,
        id: &str,
        handle: &Arc<SessionHandle>,
        initial_total_bytes: u64,
    ) -> bool {
        let started = Instant::now();
        loop {
            let current_total_bytes = handle.read().filtered_total_bytes;

            if current_total_bytes != initial_total_bytes {
                debug!(
                    session_id = id,
                    initial_total_bytes,
                    current_total_bytes,
                    waited_ms = started.elapsed().as_millis(),
                    "attach input observed output change"
                );
                return true;
            }

            if started.elapsed() >= ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT {
                debug!(
                    session_id = id,
                    last_total_bytes = initial_total_bytes,
                    waited_ms = started.elapsed().as_millis(),
                    "attach input timed out waiting for output change"
                );
                return false;
            }

            tokio::time::sleep(ATTACH_INPUT_OUTPUT_POLL_INTERVAL).await;
        }
    }

    pub async fn attach_resize(
        &self,
        id: &str,
        attachment_id: Option<u64>,
        rows: u16,
        cols: u16,
    ) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let resized = {
            let mut rt = handle.write();
            // I6: observers never resize the PTY; their declared size is a
            // viewport, recorded for status surfaces only.
            if let Some(attachment_id) = attachment_id {
                check_control(&rt, attachment_id)?;
                rt.attachments.set_viewport(attachment_id, rows, cols);
            }
            rt.mark_attach_activity();
            rt.resize_pty(rows, cols)
        };

        debug!(
            session_id = id,
            rows, cols, resized, "attach resize requested"
        );
        if resized {
            let rt = handle.read();
            let offset = rt.filtered_stream_len();
            let _ = append_resize_event(&rt.dir, offset, rows, cols);
            Ok(())
        } else {
            Err(SessionError::Evicted)
        }
    }
}

/// The result of registering an attachment (M3-4).
#[derive(Debug, Clone, Copy)]
pub struct AttachRegistration {
    /// Fencing token identifying this attachment for its lifetime.
    pub attachment_id: u64,
    /// The role actually granted (a controller request may join as observer).
    pub role: crate::session::registry::AttachRole,
}

/// Control-handoff notice element: the current controller's attachment id
/// (`None` = lease free).
pub type ControlNotice = Option<u64>;

/// Geometry/input gate for attached clients: only the controller drives.
fn check_control(
    rt: &super::super::runtime::SessionRuntime,
    attachment_id: u64,
) -> std::result::Result<(), SessionError> {
    if !rt.attachments.contains(attachment_id) {
        return Err(SessionError::StaleAttachment);
    }
    if rt.attachments.is_controller(attachment_id) {
        Ok(())
    } else {
        Err(SessionError::NotController)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport::*;
    use super::*;
    use crate::session::{SessionStatus, pty::collect_chunk_bytes};

    use std::time::{Duration, Instant};
    #[tokio::test]
    async fn test_attach_subscribe_init_reads_persisted_output_from_offset() {
        let runtime = make_runtime(
            "attach123",
            SessionStatus::Running,
            "hello world",
            Some(Duration::from_secs(1)),
        );
        let store = store_with(vec![runtime], make_test_db().await);

        let (chunks, end_offset, _rx, _bpm, _ack) = store
            .attach_subscribe_init("attach123", Some(6))
            .await
            .expect("attach subscribe init");

        assert_eq!(end_offset, 11);
        assert_eq!(collect_chunk_bytes(&chunks), b"world");
    }

    // -----------------------------------------------------------------------
    // mark_notified + output-epoch gating
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_input_writes_data_to_writer() {
        let (rt, mut writer_rx) = make_runtime_writable("inp0001", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0001", None, "hello\r", true)
            .await
            .expect("attach_input should succeed");

        let written = writer_rx.recv().await.expect("should receive bytes");
        assert_eq!(
            written, b"hello\r",
            "expected exact bytes sent via writer_tx"
        );
    }

    #[tokio::test]
    async fn test_attach_input_sets_last_input_at() {
        let (rt, _writer_rx) = make_runtime_writable("inp0002", SessionStatus::Running);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0002", None, "x", true)
            .await
            .expect("attach_input should succeed");

        let locked = rt_clone.read();
        assert!(
            locked.last_input_at.is_some(),
            "last_input_at should be set after input"
        );
    }

    #[tokio::test]
    async fn test_attach_input_decckm_transforms_arrow_up() {
        // When app_cursor_keys = true, \x1b[A → \x1bOA (DECCKM mode).
        let (rt, mut writer_rx) = make_runtime_writable("inp0003", SessionStatus::Running);
        {
            let mut locked = rt.write();
            locked.feed_engine(b"\x1b[?1h");
        }
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0003", None, "\x1b[A", true)
            .await
            .expect("attach_input should succeed");

        let written = writer_rx.recv().await.expect("should receive bytes");
        assert_eq!(
            written, b"\x1bOA",
            "arrow up should be translated to app-cursor-key form"
        );
    }

    #[tokio::test]
    async fn test_attach_input_decckm_transforms_all_arrows() {
        let (rt, mut writer_rx) = make_runtime_writable("inp0004", SessionStatus::Running);
        {
            let mut locked = rt.write();
            locked.feed_engine(b"\x1b[?1h");
        }
        let store = store_with(vec![rt], make_test_db().await);

        // Send all four arrow sequences at once.
        store
            .attach_input("inp0004", None, "\x1b[A\x1b[B\x1b[C\x1b[D", true)
            .await
            .expect("attach_input should succeed");

        let written = writer_rx.recv().await.expect("should receive bytes");
        assert_eq!(
            written, b"\x1bOA\x1bOB\x1bOC\x1bOD",
            "all arrow sequences should be translated in DECCKM mode"
        );
    }

    #[tokio::test]
    async fn test_attach_input_no_transform_when_decckm_off() {
        let (rt, mut writer_rx) = make_runtime_writable("inp0005", SessionStatus::Running);
        // app_cursor_keys is false by default.
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0005", None, "\x1b[A\x1b[B", true)
            .await
            .expect("attach_input should succeed");

        let written = writer_rx.recv().await.expect("should receive bytes");
        assert_eq!(
            written, b"\x1b[A\x1b[B",
            "arrow sequences should pass through unchanged when DECCKM is off"
        );
    }

    #[tokio::test]
    async fn test_attach_input_not_found_for_unknown_session() {
        let store = SessionStore::new(900, make_test_db().await);
        let result = store.attach_input("no_such_id", None, "data", true).await;
        assert!(
            result.is_err(),
            "attach_input to unknown session should return an error"
        );
    }

    #[tokio::test]
    async fn test_attach_input_returns_busy_when_writer_queue_is_full() {
        let (rt, _writer_rx) =
            make_runtime_writable_with_capacity("inpbusy1", SessionStatus::Running, 1);
        {
            let locked = rt.read();
            locked
                .pty
                .try_write_input(b"first".to_vec())
                .expect("first write should fit in the bounded queue");
        }
        let store = store_with(vec![rt], make_test_db().await);

        let result = store.attach_input("inpbusy1", None, "second", true).await;
        assert!(
            matches!(result, Err(SessionError::Busy)),
            "expected bounded writer queue saturation to surface SessionLookupError::Busy"
        );
    }

    #[tokio::test]
    async fn test_attach_input_returns_early_when_output_changes() {
        let (rt, _writer_rx) = make_runtime_writable("inpwait1", SessionStatus::Running);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        let updater = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let mut locked = rt_clone.write();
            locked.filtered_total_bytes += 1;
            locked.last_total_bytes += 1;
            locked.last_output_epoch = Some(Instant::now());
        });

        let started = Instant::now();
        store
            .attach_input("inpwait1", None, "x", true)
            .await
            .expect("attach_input should succeed");
        updater.await.expect("output updater should complete");

        assert!(
            started.elapsed() < ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT,
            "attach_input should return before the timeout once output advances"
        );
    }

    #[tokio::test]
    async fn test_attach_input_waits_for_timeout_without_output_change() {
        let (rt, _writer_rx) = make_runtime_writable("inpwait2", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        let started = Instant::now();
        store
            .attach_input("inpwait2", None, "x", true)
            .await
            .expect("attach_input should succeed");

        assert!(
            started.elapsed() >= ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT,
            "attach_input should wait through the timeout when output does not advance"
        );
    }

    #[tokio::test]
    async fn test_attach_busy_advances_output_epoch_and_bytes() {
        let (rt, _writer_rx) = make_runtime_writable("busy0001", SessionStatus::Running);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_busy("busy0001")
            .await
            .expect("attach_busy should succeed");

        let locked = rt_clone.read();
        assert_eq!(
            locked.last_total_bytes, 0,
            "attach_busy should advance the session byte counter"
        );
        assert!(
            locked.last_output_epoch.is_some(),
            "attach_busy should stamp a fresh output epoch"
        );
        assert!(
            locked.last_attach_activity_at.is_some(),
            "attach_busy should count as interactive attach activity"
        );
    }

    #[tokio::test]
    async fn test_attach_detach_clears_presence_but_keeps_activity() {
        let rt = make_runtime("detach001", SessionStatus::Running, "$ prompt", None);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        let reg1 = store
            .attach_register(
                "detach001",
                crate::session::registry::AttachKind::Cli,
                crate::session::registry::ControlRequest::Controller,
                None,
            )
            .await
            .expect("register should succeed");
        {
            let mut locked = rt_clone.write();
            locked.mark_attach_activity();
        }

        store
            .attach_detach("detach001", reg1.attachment_id)
            .await
            .expect("detach should succeed");

        let locked = rt_clone.read();
        assert!(
            locked.last_attach_activity_at.is_some(),
            "detach must keep the activity timestamp: it records when a user last saw the session"
        );
    }

    #[tokio::test]
    async fn test_attach_detach_only_clears_after_final_client_disconnects() {
        let rt = make_runtime("detach002", SessionStatus::Running, "$ prompt", None);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        let reg1 = store
            .attach_register(
                "detach002",
                crate::session::registry::AttachKind::Cli,
                crate::session::registry::ControlRequest::Controller,
                None,
            )
            .await
            .expect("first register should succeed");
        let reg2 = store
            .attach_register(
                "detach002",
                crate::session::registry::AttachKind::Cli,
                crate::session::registry::ControlRequest::Controller,
                None,
            )
            .await
            .expect("second register should succeed");
        {
            let mut locked = rt_clone.write();
            locked.mark_attach_activity();
        }

        store
            .attach_detach("detach002", reg1.attachment_id)
            .await
            .expect("first detach should succeed");

        {
            let locked = rt_clone.read();
            assert_eq!(
                locked.attachments.len(),
                1,
                "one client should still remain registered"
            );
            assert!(
                locked.last_attach_activity_at.is_some(),
                "activity timestamp should remain until the last client disconnects"
            );
        }

        store
            .attach_detach("detach002", reg2.attachment_id)
            .await
            .expect("second detach should succeed");

        let locked = rt_clone.read();
        assert_eq!(
            locked.attachments.len(),
            0,
            "all clients should be disconnected"
        );
        assert!(
            locked.last_attach_activity_at.is_some(),
            "final detach should still keep the last-seen activity timestamp"
        );
    }

    #[tokio::test]
    async fn attach_applied_cursor_credits_register_per_attachment() {
        use crate::session::registry::{AttachKind, ControlRequest};

        let (rt, _writer_rx) = make_runtime_writable("ack0001", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        let reg_a = store
            .attach_register("ack0001", AttachKind::Cli, ControlRequest::Controller, None)
            .await
            .expect("attach A");
        let reg_b = store
            .attach_register("ack0001", AttachKind::Web, ControlRequest::Observer, None)
            .await
            .expect("attach B");

        // Credits advance monotonically per attachment (M3-5, I7).
        store
            .attach_report_applied("ack0001", reg_a.attachment_id, 4096)
            .await;
        store
            .attach_report_applied("ack0001", reg_b.attachment_id, 1024)
            .await;
        store
            .attach_report_applied("ack0001", reg_a.attachment_id, 128)
            .await; // non-advancing: ignored
        assert_eq!(
            store.attach_min_applied_cursor("ack0001").await,
            Some(1024),
            "slowest attachment bounds the applied cursor"
        );
        // A stale token (detached attachment) is ignored, not an error.
        let _ = store.attach_detach("ack0001", reg_b.attachment_id).await;
        store
            .attach_report_applied("ack0001", reg_b.attachment_id, 8192)
            .await;
        assert_eq!(store.attach_min_applied_cursor("ack0001").await, Some(4096));
    }

    #[tokio::test]
    async fn attach_control_lease_gates_input_and_resize() {
        use crate::session::SessionError;
        use crate::session::registry::{AttachKind, ControlRequest};

        let (rt, _writer_rx) = make_runtime_writable("ctl0001", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        // First controller takes the lease.
        let first = store
            .attach_register("ctl0001", AttachKind::Cli, ControlRequest::Controller, None)
            .await
            .expect("first attach");
        assert_eq!(first.role, crate::session::registry::AttachRole::Controller);

        // A second controller request joins as observer.
        let second = store
            .attach_register("ctl0001", AttachKind::Web, ControlRequest::Controller, None)
            .await
            .expect("second attach");
        assert_eq!(second.role, crate::session::registry::AttachRole::Observer);

        // Observer input and resize are rejected with NotController.
        let err = store
            .attach_input("ctl0001", Some(second.attachment_id), "x", false)
            .await
            .expect_err("observer input must be gated");
        assert!(matches!(err, SessionError::NotController));
        let err = store
            .attach_resize("ctl0001", Some(second.attachment_id), 24, 80)
            .await
            .expect_err("observer resize must be gated");
        assert!(matches!(err, SessionError::NotController));

        // The operator control plane (no attachment) stays ungated.
        store
            .attach_input("ctl0001", None, "ls", false)
            .await
            .expect("operator input must not be gated");

        // Takeover flips the lease: second drives, first is rejected.
        let outcome = store
            .attach_acquire_control("ctl0001", second.attachment_id)
            .await
            .expect("takeover should succeed");
        assert_eq!(
            outcome.role,
            crate::session::registry::AttachRole::Controller
        );
        assert_eq!(outcome.demoted, Some(first.attachment_id));
        store
            .attach_input("ctl0001", Some(second.attachment_id), "x", false)
            .await
            .expect("new controller input");
        let err = store
            .attach_input("ctl0001", Some(first.attachment_id), "x", false)
            .await
            .expect_err("demoted controller input must be gated");
        assert!(matches!(err, SessionError::NotController));

        // Detaching the controller frees the lease; a stale token is a
        // no-op, and further control ops on it fail precisely.
        store
            .attach_detach("ctl0001", second.attachment_id)
            .await
            .expect("detach controller");
        let err = store
            .attach_input("ctl0001", Some(second.attachment_id), "x", false)
            .await
            .expect_err("stale attachment must fail precisely");
        assert!(matches!(err, SessionError::StaleAttachment));
        // The demoted first attachment stays an observer (no implicit
        // promotion), but can take the now-free lease explicitly.
        let err = store
            .attach_input("ctl0001", Some(first.attachment_id), "x", false)
            .await
            .expect_err("demoted attachment stays observer after lease frees");
        assert!(matches!(err, SessionError::NotController));
        let outcome = store
            .attach_acquire_control("ctl0001", first.attachment_id)
            .await
            .expect("takeover of the free lease should succeed");
        assert_eq!(
            outcome.role,
            crate::session::registry::AttachRole::Controller
        );
        assert_eq!(outcome.demoted, None);
        store
            .attach_input("ctl0001", Some(first.attachment_id), "x", false)
            .await
            .expect("re-acquired controller input");
    }

    #[tokio::test]
    async fn test_attach_stream_status_keeps_stopping_session_live() {
        let rt = make_runtime("stoplive", SessionStatus::Stopping, "", None);
        let store = store_with(vec![rt], make_test_db().await);

        let (running, output_closed, exit_code) = store
            .attach_stream_status("stoplive")
            .await
            .expect("status lookup should succeed");

        assert!(
            running,
            "stopping sessions should remain streamable until exit"
        );
        assert!(
            !output_closed,
            "fresh test runtime should still have open output"
        );
        assert_eq!(exit_code, None);
    }

    #[tokio::test]
    async fn attach_scrollback_seed_renders_scrolled_off_rows_only() {
        // 30 lines on a 24-row screen: only the first rows live in the
        // parser's retained scrollback; the rest are still visible.
        let mut excerpt = String::new();
        for i in 1..=30 {
            excerpt.push_str(&format!("history line {i:02}\r\n"));
        }
        let rt = make_runtime("seedhist", SessionStatus::Running, &excerpt, None);
        let store = store_with(vec![rt], make_test_db().await);

        let seed = store
            .attach_scrollback_seed("seedhist", 24)
            .await
            .expect("session with scrollback should render a seed");
        let text = String::from_utf8_lossy(&seed);
        assert!(text.contains("history line 01"));
        assert!(text.contains("history line 07"));
        // The visible screen is covered by the attach snapshot, not the seed.
        assert!(!text.contains("history line 08"));
        assert!(!text.contains("history line 30"));
    }

    #[tokio::test]
    async fn attach_scrollback_seed_reaches_at_least_a_thousand_rows() {
        // 1200 lines on a 24-row screen: the trailing line feeds scroll lines
        // 1..=1177 off the visible screen. The seed must cover 1000 of them
        // — far beyond the attaching terminal's own screen height — so a
        // reattach keeps deep history instead of a single screenful.
        let mut excerpt = String::new();
        for i in 1..=1200 {
            excerpt.push_str(&format!("history line {i:04}\r\n"));
        }
        let rt = make_runtime("seeddeep", SessionStatus::Running, &excerpt, None);
        let store = store_with(vec![rt], make_test_db().await);

        let seed = store
            .attach_scrollback_seed("seeddeep", 24)
            .await
            .expect("session with deep scrollback should render a seed");
        let text = String::from_utf8_lossy(&seed);
        // Depth floor: rows past the newest 1000 scrollback rows are dropped,
        // the newest 1000 are all kept (scrollback holds lines 1..=1177).
        assert!(text.contains("history line 0178"));
        assert!(text.contains("history line 0200"));
        assert!(text.contains("history line 1177"));
        assert!(!text.contains("history line 0177"));
        // The visible screen (lines 1178..=1200) is covered by the snapshot.
        assert!(!text.contains("history line 1178"));
    }

    #[tokio::test]
    async fn attach_scrollback_seed_is_none_when_nothing_scrolled_off() {
        let rt = make_runtime(
            "seedrows",
            SessionStatus::Running,
            "seeded line one\nseeded line two\n",
            None,
        );
        let store = store_with(vec![rt], make_test_db().await);

        assert!(store.attach_scrollback_seed("seedrows", 24).await.is_none());
    }

    #[tokio::test]
    async fn attach_scrollback_seed_skips_alternate_screen_sessions() {
        let rt = make_runtime("seedalt", SessionStatus::Running, "plain\n", None);
        rt.write().feed_engine(b"\x1b[?1049h\x1b[2J\x1b[Htui");
        let store = store_with(vec![rt], make_test_db().await);

        assert!(store.attach_scrollback_seed("seedalt", 24).await.is_none());
    }

    #[tokio::test]
    async fn attach_scrollback_seed_is_none_without_content_rows() {
        let rt = make_runtime("seednone", SessionStatus::Running, "", None);
        let store = store_with(vec![rt], make_test_db().await);

        assert!(store.attach_scrollback_seed("seednone", 24).await.is_none());
    }
}
