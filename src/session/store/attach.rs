//! The attach path: subscribing a client, forwarding input and resizes.
//!
//! This is the latency-sensitive surface. Lock scopes are deliberately narrow
//! and the input path never holds a write lock across an await.

use std::{sync::Arc, time::Instant};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc::error::TrySendError};
use tracing::{debug, warn};

use crate::session::SessionEvent;

use super::super::{
    SessionError,
    persist::{append_resize_event, current_output_offset, read_output_from},
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

    pub async fn register_attach_client(&self, id: &str) {
        let sessions = self.sessions.load();
        if let Some(handle) = sessions.get(id).cloned() {
            handle.write().register_attach_client();
        }
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
            broadcast::Receiver<Bytes>,
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
        let (data, end_offset) = read_output_from(&dir, offset).map_err(|err| {
            warn!(session_id = id, %err, "failed to read persisted attach output");
            SessionError::Evicted
        })?;
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
    pub async fn attach_snapshot_init(
        &self,
        id: &str,
    ) -> std::result::Result<(Vec<u8>, u64, broadcast::Receiver<Bytes>, bool, bool), SessionError>
    {
        let handle = self.lookup_runtime(id).await?;
        let rt = handle.read();
        let snapshot = rt.attach_snapshot_bytes();
        let end_offset = current_output_offset(&rt.dir);
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

    pub async fn attach_detach(&self, id: &str) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        handle.write().detach_attach_client();
        debug!(session_id = id, "attach detach acknowledged");
        Ok(())
    }

    pub async fn attach_input(
        &self,
        id: &str,
        data: &str,
        wait_for_change: bool,
    ) -> std::result::Result<(), SessionError> {
        // Avoid sending lose focus escape sequence which will cause other clients not able to input anything
        if data == "\x1b[O" {
            return Ok(());
        }

        let handle = self.lookup_runtime(id).await?;

        // Read lock: gather mode flags, transform input, send to PTY channel.
        // try_write_input() is a non-blocking channel send that only needs &self.
        let (initial_total_bytes, byte_len, transformed, app_cursor_keys) = {
            let rt = handle.read();
            let initial_total_bytes = rt.raw_total_bytes;
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
            let current_total_bytes = handle.read().raw_total_bytes;

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
        rows: u16,
        cols: u16,
    ) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let resized = {
            let mut rt = handle.write();
            rt.mark_attach_activity();
            rt.resize_pty(rows, cols)
        };

        debug!(
            session_id = id,
            rows, cols, resized, "attach resize requested"
        );
        if resized {
            let rt = handle.read();
            let offset = current_output_offset(&rt.dir);
            let _ = append_resize_event(&rt.dir, offset, rows, cols);
            Ok(())
        } else {
            Err(SessionError::Evicted)
        }
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
            .attach_input("inp0001", "hello\r", true)
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
            .attach_input("inp0002", "x", true)
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
            locked.screen_parser.process(b"\x1b[?1h");
        }
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0003", "\x1b[A", true)
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
            locked.screen_parser.process(b"\x1b[?1h");
        }
        let store = store_with(vec![rt], make_test_db().await);

        // Send all four arrow sequences at once.
        store
            .attach_input("inp0004", "\x1b[A\x1b[B\x1b[C\x1b[D", true)
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
            .attach_input("inp0005", "\x1b[A\x1b[B", true)
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
        let result = store.attach_input("no_such_id", "data", true).await;
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

        let result = store.attach_input("inpbusy1", "second", true).await;
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
            locked.raw_total_bytes += 1;
            locked.last_total_bytes += 1;
            locked.last_output_epoch = Some(Instant::now());
        });

        let started = Instant::now();
        store
            .attach_input("inpwait1", "x", true)
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
            .attach_input("inpwait2", "x", true)
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
    async fn test_attach_detach_clears_presence_and_activity() {
        let rt = make_runtime("detach001", SessionStatus::Running, "$ prompt", None);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        store.register_attach_client("detach001").await;
        {
            let mut locked = rt_clone.write();
            locked.mark_attach_activity();
        }

        store
            .attach_detach("detach001")
            .await
            .expect("detach should succeed");

        let locked = rt_clone.read();
        assert!(
            locked.last_attach_activity_at.is_none(),
            "detach should clear attach activity"
        );
    }

    #[tokio::test]
    async fn test_attach_detach_only_clears_after_final_client_disconnects() {
        let rt = make_runtime("detach002", SessionStatus::Running, "$ prompt", None);
        let rt_clone = rt.clone();
        let store = store_with(vec![rt], make_test_db().await);

        store.register_attach_client("detach002").await;
        store.register_attach_client("detach002").await;
        {
            let mut locked = rt_clone.write();
            locked.mark_attach_activity();
        }

        store
            .attach_detach("detach002")
            .await
            .expect("first detach should succeed");

        {
            let locked = rt_clone.read();
            assert_eq!(
                locked.attach_count, 1,
                "one client should still remain registered"
            );
            assert!(
                locked.last_attach_activity_at.is_some(),
                "activity timestamp should remain until the last client disconnects"
            );
        }

        store
            .attach_detach("detach002")
            .await
            .expect("second detach should succeed");

        let locked = rt_clone.read();
        assert_eq!(locked.attach_count, 0, "all clients should be disconnected");
        assert!(
            locked.last_attach_activity_at.is_none(),
            "final detach should clear attach activity"
        );
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
}
