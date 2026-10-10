//! The attach path: subscribing a client, forwarding input and resizes.
//!
//! This is the latency-sensitive surface. Lock scopes are deliberately narrow
//! and the input path never holds a write lock across an await.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use tokio::sync::{broadcast, mpsc::error::TrySendError};
use tracing::{debug, warn};

use crate::session::{
    SessionEvent,
    runtime::{ModeSnapshot, SequencedChunk},
};

use super::super::{SessionError, journal, replay};
use super::{
    ATTACH_INPUT_OUTPUT_POLL_INTERVAL, ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT, SessionHandle,
    SessionStore,
};

impl SessionStore {
    pub async fn attach_stream_status(
        &self,
        id: &str,
    ) -> std::result::Result<(bool, bool, Option<i32>), SessionError> {
        match self.lookup_runtime(id).await {
            Ok(handle) => {
                let rt = handle.read();
                Ok((!rt.is_completed(), rt.output_closed, rt.meta.exit_code))
            }
            // Persisted fallback (post-review corrective increment): a
            // session keeps an answerable stream status after eviction or
            // daemon restart, so `logs --after`/`logs --from` keep
            // working against the canonical store.
            Err(err) => match self.db.get_session(id).await {
                Ok(Some(meta)) => Ok((false, true, meta.exit_code)),
                _ => Err(err),
            },
        }
    }

    /// Persisted session dir for a session with no live runtime (evicted
    /// or post-restart); `None` when the id is unknown entirely.
    pub(super) async fn persisted_session_dir(&self, id: &str) -> Option<PathBuf> {
        match self.db.get_session_dir(id).await {
            Ok(Some(dir)) if dir.is_dir() => Some(dir),
            _ => None,
        }
    }

    /// Register an identified attachment. The fencing is per-session
    /// incarnation plus a token. The role is chosen here and never
    /// changes: controllers drive the session snapshot, observers
    /// launched with `--observe` only watch. When a controller declares
    /// a viewport, the initial geometry is applied through the session
    /// sequencer before any snapshot is taken.
    pub async fn attach_register(
        &self,
        id: &str,
        kind: crate::session::registry::AttachKind,
        role: crate::session::registry::AttachRole,
        viewport: Option<(u16, u16)>,
    ) -> std::result::Result<AttachRegistration, SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let (attachment_id, role, _resized) = {
            let mut rt = handle.write();
            rt.register_attachment(kind, role, viewport)
        };
        // Migration note: resize geometry is journaled by resize_pty itself;
        // no separate events.log record.
        debug!(
            session_id = id,
            attachment_id,
            role = role.as_str(),
            "attach client registered"
        );
        Ok(AttachRegistration {
            attachment_id,
            role,
        })
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
            ModeSnapshot,
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
        // The filtered display stream is derived from the raw journal;
        // sessions without a journal are pre-0.5 and unsupported
        // (the output.log fallback was removed; see MIGRATION.md).
        if !dir.join(journal::JOURNAL_DIR_NAME).is_dir() {
            warn!(
                session_id = id,
                "session has no journal (pre-0.5 log format)"
            );
            return Err(SessionError::Evicted);
        }
        let (data, end_offset) = replay::filtered_stream_from(&dir, offset).map_err(|err| {
            warn!(session_id = id, %err, "failed to derive attach output from the journal");
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
        Ok((chunks, end_offset, rx, modes))
    }

    /// Initialise an attach stream from the current rendered terminal state
    /// instead of replaying persisted PTY history from byte offset 0.
    /// Read one bounded window of the persisted filtered stream starting
    /// at `from`: at most `max_bytes`, so a lagged attachment resyncs in
    /// slices instead of one unbounded allocation.
    pub async fn attach_resync_window(
        &self,
        id: &str,
        from: u64,
        max_bytes: usize,
    ) -> std::result::Result<Vec<u8>, SessionError> {
        let dir = match self.lookup_runtime(id).await {
            Ok(handle) => handle.read().dir.clone(),
            // Persisted fallback (post-review corrective increment):
            // bounded resync windows work after eviction/restart too.
            Err(err) => match self.persisted_session_dir(id).await {
                Some(dir) => dir,
                None => return Err(err),
            },
        };
        tokio::task::spawn_blocking(move || read_filtered_window(&dir, from, max_bytes))
            .await
            .map_err(|err| SessionError::Internal(format!("stream window worker failed: {err}")))?
            .map_err(|err| {
                warn!(session_id = id, %err, "attach resync window read failed");
                SessionError::Evicted
            })
    }

    /// Record a client's applied-cursor credit. Stale attachment
    /// tokens and non-advancing cursors are ignored. The report lands in
    /// the attachment's shared cell, which its output pump gates on
    /// (credits are enforced, not advisory).
    pub async fn attach_report_applied(&self, id: &str, attachment_id: u64, cursor: u64) {
        if let Ok(handle) = self.lookup_runtime(id).await {
            let mut rt = handle.write();
            rt.attachments.report_applied(attachment_id, cursor);
        }
    }

    /// The shared applied-cursor cell for one registered attachment, used
    /// by the output pump's credit gate. `None` for stale/unknown
    /// attachment tokens.
    pub(crate) async fn attachment_credit_cell(
        &self,
        id: &str,
        attachment_id: u64,
    ) -> Option<std::sync::Arc<std::sync::atomic::AtomicU64>> {
        let handle = self.lookup_runtime(id).await.ok()?;
        let rt = handle.read();
        rt.attachments.credit_cell(attachment_id)
    }

    /// Slowest applied cursor across registered attachments that have
    /// reported (backpressure signal; `None` when none have).
    #[cfg(test)]
    pub async fn attach_min_applied_cursor(&self, id: &str) -> Option<u64> {
        let handle = self.lookup_runtime(id).await.ok()?;
        let rt = handle.read();
        rt.attachments
            .attachments()
            .map(|a| a.applied_cursor.load(std::sync::atomic::Ordering::Relaxed))
            .filter(|&cursor| cursor > 0)
            .min()
    }

    /// Filtered-stream length: the in-memory count for live
    /// sessions (it counts bytes the journal appender may not have flushed
    /// yet, so the pump's completion drain knows when the persisted tail
    /// has caught up); the persisted, incarnation-cached derivation for
    /// sessions without a live runtime (post-review corrective increment).
    pub async fn attach_filtered_len(&self, id: &str) -> Option<u64> {
        if let Ok(handle) = self.lookup_runtime(id).await {
            return Some(handle.read().filtered_stream_len());
        }
        self.persisted_filtered_len(id).await
    }

    /// Filtered-stream end offset derived from the persisted journal (or
    /// legacy `output.log`) for a session with no live runtime. The scan
    /// is O(journal), so results are cached per incarnation: a completed
    /// session's stream never changes within one incarnation, and polling
    /// callers (`logs --after`) must not re-scan the whole journal every tick.
    pub(super) async fn persisted_filtered_len(&self, id: &str) -> Option<u64> {
        let dir = self.persisted_session_dir(id).await?;
        if dir.join(journal::JOURNAL_DIR_NAME).is_dir() {
            let incarnation = journal::list_incarnations(&dir.join(journal::JOURNAL_DIR_NAME))
                .ok()?
                .last()
                .copied()?;
            {
                let state = self.mutable.lock().await;
                if let Some(&(cached_incarnation, len)) = state.persisted_stream_len_cache.get(id)
                    && cached_incarnation == incarnation
                {
                    return Some(len);
                }
            }
            let scan_dir = dir.clone();
            let len = tokio::task::spawn_blocking(move || replay::filtered_stream_len(&scan_dir))
                .await
                .ok()?
                .ok()?;
            let mut state = self.mutable.lock().await;
            // Completed sessions accumulate; bound the cache.
            if state.persisted_stream_len_cache.len() >= 1024 {
                state.persisted_stream_len_cache.clear();
            }
            state
                .persisted_stream_len_cache
                .insert(id.to_string(), (incarnation, len));
            Some(len)
        } else {
            None
        }
    }

    pub async fn attach_snapshot_init(
        &self,
        id: &str,
    ) -> std::result::Result<
        (
            Vec<u8>,
            u64,
            broadcast::Receiver<SequencedChunk>,
            ModeSnapshot,
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
        Ok((snapshot, end_offset, rx, modes))
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
    /// Param shape mirrors `ResizeCoordinator::new`; the type alias is internal so
    /// the complexity lives there, not at this subscriber's call sites.
    #[allow(clippy::type_complexity)]
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

    /// Forward raw input bytes to the session PTY.
    ///
    /// Byte-exact: the bytes the client sent reach the PTY unchanged.
    /// Key-to-sequence encoding (DECCKM application cursor keys, modifier
    /// parameters, bracketed-paste wrapping) happens at
    /// the *client* that owns the key event; the daemon never rewrites
    /// input, so pasted or scripted bytes containing `ESC [ A`-style
    /// sequences are not corrupted.
    pub async fn attach_input(
        &self,
        id: &str,
        attachment_id: Option<u64>,
        data: &[u8],
        wait_for_change: bool,
    ) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;

        // Observe-mode enforcement: an attached observer never drives input
        // directly. `None` is the operator control plane
        // (`oly send`, HTTP input), which is not an attachment and stays
        // ungated.
        if let Some(attachment_id) = attachment_id {
            let rt = handle.read();
            check_control(&rt, attachment_id)?;
        }

        // Read lock: send to the PTY channel. try_write_input() is a
        // non-blocking channel send that only needs &self.
        let (initial_total_bytes, byte_len) = {
            let rt = handle.read();
            let initial_total_bytes = rt.filtered_total_bytes;
            let byte_len = data.len();
            match rt.pty.try_write_input(data.to_vec()) {
                Ok(()) => Ok((initial_total_bytes, byte_len)),
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

        // Brief write lock: touch the activity timestamp fields.
        {
            let mut rt = handle.write();
            rt.mark_attach_activity();
            rt.last_input_at = Some(Instant::now());
        }

        debug!(session_id = id, bytes = byte_len, "attach input forwarded");

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
            // Observe-mode enforcement: an observer's declared size is a
            // viewport, recorded for status surfaces only; controllers
            // resize freely — the last successful resize wins.
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
            // Geometry is journaled by resize_pty (no events.log).
            Ok(())
        } else {
            Err(SessionError::Evicted)
        }
    }
}

/// Format a scrollback seed (from [`SessionStore::attach_scrollback_seed`])
/// as terminal bytes to write before the screen snapshot: LF becomes CRLF,
/// the seeded rows are guaranteed to have scrolled off the visible screen
/// (so the snapshot repaint does not duplicate them), and the cursor is
/// homed. Shared by the native attach client and the WebSocket attach path,
/// whose clients render the same byte stream.
pub fn scrollback_seed_bytes(seed: &[u8], rows: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(seed.len() + usize::from(rows) + 16);
    // Start on a blank main viewport at a known cursor/scroll region. EL does
    // not push stale screen contents into history as ED 2 can. History rows
    // are followed by exactly rows-1 line feeds so even the LAST seeded row
    // scrolls off; seed length is irrelevant.
    out.extend_from_slice(b"\x1b[r\x1b[H\x1b[0J");
    // Attaching terminals are in raw mode, so `\n` does not imply a carriage
    // return and every seeded row needs an explicit CRLF. Unlike ED 2
    // (`\x1b[2J`), whose effect on scrollback varies between terminals, the
    // plain scrolling below works everywhere.
    for &byte in seed {
        if byte == b'\n' {
            out.push(b'\r');
        }
        out.push(byte);
    }
    out.resize(out.len() + usize::from(rows.max(1) - 1), b'\n');
    out.extend_from_slice(b"\x1b[H");
    out
}

/// One bounded window of the persisted filtered display stream, derived
/// from the journal. Shared by live-runtime and persisted fallback paths
/// so both read the same canonical bytes.
fn read_filtered_window(
    dir: &Path,
    from: u64,
    max_bytes: usize,
) -> std::result::Result<Vec<u8>, String> {
    replay::filtered_stream_window(dir, from, max_bytes).map_err(|err| err.to_string())
}

/// The result of registering an attachment.
#[derive(Debug, Clone, Copy)]
pub struct AttachRegistration {
    /// Fencing token identifying this attachment for its lifetime.
    pub attachment_id: u64,
    /// The role granted at register time (fixed for the attachment's life).
    pub role: crate::session::registry::AttachRole,
}

/// Input/geometry gate for attached clients: observers (view-only
/// attaches) cannot drive; stale fencing tokens cannot do anything.
fn check_control(
    rt: &super::super::runtime::SessionRuntime,
    attachment_id: u64,
) -> std::result::Result<(), SessionError> {
    if !rt.attachments.contains(attachment_id) {
        return Err(SessionError::StaleAttachment);
    }
    if rt.attachments.can_control(attachment_id) {
        Ok(())
    } else {
        Err(SessionError::ViewOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport::*;
    use super::*;
    use crate::session::{SessionStatus, pty::collect_chunk_bytes};

    #[test]
    fn scrollback_seed_places_every_row_in_history_without_blank_gap() {
        for count in [1, 2, 3, 8] {
            let seed = (0..count)
                .map(|index| format!("seed {index}\n"))
                .collect::<String>();
            let mut outer = crate::terminal::Terminal::new(3, 20, 100);
            outer.feed(&scrollback_seed_bytes(seed.as_bytes(), 3));
            assert_eq!(
                outer.history_size(),
                count,
                "seed of {count} rows was not fully scrolled off"
            );
            assert_eq!(
                outer.full_lines()[..count],
                (0..count)
                    .map(|index| format!("seed {index}"))
                    .collect::<Vec<_>>()
            );
            assert!(outer.screen_lines().iter().all(String::is_empty));
        }
    }

    #[test]
    fn scrollback_seed_uses_crlf_and_scrolls_every_seeded_row_into_history() {
        let bytes = scrollback_seed_bytes(b"line one\nline two\n\x1b[0m", 3);
        assert_eq!(
            bytes,
            b"\x1b[r\x1b[H\x1b[0Jline one\r\nline two\r\n\x1b[0m\n\n\x1b[H".as_slice()
        );
    }

    #[test]
    fn scrollback_seed_leaves_blank_screen_without_ed2() {
        let bytes = scrollback_seed_bytes(b"only\n", 2);
        assert!(!bytes.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(bytes.ends_with(b"\x1b[H"));
    }

    #[test]
    fn scrollback_seed_taller_than_screen_scrolls_its_final_rows_off() {
        // Even a deep seed leaves its final screenful visible until flushed.
        let seed = "row 0\nrow 1\nrow 2\n";
        let bytes = scrollback_seed_bytes(seed.as_bytes(), 3);
        assert_eq!(
            bytes,
            b"\x1b[r\x1b[H\x1b[0Jrow 0\r\nrow 1\r\nrow 2\r\n\n\n\x1b[H".as_slice()
        );
    }

    #[test]
    fn scrollback_seed_one_line_short_of_screen_still_scrolls_off() {
        // Known starting cursor means rows-1 newlines suffice for any seed.
        let seed = "row 0\nrow 1\n";
        let bytes = scrollback_seed_bytes(seed.as_bytes(), 3);
        assert_eq!(
            bytes,
            b"\x1b[r\x1b[H\x1b[0Jrow 0\r\nrow 1\r\n\n\n\x1b[H".as_slice()
        );
    }

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

        let (chunks, end_offset, _rx, _modes) = store
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
            .attach_input("inp0001", None, b"hello\r", true)
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
            .attach_input("inp0002", None, b"x", true)
            .await
            .expect("attach_input should succeed");

        let locked = rt_clone.read();
        assert!(
            locked.last_input_at.is_some(),
            "last_input_at should be set after input"
        );
    }

    /// Byte-exactness: the daemon NEVER rewrites input, even when the
    /// child has DECCKM application cursor keys enabled — the
    /// key-to-sequence mapping is the sending client's job, and a pasted
    /// or scripted stream containing `ESC [ A` must survive unchanged.
    #[tokio::test]
    async fn test_attach_input_never_rewrites_arrow_bytes_under_decckm() {
        let (rt, mut writer_rx) = make_runtime_writable("inp0003", SessionStatus::Running);
        {
            let mut locked = rt.write();
            locked.feed_engine(b"\x1b[?1h");
        }
        let store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0003", None, b"\x1b[A\x1b[B\x1b[C\x1b[D", true)
            .await
            .expect("attach_input should succeed");

        let written = writer_rx.recv().await.expect("should receive bytes");
        assert_eq!(
            written, b"\x1b[A\x1b[B\x1b[C\x1b[D",
            "raw arrow sequences pass through byte-exact under DECCKM"
        );
    }

    /// Byte-exactness for non-UTF-8 input (binary paste, hex: specs):
    /// bytes that are not valid UTF-8 reach the PTY unmodified.
    #[tokio::test]
    async fn test_attach_input_passes_non_utf8_bytes_through() {
        let (rt, mut writer_rx) = make_runtime_writable("inp0004", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        let raw: &[u8] = b"\x00\xff\xfe\x80abc\x1b[A";
        store
            .attach_input("inp0004", None, raw, true)
            .await
            .expect("attach_input should succeed");

        let written = writer_rx.recv().await.expect("should receive bytes");
        assert_eq!(written, raw, "non-UTF-8 bytes pass through byte-exact");
    }

    #[tokio::test]
    async fn test_attach_input_not_found_for_unknown_session() {
        let store = SessionStore::with_journal_byte_cap(900, 0, make_test_db().await);
        let result = store.attach_input("no_such_id", None, b"data", true).await;
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

        let result = store.attach_input("inpbusy1", None, b"second", true).await;
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
            .attach_input("inpwait1", None, b"x", true)
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
            .attach_input("inpwait2", None, b"x", true)
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
                crate::session::registry::AttachRole::Controller,
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
                crate::session::registry::AttachRole::Controller,
                None,
            )
            .await
            .expect("first register should succeed");
        let reg2 = store
            .attach_register(
                "detach002",
                crate::session::registry::AttachKind::Cli,
                crate::session::registry::AttachRole::Controller,
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
        use crate::session::registry::{AttachKind, AttachRole};

        let (rt, _writer_rx) = make_runtime_writable("ack0001", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        let reg_a = store
            .attach_register("ack0001", AttachKind::Cli, AttachRole::Controller, None)
            .await
            .expect("attach A");
        let reg_b = store
            .attach_register("ack0001", AttachKind::Web, AttachRole::Observer, None)
            .await
            .expect("attach B");

        // Credits advance monotonically per attachment (the cell's
        // `fetch_max` lets stale reports no-op instead of regressing).
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
    async fn attach_observe_mode_gates_input_and_resize() {
        use crate::session::SessionError;
        use crate::session::registry::{AttachKind, AttachRole};

        let (rt, _writer_rx) = make_runtime_writable("ctl0001", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        // Every attach is a controller by default — several at once is fine.
        let first = store
            .attach_register("ctl0001", AttachKind::Cli, AttachRole::Controller, None)
            .await
            .expect("first attach");
        assert_eq!(first.role, AttachRole::Controller);
        let second = store
            .attach_register("ctl0001", AttachKind::Web, AttachRole::Controller, None)
            .await
            .expect("second attach");
        assert_eq!(second.role, AttachRole::Controller);
        store
            .attach_input("ctl0001", Some(first.attachment_id), b"a", false)
            .await
            .expect("first controller input");
        store
            .attach_input("ctl0001", Some(second.attachment_id), b"b", false)
            .await
            .expect("second controller input");

        // An explicit observe attach is view-only for its whole lifetime.
        let observer = store
            .attach_register("ctl0001", AttachKind::Cli, AttachRole::Observer, None)
            .await
            .expect("observe attach");
        assert_eq!(observer.role, AttachRole::Observer);
        let err = store
            .attach_input("ctl0001", Some(observer.attachment_id), b"x", false)
            .await
            .expect_err("observer input must be gated");
        assert!(matches!(err, SessionError::ViewOnly));
        let err = store
            .attach_resize("ctl0001", Some(observer.attachment_id), 24, 80)
            .await
            .expect_err("observer resize must be gated");
        assert!(matches!(err, SessionError::ViewOnly));

        // The operator control plane (no attachment) stays ungated.
        store
            .attach_input("ctl0001", None, b"ls", false)
            .await
            .expect("operator input must not be gated");

        // Detaching leaves a precise stale-token error, and other
        // attachments keep driving.
        store
            .attach_detach("ctl0001", observer.attachment_id)
            .await
            .expect("detach observer");
        let err = store
            .attach_input("ctl0001", Some(observer.attachment_id), b"x", false)
            .await
            .expect_err("stale attachment must fail precisely");
        assert!(matches!(err, SessionError::StaleAttachment));
        store
            .attach_input("ctl0001", Some(first.attachment_id), b"x", false)
            .await
            .expect("remaining controller input");
    }

    #[tokio::test]
    async fn observe_viewport_never_resizes_but_controllers_win_last_writer() {
        use crate::session::registry::{AttachKind, AttachRole};

        let (rt, _writer_rx) = make_runtime_writable("ctl0002", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        // Controller A attaches with a 24x80 viewport: geometry applies.
        let _first = store
            .attach_register(
                "ctl0002",
                AttachKind::Cli,
                AttachRole::Controller,
                Some((24, 80)),
            )
            .await
            .expect("first attach");
        let initial = store.subscribe_resize("ctl0002").and_then(|(_, size)| size);
        assert_eq!(initial, Some((24, 80)));

        // An observer joins with a different viewport; session geometry
        // is unchanged.
        let observer = store
            .attach_register(
                "ctl0002",
                AttachKind::Web,
                AttachRole::Observer,
                Some((40, 120)),
            )
            .await
            .expect("observe attach");
        assert_eq!(observer.role, AttachRole::Observer);
        let before = store.subscribe_resize("ctl0002").and_then(|(_, size)| size);
        assert_eq!(before, Some((24, 80)), "observer viewport must not resize");
        // Even an explicit resize from the observer changes nothing.
        let err = store
            .attach_resize("ctl0002", Some(observer.attachment_id), 50, 200)
            .await
            .expect_err("observer resize must be gated");
        assert!(matches!(err, crate::session::SessionError::ViewOnly));

        // A new controller's declared viewport applies immediately
        // (last-writer-wins).
        let _third = store
            .attach_register(
                "ctl0002",
                AttachKind::Cli,
                AttachRole::Controller,
                Some((40, 120)),
            )
            .await
            .expect("third attach");
        let after = store.subscribe_resize("ctl0002").and_then(|(_, size)| size);
        assert_eq!(
            after,
            Some((40, 120)),
            "a controller's attach resizes the session to its viewport"
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

    // -----------------------------------------------------------------------
    // Persisted agent surfaces (post-review corrective increment): cursor,
    // history windows and stream status stay answerable for sessions with no
    // live runtime — after eviction or a daemon restart.
    // -----------------------------------------------------------------------

    /// A completed session that exists only on disk: db row plus a sealed
    /// one-incarnation journal, with no runtime in any store.
    async fn persisted_journal_session(
        id: &str,
        chunks: &[&[u8]],
        exit_code: Option<i32>,
    ) -> (Arc<crate::db::Database>, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("oly_persisted_{id}_{}", uuid::Uuid::new_v4()));
        let sessions_dir = base.join("sessions");
        let session_dir = sessions_dir.join(id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let db = Arc::new(
            crate::db::Database::open(&base.join("oly.db"), sessions_dir)
                .await
                .expect("open test db"),
        );
        let (mut journal, _incarnation, _report) =
            crate::session::journal::ShadowJournal::open_with_options(
                &session_dir,
                Duration::from_secs(3600),
                1 << 20,
            )
            .expect("open journal");
        for chunk in chunks {
            journal
                .record_output(bytes::Bytes::copy_from_slice(chunk))
                .expect("record output");
        }
        journal.shutdown();
        db.insert_session(&crate::session::SessionMeta {
            id: id.to_string(),
            title: None,
            tags: vec![],
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            ended_at: Some(chrono::Utc::now()),
            resume_command: None,
            status: SessionStatus::Stopped,
            pid: None,
            exit_code,
            notifications_enabled: true,
            foreground_color: None,
            background_color: None,
        })
        .await
        .expect("insert session row");
        (db, session_dir)
    }

    #[tokio::test]
    async fn agent_surfaces_fall_back_to_persisted_state_without_runtime() {
        let (db, _dir) =
            persisted_journal_session("persist1", &[b"hello ", b"world"], Some(7)).await;
        let store = store_with(Vec::new(), db);

        // The session-cursor trio backs `logs --from/--after`.
        assert_eq!(store.attach_filtered_len("persist1").await, Some(11));
        assert_eq!(
            store
                .attach_resync_window("persist1", 0, 1024)
                .await
                .expect("resync window"),
            b"hello world"
        );
        assert_eq!(
            store
                .attach_resync_window("persist1", 6, 3)
                .await
                .expect("offset window"),
            b"wor"
        );
        assert_eq!(
            store
                .attach_stream_status("persist1")
                .await
                .expect("stream status"),
            (false, true, Some(7))
        );
        assert_eq!(store.journal_incarnation("persist1"), Some(1));
        // The incarnation-keyed cache serves repeat polls consistently.
        assert_eq!(store.attach_filtered_len("persist1").await, Some(11));
    }

    #[tokio::test]
    async fn persisted_fallback_preserves_unknown_session_errors() {
        let store = SessionStore::with_journal_byte_cap(900, 0, make_test_db().await);
        assert!(matches!(
            store.attach_stream_status("nope000").await,
            Err(SessionError::NotRunning)
        ));
        assert_eq!(store.attach_filtered_len("nope000").await, None);
        assert!(store.attach_resync_window("nope000", 0, 64).await.is_err());
        assert_eq!(store.journal_incarnation("nope000"), None);
    }

    #[tokio::test]
    async fn persisted_filtered_len_cache_tracks_incarnation_changes() {
        let (db, session_dir) = persisted_journal_session("persist2", &[b"first"], Some(0)).await;
        let store = store_with(Vec::new(), db);
        assert_eq!(store.attach_filtered_len("persist2").await, Some(5));

        // A restart appends a fresh incarnation; the per-incarnation cache
        // must not go stale, and resync windows follow the newest stream.
        let (mut journal, incarnation, _report) =
            crate::session::journal::ShadowJournal::open_with_options(
                &session_dir,
                Duration::from_secs(3600),
                1 << 20,
            )
            .expect("reopen journal");
        assert_eq!(incarnation, 2);
        journal
            .record_output(bytes::Bytes::from_static(b"second"))
            .expect("record output");
        journal.shutdown();

        assert_eq!(store.journal_incarnation("persist2"), Some(2));
        assert_eq!(store.attach_filtered_len("persist2").await, Some(6));
        assert_eq!(
            store
                .attach_resync_window("persist2", 0, 64)
                .await
                .expect("resync after restart"),
            b"second"
        );
    }
}
