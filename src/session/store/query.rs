//! Read-only session queries and metadata updates.
//!
//! Nothing here starts or stops a runtime; these methods observe the session
//! map or edit a session's metadata in place.

use std::sync::Arc;

use tracing::{debug, info};

use crate::{
    db::meta_to_summary,
    error::AppError,
    error::Result,
    protocol::{ListQuery, SessionSummary},
    session::{SessionEvent, validate_session_metadata_update},
};

use super::super::SessionError;
use super::super::logs::{finish_render, split_rendered_log_output};
use super::SessionStore;

/// Atomic result of a live screen render: the rendered bytes, the resize
/// history the render was generated against, and the filtered-stream end
/// offset captured under the same runtime read lock. Callers MUST use
/// `captured_offset` as the cursor they hand back to a follow-up `--since`
/// call to avoid leaking bytes the render never saw.
pub struct LiveRenderSnapshot {
    pub output: Vec<u8>,
    pub resize_history: Vec<crate::protocol::LogResize>,
    pub captured_offset: u64,
    pub incarnation: Option<u64>,
}

impl SessionStore {
    /// Liveness, incarnation and byte offset share one runtime lock.
    pub async fn stream_observation(
        &self,
        id: &str,
    ) -> Option<(bool, Option<i32>, u64, Option<u64>, bool)> {
        if let Ok(handle) = self.lookup_runtime(id).await {
            let rt = handle.read();
            return Some((
                !rt.is_completed(),
                rt.meta.exit_code,
                rt.filtered_stream_len(),
                rt.journal_incarnation(),
                rt.input_needed(),
            ));
        }
        let dir = self.persisted_session_dir(id).await?;
        let incarnation_dir = dir.clone();
        let incarnation = tokio::task::spawn_blocking(move || {
            crate::session::journal::list_incarnations(
                &incarnation_dir.join(crate::session::journal::JOURNAL_DIR_NAME),
            )
            .ok()
            .and_then(|v| v.last().copied())
        })
        .await
        .ok()
        .flatten();
        let offset = self.persisted_filtered_len(id).await?;
        let after = tokio::task::spawn_blocking(move || {
            crate::session::journal::list_incarnations(
                &dir.join(crate::session::journal::JOURNAL_DIR_NAME),
            )
            .ok()
            .and_then(|v| v.last().copied())
        })
        .await
        .ok()
        .flatten();
        if after != incarnation {
            return None;
        }
        if let Ok(handle) = self.lookup_runtime(id).await {
            let rt = handle.read();
            return Some((
                !rt.is_completed(),
                rt.meta.exit_code,
                rt.filtered_stream_len(),
                rt.journal_incarnation(),
                rt.input_needed(),
            ));
        }
        let (_, _, exit_code) = self
            .attach_stream_status(id)
            .await
            .unwrap_or((false, true, None));
        Some((false, exit_code, offset, incarnation, false))
    }

    pub async fn list_summaries(&self, query: &ListQuery) -> Result<Vec<SessionSummary>> {
        // PERF: the journal-derived byte offset is deliberately not fetched
        // in SQL. A live session's whole summary comes from its runtime
        // handle (O(1) counters); only rows without a live handle consult
        // the persisted stream, through the per-incarnation cache in
        // `persisted_filtered_len`. Decoding journals inline per row made
        // every list operation O(total journal bytes) — the source of the
        // "oly ls is mysteriously slow" reports (see PERFORMANCE.md).
        let mut sessions = self.db.list_summaries_without_offsets(query).await?;

        let live_sessions = self.sessions.load();
        for session in &mut sessions {
            if let Some(handle) = live_sessions.get(&session.id) {
                *session = handle.read().to_summary();
            } else if let Some(len) = self.persisted_filtered_len(&session.id).await {
                session.last_total_bytes = len;
            }
        }

        Ok(sessions)
    }

    pub fn get_summary(&self, id: &str) -> Option<SessionSummary> {
        let sessions = self.sessions.load();
        sessions.get(id).map(|handle| handle.read().to_summary())
    }

    pub fn get_exit_code(&self, id: &str) -> Option<i32> {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .and_then(|handle| handle.read().meta.exit_code)
    }

    pub fn is_running(&self, id: &str) -> bool {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .map(|handle| !handle.read().is_completed())
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub fn is_input_needed(&self, id: &str) -> bool {
        self.sessions
            .load()
            .get(id)
            .is_some_and(|handle| handle.read().input_needed())
    }

    pub async fn update_session_metadata(
        &self,
        id: &str,
        title: Option<String>,
        tags: Option<Vec<String>>,
        notifications_enabled: Option<bool>,
    ) -> Result<SessionSummary> {
        let title_provided = title.is_some();
        let (title, tags) = validate_session_metadata_update(title, tags)?;
        let live_handle = {
            let sessions = self.sessions.load();
            sessions.get(id).cloned()
        };

        let summary = if let Some(handle) = live_handle {
            let meta = {
                let mut rt = handle.write();
                if notifications_enabled.is_some() && rt.is_completed() {
                    return Err(AppError::Protocol(format!("session not running: {id}")));
                }
                if title_provided {
                    rt.meta.title = title.clone();
                    // An explicit title update wins over terminal-emitted
                    // title signals; clearing it hands control back to them.
                    rt.title_user_set = title.is_some();
                }
                if let Some(tags) = tags.as_ref() {
                    rt.meta.tags = tags.clone();
                }
                if let Some(enabled) = notifications_enabled {
                    rt.set_notifications_enabled(enabled);
                }
                rt.meta.clone()
            };
            self.db.update_session(&meta).await?;
            handle.read().to_summary()
        } else {
            let Some(mut meta) = self.db.get_session(id).await? else {
                return Err(AppError::Protocol(format!("session not found: {id}")));
            };
            if notifications_enabled.is_some() {
                return Err(AppError::Protocol(format!("session not running: {id}")));
            }
            if title_provided {
                meta.title = title;
            }
            if let Some(tags) = tags {
                meta.tags = tags;
            }
            self.db.update_session(&meta).await?;
            meta_to_summary(&meta, false, self.db.session_output_offset(id))
        };

        info!(session_id = id, "session metadata updated");
        let _ = self
            .event_tx
            .send(SessionEvent::SessionUpdated(summary.clone()));
        Ok(summary)
    }

    /// Returns the session's lock-free terminal-mode mirror.
    ///
    /// Attach relays hold this for the lifetime of the connection so they can
    /// detect DECCKM/bracketed-paste changes after every output chunk without
    /// taking the session lock.
    pub fn shared_modes(&self, id: &str) -> Option<Arc<crate::session::runtime::SharedModes>> {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .map(|handle| Arc::clone(&handle.read().shared_modes))
    }
    /// Current journal incarnation of a session, if journaled: the live
    /// runtime's counter, or — for sessions without a runtime (evicted or
    /// post-restart, post-review corrective increment) — the newest
    /// incarnation on disk. `None` for unknown or non-journaled sessions.
    pub fn journal_incarnation(&self, id: &str) -> Option<u64> {
        let sessions = self.sessions.load();
        if let Some(handle) = sessions.get(id) {
            // Prefer the live writer's incarnation; a runtime without a
            // live journal (legacy or test fixture) falls through to the
            // on-disk journal, which is also where `attach_subscribe_init`
            // reads the stream from — fencing must name the same source.
            if let Some(incarnation) = handle.read().journal_incarnation() {
                return Some(incarnation);
            }
            let dir = handle.read().dir.clone();
            return disk_incarnation(&dir);
        }
        disk_incarnation(&self.db.session_dir_by_id(id))
    }

    /// Live-tail render of the engine screen, off the runtime read
    /// lock. Snapshots the engine's content rows under the guard,
    /// drops it, then runs the CPU-only `finish_render` on
    /// `spawn_blocking`. Holding the runtime read lock across a full
    /// tabular render was starving the PTY reader's write lock when the
    /// visible region grew.
    pub async fn render_live_logs(
        &self,
        id: &str,
        tail: usize,
        keep_color: bool,
        term_cols: u16,
    ) -> std::result::Result<(Vec<u8>, Vec<crate::protocol::LogResize>), SessionError> {
        let snapshot = self
            .snapshot_live_render(id, tail, keep_color, term_cols)
            .await?;
        Ok((snapshot.output, snapshot.resize_history))
    }

    /// Atomic variant of [`Self::render_live_logs`]: returns the rendered
    /// screen bytes together with the filtered-stream end offset as
    /// captured under the same runtime read lock that produced them. The
    /// captured offset is the right cursor to hand back to a subsequent
    /// `--since` read; using a separately-locked `attach_filtered_len`
    /// call would race the PTY writer and leak bytes the render never
    /// saw.
    pub async fn snapshot_live_render(
        &self,
        id: &str,
        tail: usize,
        keep_color: bool,
        term_cols: u16,
    ) -> std::result::Result<LiveRenderSnapshot, SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let (rows, resize_history, offset, incarnation) = {
            let rt = handle.read();
            if rt.is_completed() || rt.output_closed {
                return Err(SessionError::NotRunning);
            }
            (
                rt.snapshot_engine_rows(keep_color, term_cols),
                rt.resize_history.clone(),
                rt.filtered_stream_len(),
                rt.journal_incarnation(),
            )
        };

        let rendered = tokio::task::spawn_blocking(move || finish_render(rows, tail, keep_color))
            .await
            .map_err(|join_err| {
                SessionError::Internal(format!("log render worker join failed: {join_err}"))
            })?;

        Ok(LiveRenderSnapshot {
            output: rendered,
            resize_history,
            captured_offset: offset,
            incarnation,
        })
    }

    /// Same off-lock pattern as [`Self::render_live_logs`], but
    /// returns the typed chunks used by the HTTP live-tail endpoint.
    pub async fn read_live_log_tail_page(
        &self,
        id: &str,
        tail: usize,
    ) -> std::result::Result<
        (Vec<String>, usize, usize, Vec<crate::protocol::LogResize>),
        SessionError,
    > {
        let handle = self.lookup_runtime(id).await?;
        let (rows, resize_history) = {
            let rt = handle.read();
            if rt.is_completed() || rt.output_closed {
                return Err(SessionError::NotRunning);
            }
            let term_cols = rt
                .pty_size
                .map(|(_, cols)| cols)
                .or_else(|| rt.resize_history.last().map(|resize| resize.cols))
                .filter(|cols| *cols > 0)
                .unwrap_or(80);
            (
                rt.snapshot_engine_rows(true, term_cols),
                rt.resize_history.clone(),
            )
        };

        let rendered = tokio::task::spawn_blocking(move || finish_render(rows, tail, true))
            .await
            .map_err(|join_err| {
                SessionError::Internal(format!("log render worker join failed: {join_err}"))
            })?;

        let chunks = split_rendered_log_output(&rendered);
        let total = chunks.len();
        Ok((chunks, total, 0, resize_history))
    }

    pub async fn read_live_log_chunk_count(
        &self,
        id: &str,
    ) -> std::result::Result<usize, SessionError> {
        let (_, total, _, _) = self.read_live_log_tail_page(id, usize::MAX).await?;
        Ok(total)
    }

    pub async fn set_notifications_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> std::result::Result<(), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let (meta, previous) = {
            let mut rt = handle.write();
            if rt.is_completed() {
                return Err(SessionError::NotRunning);
            }
            let previous = rt.notifications_enabled;
            rt.set_notifications_enabled(enabled);
            (rt.meta.clone(), previous)
        };
        if let Err(err) = self.db.update_session(&meta).await {
            handle.write().set_notifications_enabled(previous);
            debug!(session_id = id, %err, "failed to persist session notification setting");
            return Err(SessionError::Persistence(err.to_string()));
        }
        debug!(
            session_id = id,
            notifications_enabled = enabled,
            "session notification setting updated"
        );
        Ok(())
    }
}

/// Latest journal incarnation on disk for a session directory, if any.
fn disk_incarnation(session_dir: &std::path::Path) -> Option<u64> {
    let journal_dir = session_dir.join(super::super::journal::JOURNAL_DIR_NAME);
    super::super::journal::list_incarnations(&journal_dir)
        .ok()?
        .last()
        .copied()
}

#[cfg(test)]
// Mirror of the lifecycle/notify convention: tests hold `state.read()`
// across `.await`s so they can assert on a consistent snapshot.
// Production store code in this file stays linted.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::super::testsupport::*;
    use super::*;
    use crate::session::{SessionMeta, SessionStatus};
    use chrono::Utc;

    use std::time::Duration;
    #[tokio::test]
    async fn render_live_logs_uses_runtime_screen_tail() {
        let runtime = make_runtime(
            "live123",
            SessionStatus::Running,
            "persisted line\n",
            Some(Duration::from_secs(5)),
        );
        {
            let mut rt = runtime.write();
            rt.engine = crate::terminal::Terminal::new(24, 80, 0);
            rt.feed_engine(b"\x1b[1;1Hscreen one\x1b[2;1Hscreen two\x1b[3;1Hscreen three");
        }
        let store = store_with(vec![runtime], make_test_db().await);

        let (output, resizes) = store
            .render_live_logs("live123", 2, false, 80)
            .await
            .expect("render live logs");

        assert_eq!(
            String::from_utf8_lossy(&output),
            "screen two\nscreen three\n"
        );
        assert!(resizes.is_empty());
    }

    #[tokio::test]
    async fn render_live_logs_rejects_completed_sessions() {
        let runtime = make_runtime(
            "stopped123",
            SessionStatus::Stopped,
            "persisted line\n",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![runtime], make_test_db().await);

        let err = store
            .render_live_logs("stopped123", 10, false, 80)
            .await
            .expect_err("completed session should not render live logs");

        assert!(matches!(err, SessionError::NotRunning));
    }

    #[tokio::test]
    async fn snapshot_live_render_captures_rows_and_offset_atomically() {
        let runtime = make_runtime(
            "atomic123",
            SessionStatus::Running,
            "persisted line\n",
            Some(Duration::from_secs(5)),
        );
        {
            let mut rt = runtime.write();
            rt.engine = crate::terminal::Terminal::new(24, 80, 0);
            rt.feed_engine(b"alpha\nbeta\n");
            // Force filtered_total_bytes to a known value so the snapshot
            // has a deterministic offset to assert against.
            const SAMPLE_OFFSET: u64 = 7;
            rt.filtered_total_bytes = SAMPLE_OFFSET;
        }
        let store = store_with(vec![runtime], make_test_db().await);

        let snapshot = store
            .snapshot_live_render("atomic123", usize::MAX, false, 80)
            .await
            .expect("atomic snapshot");

        // The captured offset must reflect the bytes the engine has seen
        // at the moment the snapshot was taken; a follow-up `--since`
        // cursor derived from it must NOT replay any bytes between the
        // snapshot and a separately-locked `attach_filtered_len`.
        assert_eq!(
            snapshot.captured_offset, 7,
            "snapshot cursor must be paired with the engine rows"
        );
        assert!(
            snapshot.output.starts_with(b"alpha\n"),
            "rendered output preserved"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_snapshot_cursor_stays_paired_with_concurrent_output() {
        let runtime = make_runtime(
            "race123",
            SessionStatus::Running,
            "",
            Some(Duration::from_secs(5)),
        );
        {
            let mut rt = runtime.write();
            rt.engine = crate::terminal::Terminal::new(24, 80, 0);
            rt.feed_engine(b"0");
            rt.filtered_total_bytes = 0;
        }
        let store = store_with(vec![runtime.clone()], make_test_db().await);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_stop = stop.clone();
        let writer = std::thread::spawn(move || {
            let mut generation = 0u64;
            while !writer_stop.load(std::sync::atomic::Ordering::Relaxed) {
                generation += 1;
                let mut rt = runtime.write();
                rt.feed_engine(format!("\x1b[H\x1b[2J{generation}").as_bytes());
                rt.filtered_total_bytes = generation;
                drop(rt);
                std::thread::yield_now();
            }
        });
        let mut mismatch = None;
        for _ in 0..100 {
            let snapshot = store
                .snapshot_live_render("race123", usize::MAX, false, 80)
                .await
                .unwrap();
            let visible = String::from_utf8(snapshot.output)
                .unwrap()
                .trim()
                .parse::<u64>()
                .unwrap();
            if visible != snapshot.captured_offset {
                mismatch = Some((visible, snapshot.captured_offset));
                break;
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        assert_eq!(
            mismatch, None,
            "rows and cursor must be from one generation"
        );
    }

    #[tokio::test]
    async fn read_live_log_tail_page_returns_runtime_chunks() {
        let runtime = make_runtime(
            "live-page123",
            SessionStatus::Running,
            "persisted line\n",
            Some(Duration::from_secs(5)),
        );
        {
            let mut rt = runtime.write();
            rt.engine = crate::terminal::Terminal::new(24, 80, 0);
            rt.feed_engine(b"\x1b[1;1Hscreen one\x1b[2;1Hscreen two\x1b[3;1Hscreen three");
        }
        let store = store_with(vec![runtime], make_test_db().await);

        let (chunks, total, offset, resizes) = store
            .read_live_log_tail_page("live-page123", 2)
            .await
            .expect("read live tail page");

        assert_eq!(
            chunks,
            vec![
                "screen two\x1b[0m\n".to_string(),
                "screen three\x1b[0m\n".to_string()
            ]
        );
        assert_eq!(total, 2);
        assert_eq!(offset, 0);
        assert!(resizes.is_empty());
    }

    #[tokio::test]
    async fn read_live_log_chunk_count_returns_visible_row_count() {
        let runtime = make_runtime(
            "live-count123",
            SessionStatus::Running,
            "persisted line\n",
            Some(Duration::from_secs(5)),
        );
        {
            let mut rt = runtime.write();
            rt.engine = crate::terminal::Terminal::new(24, 80, 0);
            rt.feed_engine(b"\x1b[1;1Hscreen one\x1b[2;1Hscreen two\x1b[3;1Hscreen three");
        }
        let store = store_with(vec![runtime], make_test_db().await);

        let total = store
            .read_live_log_chunk_count("live-count123")
            .await
            .expect("read live chunk count");

        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn update_session_metadata_updates_live_runtime_and_summary() {
        let (rt, _writer_rx) = make_runtime_writable("meta001", SessionStatus::Running);
        let db = make_test_db().await;
        db.insert_session(&rt.read().meta.clone())
            .await
            .expect("insert live session");
        let store = store_with(vec![rt.clone()], db);

        let summary = store
            .update_session_metadata(
                "meta001",
                Some("  Deploy ready  ".to_string()),
                Some(vec![
                    "prod".to_string(),
                    " Prod ".to_string(),
                    "".to_string(),
                ]),
                Some(false),
            )
            .await
            .expect("update live session metadata");

        assert_eq!(summary.title.as_deref(), Some("Deploy ready"));
        assert_eq!(summary.tags, vec!["prod".to_string()]);
        let locked = rt.read();
        assert_eq!(locked.meta.title.as_deref(), Some("Deploy ready"));
        assert_eq!(locked.meta.tags, vec!["prod".to_string()]);
        assert!(!locked.notifications_enabled);
    }

    #[tokio::test]
    async fn update_session_metadata_updates_persisted_session() {
        let db = make_test_db().await;
        let meta = SessionMeta {
            id: "meta002".to_string(),
            title: Some("old".to_string()),
            tags: vec!["old".to_string()],
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: None,
            resume_command: None,
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
            notifications_enabled: true,
            foreground_color: Some("rgb:ffff/ffff/ffff".to_string()),
            background_color: Some("#1e1e1e".to_string()),
        };
        db.insert_session(&meta)
            .await
            .expect("insert persisted session");
        let store = store_with(Vec::new(), db.clone());

        let summary = store
            .update_session_metadata(
                "meta002",
                Some("new".to_string()),
                Some(vec![" release ".to_string()]),
                None,
            )
            .await
            .expect("update persisted session metadata");

        assert_eq!(summary.title, Some("new".to_string()));
        assert_eq!(summary.tags, vec!["release".to_string()]);
        let saved = db
            .get_session("meta002")
            .await
            .expect("load saved session")
            .expect("session should exist");
        assert_eq!(saved.title, Some("new".to_string()));
        assert_eq!(saved.tags, vec!["release".to_string()]);
        // Terminal-reported colours survive the insert/update/read roundtrip.
        assert_eq!(
            saved.foreground_color.as_deref(),
            Some("rgb:ffff/ffff/ffff")
        );
        assert_eq!(saved.background_color.as_deref(), Some("#1e1e1e"));
    }

    #[tokio::test]
    async fn update_session_metadata_clears_explicit_empty_values() {
        let db = make_test_db().await;
        let meta = SessionMeta {
            id: "meta003".to_string(),
            title: Some("old".to_string()),
            tags: vec!["old".to_string()],
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: None,
            resume_command: None,
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
            notifications_enabled: true,
            foreground_color: None,
            background_color: None,
        };
        db.insert_session(&meta)
            .await
            .expect("insert persisted session");
        let store = store_with(Vec::new(), db.clone());

        let summary = store
            .update_session_metadata(
                "meta003",
                Some("   ".to_string()),
                Some(vec!["".to_string(), "   ".to_string()]),
                None,
            )
            .await
            .expect("clear persisted session metadata");

        assert_eq!(summary.title, None);
        assert!(summary.tags.is_empty());
        let saved = db
            .get_session("meta003")
            .await
            .expect("load saved session")
            .expect("session should exist");
        assert_eq!(saved.title, None);
        assert!(saved.tags.is_empty());
    }

    #[tokio::test]
    async fn update_session_metadata_ignores_omitted_fields() {
        let db = make_test_db().await;
        let meta = SessionMeta {
            id: "meta004".to_string(),
            title: Some("keep".to_string()),
            tags: vec!["keep".to_string()],
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: None,
            resume_command: None,
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
            notifications_enabled: true,
            foreground_color: None,
            background_color: None,
        };
        db.insert_session(&meta)
            .await
            .expect("insert persisted session");
        let store = store_with(Vec::new(), db.clone());

        let summary = store
            .update_session_metadata("meta004", None, None, None)
            .await
            .expect("ignore omitted metadata");

        assert_eq!(summary.title.as_deref(), Some("keep"));
        assert_eq!(summary.tags, vec!["keep".to_string()]);
        let saved = db
            .get_session("meta004")
            .await
            .expect("load saved session")
            .expect("session should exist");
        assert_eq!(saved.title.as_deref(), Some("keep"));
        assert_eq!(saved.tags, vec!["keep".to_string()]);
    }

    #[tokio::test]
    async fn update_session_metadata_rejects_notification_change_for_stopped_session() {
        let db = make_test_db().await;
        let meta = SessionMeta {
            id: "meta005".to_string(),
            title: Some("keep".to_string()),
            tags: vec!["keep".to_string()],
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: Some(Utc::now()),
            resume_command: None,
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
            notifications_enabled: true,
            foreground_color: None,
            background_color: None,
        };
        db.insert_session(&meta)
            .await
            .expect("insert persisted session");
        let store = store_with(Vec::new(), db);

        let error = store
            .update_session_metadata("meta005", None, None, Some(false))
            .await
            .expect_err("stopped sessions cannot change notification state");
        assert_eq!(
            error.to_string(),
            "protocol error: session not running: meta005"
        );
    }
}
