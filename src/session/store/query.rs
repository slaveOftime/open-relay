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
    session::{SessionEvent, SessionLiveSummary, validate_session_metadata_update},
};

use super::super::SessionError;
use super::super::logs::split_rendered_log_output;
use super::SessionStore;

impl SessionStore {
    pub async fn list_summaries(&self, query: &ListQuery) -> Result<Vec<SessionSummary>> {
        let mut sessions = self.db.list_summaries(query).await?;

        let live_sessions = self.sessions.load();
        for session in &mut sessions {
            if let Some(handle) = live_sessions.get(&session.id) {
                *session = handle.read().to_summary();
            }
        }

        Ok(sessions)
    }

    pub fn get_summary(&self, id: &str) -> Option<SessionSummary> {
        let sessions = self.sessions.load();
        sessions.get(id).map(|handle| handle.read().to_summary())
    }

    /// Returns summaries for all sessions that are currently held in memory
    /// (live or recently evicted), without touching the database.
    /// Used by the SSE session poller to avoid a DB query every 500 ms.
    pub fn list_live_summaries(&self) -> Vec<SessionLiveSummary> {
        let sessions = self.sessions.load();
        sessions
            .values()
            .map(|handle| {
                let rt = handle.read();
                SessionLiveSummary {
                    last_output_at: rt.last_output_epoch,
                    summary: rt.to_summary(),
                }
            })
            .collect()
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

    pub fn is_input_needed(&self, id: &str) -> bool {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .map(|handle| handle.read().input_needed())
            .unwrap_or(false)
    }

    pub fn is_silent_for(&self, id: &str, duration: std::time::Duration) -> bool {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .map(|handle| {
                handle
                    .read()
                    .last_output_epoch
                    .map(|last_output| {
                        std::time::Instant::now().duration_since(last_output) >= duration
                    })
                    .unwrap_or(true)
            })
            .unwrap_or(true)
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
    pub fn shared_modes(&self, id: &str) -> Option<Arc<crate::session::SharedModes>> {
        let sessions = self.sessions.load();
        sessions
            .get(id)
            .map(|handle| Arc::clone(&handle.read().shared_modes))
    }

    pub async fn render_live_logs(
        &self,
        id: &str,
        tail: usize,
        keep_color: bool,
        term_cols: u16,
    ) -> std::result::Result<(Vec<u8>, Vec<crate::protocol::LogResize>), SessionError> {
        let handle = self.lookup_runtime(id).await?;
        let rt = handle.read();
        if rt.is_completed() || rt.output_closed {
            return Err(SessionError::NotRunning);
        }
        Ok((
            rt.render_logs(tail, keep_color, term_cols),
            rt.resize_history.clone(),
        ))
    }

    pub async fn read_live_log_tail_page(
        &self,
        id: &str,
        tail: usize,
    ) -> std::result::Result<
        (Vec<String>, usize, usize, Vec<crate::protocol::LogResize>),
        SessionError,
    > {
        let handle = self.lookup_runtime(id).await?;
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
        let chunks = split_rendered_log_output(&rt.render_logs(tail, true, term_cols));
        let total = chunks.len();

        Ok((chunks, total, 0, rt.resize_history.clone()))
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
        let mut rt = handle.write();
        if rt.is_completed() {
            return Err(SessionError::NotRunning);
        }
        rt.set_notifications_enabled(enabled);
        debug!(
            session_id = id,
            notifications_enabled = enabled,
            "session notification setting updated"
        );
        Ok(())
    }
}

#[cfg(test)]
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
            rt.screen_parser = vt100::Parser::new(24, 80, 0);
            rt.screen_parser
                .process(b"\x1b[1;1Hscreen one\x1b[2;1Hscreen two\x1b[3;1Hscreen three");
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
    async fn read_live_log_tail_page_returns_runtime_chunks() {
        let runtime = make_runtime(
            "live-page123",
            SessionStatus::Running,
            "persisted line\n",
            Some(Duration::from_secs(5)),
        );
        {
            let mut rt = runtime.write();
            rt.screen_parser = vt100::Parser::new(24, 80, 0);
            rt.screen_parser
                .process(b"\x1b[1;1Hscreen one\x1b[2;1Hscreen two\x1b[3;1Hscreen three");
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
            rt.screen_parser = vt100::Parser::new(24, 80, 0);
            rt.screen_parser
                .process(b"\x1b[1;1Hscreen one\x1b[2;1Hscreen two\x1b[3;1Hscreen three");
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
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
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
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
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
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
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
            status: SessionStatus::Stopped,
            pid: None,
            exit_code: Some(0),
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
