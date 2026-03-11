use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use bytes::Bytes;
use chrono::Utc;
use tokio::sync::broadcast;
use tracing::{debug, trace, warn};

use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    protocol::{ListQuery, SessionSummary},
    session::SessionLiveSummary,
};

use super::{
    SessionLookupError, SessionMeta, SessionStatus, StartSpec,
    persist::{append_event, append_resize_event, current_output_offset, format_age},
    runtime::{SessionRuntime, generate_session_id, spawn_session},
};

pub struct SessionStore {
    sessions: HashMap<String, Arc<Mutex<SessionRuntime>>>,
    evicted_sessions: HashMap<String, Instant>,
    eviction_ttl: Duration,
    db: Arc<Database>,
}

#[derive(Debug, Clone)]
pub struct SilentCandidate {
    pub session_id: String,
    pub raw_excerpt: String,
    pub output_epoch: Instant,
    pub notifications_enabled: bool,
}

impl SessionStore {
    pub fn new(eviction_seconds: u64, db: Arc<Database>) -> Self {
        Self {
            sessions: HashMap::new(),
            evicted_sessions: HashMap::new(),
            eviction_ttl: Duration::from_secs(eviction_seconds.max(1)),
            db,
        }
    }

    /// Load session history from the SQLite database on daemon startup.
    ///
    /// Any stale `running` / `stopping` sessions are reconciled to `failed`,
    /// persisted back to SQLite, and returned so callers can emit user-facing
    /// startup notifications.
    pub async fn load_running_stopping_sessions(&mut self) -> Vec<SessionMeta> {
        let db = self.db.clone();

        let mut startup_failed = Vec::new();

        match db
            .load_sessions_with_status(&[SessionStatus::Running, SessionStatus::Stopping])
            .await
        {
            Ok(rows) => {
                for (_, mut meta) in rows {
                    meta.status = SessionStatus::Failed;
                    meta.exit_code = None;
                    if let Err(err) = db.update_session(&meta).await {
                        tracing::warn!(
                            %err,
                            session_id = %meta.id,
                            "failed to persist startup stale-session reconciliation"
                        );
                    }
                    startup_failed.push(meta);
                }
            }
            Err(err) => {
                tracing::warn!(%err, "failed to load startup stale-status sessions from DB");
            }
        }

        startup_failed
    }

    pub async fn start_session(&mut self, config: &AppConfig, spec: StartSpec) -> Result<String> {
        let running_count = self
            .sessions
            .values()
            .filter(|s| {
                s.lock()
                    .map(|rt| matches!(rt.meta.status, SessionStatus::Running))
                    .unwrap_or(false)
            })
            .count();

        if running_count >= config.max_running_sessions {
            return Err(AppError::MaxSessionsReached(config.max_running_sessions));
        }

        let id = generate_session_id(|candidate| self.sessions.contains_key(candidate));

        let rows = spec.rows.unwrap_or(24).max(1);
        let cols = spec.cols.unwrap_or(80).max(1);
        let created_at = Utc::now();

        let mut meta = SessionMeta {
            id: id.clone(),
            title: spec.title,
            command: spec.cmd,
            args: spec.args,
            cwd: spec.cwd,
            created_at,
            started_at: Some(created_at),
            ended_at: None,
            status: SessionStatus::Running,
            pid: None,
            exit_code: None,
        };

        self.db.insert_session(&meta).await?;

        let session_dir = config.sessions_dir.join(&id);
        let runtime = spawn_session(
            config,
            &mut meta,
            session_dir,
            rows,
            cols,
            spec.notifications_enabled,
        )?;

        self.db.update_session(&meta).await?;

        self.sessions.insert(id.clone(), runtime);

        Ok(id)
    }

    pub async fn list_summaries(&mut self, query: &ListQuery) -> Result<Vec<SessionSummary>> {
        let mut sessions = self.db.list_summaries(query).await?;

        self.prune_evicted_sessions().await;
        for runtime in self.sessions.values() {
            if let Ok(mut rt) = runtime.lock() {
                if let Some(session) = sessions.iter_mut().find(|s| s.id == rt.meta.id) {
                    rt.refresh_status();

                    session.input_needed = matches!(rt.meta.status, super::SessionStatus::Running)
                        && rt.notified_output_epoch.is_some()
                        && rt.notified_output_epoch == rt.last_output_at;
                }
            } else {
                warn!("failed to lock session runtime for summary refresh");
            }
        }

        Ok(sessions)
    }

    pub fn get_summary(&mut self, id: &str) -> Option<SessionSummary> {
        let runtime = self.sessions.get(id)?;
        let mut rt = runtime.lock().ok()?;
        rt.refresh_status();
        let input_needed = matches!(rt.meta.status, super::SessionStatus::Running)
            && rt.notified_output_epoch.is_some()
            && rt.notified_output_epoch == rt.last_output_at;
        Some(SessionSummary {
            id: rt.meta.id.clone(),
            title: rt.meta.title.clone(),
            command: rt.meta.command.clone(),
            args: rt.meta.args.clone(),
            pid: rt.meta.pid,
            status: rt.meta.status.as_str().to_string(),
            age: format_age(rt.meta.created_at, rt.meta.started_at, rt.meta.ended_at),
            created_at: rt.meta.created_at,
            cwd: rt.meta.cwd.clone(),
            input_needed,
        })
    }

    /// Returns summaries for all sessions that are currently held in memory
    /// (live or recently evicted), without touching the database.
    /// Used by the SSE session poller to avoid a DB query every 500 ms.
    pub fn list_live_summaries(&mut self) -> Vec<SessionLiveSummary> {
        let mut out = Vec::with_capacity(self.sessions.len());
        for runtime in self.sessions.values() {
            if let Ok(mut rt) = runtime.lock() {
                rt.refresh_status();
                let input_needed = matches!(rt.meta.status, super::SessionStatus::Running)
                    && rt.notified_output_epoch.is_some()
                    && rt.notified_output_epoch == rt.last_output_at;
                out.push(SessionLiveSummary {
                    last_output_at: rt.last_output_at,
                    summary: SessionSummary {
                        id: rt.meta.id.clone(),
                        title: rt.meta.title.clone(),
                        command: rt.meta.command.clone(),
                        args: rt.meta.args.clone(),
                        pid: rt.meta.pid,
                        status: rt.meta.status.as_str().to_string(),
                        age: format_age(rt.meta.created_at, rt.meta.started_at, rt.meta.ended_at),
                        created_at: rt.meta.created_at,
                        cwd: rt.meta.cwd.clone(),
                        input_needed,
                    },
                });
            } else {
                tracing::warn!("failed to lock session runtime in list_live_summaries");
            }
        }
        out
    }

    pub fn get_exit_code(&self, id: &str) -> Option<i32> {
        let runtime = self.sessions.get(id)?;
        let rt = runtime.lock().ok()?;
        rt.meta.exit_code
    }

    pub fn is_running(&self, id: &str) -> bool {
        let Some(runtime) = self.sessions.get(id) else {
            return false;
        };
        let Ok(rt) = runtime.lock() else {
            return false;
        };
        matches!(rt.meta.status, super::SessionStatus::Running)
    }

    /// Returns the current terminal mode snapshot for the session, if available.
    pub fn get_mode_snapshot(
        &self,
        id: &str,
    ) -> Option<crate::session::mode_tracker::ModeSnapshot> {
        let runtime = self.sessions.get(id)?;
        let rt = runtime.lock().ok()?;
        Some(rt.mode_snapshot())
    }

    pub async fn attach_stream_status(
        &mut self,
        id: &str,
    ) -> std::result::Result<(bool, bool, Option<i32>), SessionLookupError> {
        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::Evicted);
        };
        rt.refresh_status();
        Ok((
            matches!(rt.meta.status, super::SessionStatus::Running),
            rt.output_closed,
            rt.meta.exit_code,
        ))
    }

    pub async fn mark_attach_presence(&mut self, id: &str) {
        if let Some(runtime) = self.sessions.get(id) {
            if let Ok(mut rt) = runtime.lock() {
                rt.mark_attach_presence();
            }
        }
    }

    /// Initialise an attach subscription: return the ring content since
    /// `from_byte_offset` (or all content if `None`), the current end offset,
    /// a live broadcast receiver, and the current terminal mode flags.
    pub async fn attach_subscribe_init(
        &mut self,
        id: &str,
        from_byte_offset: Option<u64>,
    ) -> std::result::Result<
        (
            Vec<(u64, Bytes)>,
            u64,
            broadcast::Receiver<Arc<Bytes>>,
            bool,
            bool,
        ),
        SessionLookupError,
    > {
        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::Evicted);
        };
        rt.refresh_status();
        let offset = from_byte_offset.unwrap_or(0);
        let (chunks, end_offset) = rt.ring.read_from(offset);
        let rx = rt.broadcast_tx.subscribe();
        let modes = rt.mode_snapshot();
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

    pub async fn attach_detach(&mut self, id: &str) -> std::result::Result<(), SessionLookupError> {
        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::Evicted);
        };
        rt.clear_attach_state();
        debug!(session_id = id, "attach detach acknowledged");
        Ok(())
    }

    pub async fn attach_input(
        &mut self,
        id: &str,
        data: &str,
    ) -> std::result::Result<(), SessionLookupError> {
        // Avoid sending lose focus escape sequence which will cause other clients not able to input anything
        if data == "\x1b[O" {
            return Ok(());
        }

        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::Evicted);
        };
        // When the child process has enabled DECCKM (application cursor key
        // mode via `\x1b[?1h`), arrow key sequences must use `\x1bO` prefix
        // instead of `\x1b[`.  Transform transparently here so both
        // `oly attach` and `oly input` always work, regardless of whether the
        // caller tracks DECCKM state itself.
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
            cooked.as_bytes()
        } else {
            data.as_bytes()
        };
        if rt.pty.write_input(bytes.to_vec()) {
            rt.mark_attach_activity();
            rt.last_input_at = Some(Instant::now());
            debug!(
                session_id = id,
                bytes = bytes.len(),
                transformed,
                app_cursor_keys = modes.app_cursor_keys,
                "attach input forwarded"
            );
            Ok(())
        } else {
            debug!(
                session_id = id,
                bytes = bytes.len(),
                "attach input failed while writing to PTY"
            );
            Err(SessionLookupError::Evicted)
        }
    }

    pub async fn attach_resize(
        &mut self,
        id: &str,
        rows: u16,
        cols: u16,
    ) -> std::result::Result<(), SessionLookupError> {
        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::Evicted);
        };
        rt.mark_attach_activity();
        let resized = rt.resize_pty(rows, cols);
        debug!(
            session_id = id,
            rows, cols, resized, "attach resize requested"
        );
        if resized {
            let offset = current_output_offset(&rt.dir);
            let _ = append_resize_event(&rt.dir, offset, rows, cols);
            Ok(())
        } else {
            Err(SessionLookupError::Evicted)
        }
    }

    pub async fn stop_session(&mut self, id: &str, grace_seconds: u64) -> bool {
        self.prune_evicted_sessions().await;

        let Some(runtime) = self.sessions.get(id).cloned() else {
            return false;
        };

        // Send Ctrl-C and mark as stopping.
        {
            let Ok(mut rt) = runtime.lock() else {
                return false;
            };
            rt.meta.status = SessionStatus::Stopping;
            let _ = rt.pty.write_input(vec![0x03]);
        }

        let deadline = Instant::now() + Duration::from_secs(grace_seconds);
        while Instant::now() < deadline {
            {
                let Ok(mut rt) = runtime.lock() else {
                    return true;
                };
                if rt.refresh_status() {
                    return true;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let Ok(mut rt) = runtime.lock() else {
            return false;
        };
        if rt.pty.kill().is_ok() {
            let _ = rt.refresh_status();
            true
        } else {
            false
        }
    }

    pub async fn stop_all_sessions(&mut self, grace_seconds: u64) -> bool {
        self.prune_evicted_sessions().await;
        let ids: Vec<String> = self.sessions.keys().cloned().collect();
        let mut all_stopped = true;
        for id in ids {
            if !self.stop_session(&id, grace_seconds).await {
                all_stopped = false;
            }
        }
        all_stopped
    }

    pub async fn logs_snapshot(
        &mut self,
        id: &str,
        tail: usize,
    ) -> Option<(Vec<String>, u64, bool)> {
        self.prune_evicted_sessions().await;

        let runtime = self.sessions.get(id)?;
        let mut rt = runtime.lock().ok()?;
        rt.refresh_status();
        let mut filter = crate::session::pty::EscapeFilter::new();
        let all_bytes: Vec<u8> = rt
            .ring
            .all_chunks()
            .flat_map(|b| filter.filter(b))
            .collect();
        let text = String::from_utf8_lossy(&all_bytes);
        let all_lines: Vec<String> = text.lines().map(|l| format!("{l}\n")).collect();
        let skip = all_lines.len().saturating_sub(tail);
        let lines: Vec<String> = all_lines.into_iter().skip(skip).collect();
        let cursor = rt.ring.end_offset();
        let running = rt.meta.status.as_str() == "running";
        Some((lines, cursor, running))
    }

    pub async fn logs_poll(&mut self, id: &str, cursor: u64) -> Option<(Vec<String>, u64, bool)> {
        self.prune_evicted_sessions().await;

        let runtime = self.sessions.get(id)?;
        let mut rt = runtime.lock().ok()?;
        rt.refresh_status();
        let (chunks, end_offset) = rt.ring.read_from(cursor);
        let mut filter = crate::session::pty::EscapeFilter::new();
        let raw: Vec<u8> = chunks.iter().flat_map(|(_, b)| filter.filter(b)).collect();
        let text = String::from_utf8_lossy(&raw);
        let lines: Vec<String> = text.lines().map(|l| format!("{l}\n")).collect();
        let running = rt.meta.status.as_str() == "running";
        Some((lines, end_offset, running))
    }

    async fn lookup_runtime(
        &mut self,
        id: &str,
    ) -> std::result::Result<Arc<Mutex<SessionRuntime>>, SessionLookupError> {
        self.prune_evicted_sessions().await;

        if let Some(runtime) = self.sessions.get(id) {
            trace!(
                session_id = id,
                "session runtime lookup hit in-memory runtime"
            );
            return Ok(runtime.clone());
        }

        if self.evicted_sessions.contains_key(id) {
            debug!(
                session_id = id,
                "session runtime lookup hit evicted tombstone"
            );
            return Err(SessionLookupError::Evicted);
        }

        debug!(session_id = id, "session runtime lookup missed");
        Err(SessionLookupError::NotRunning)
    }

    async fn prune_evicted_sessions(&mut self) {
        let now = Instant::now();
        let mut to_persist: Vec<SessionMeta> = Vec::new();
        let mut evicted_ids: Vec<String> = Vec::new();

        self.sessions.retain(|id, runtime| {
            let Ok(mut rt) = runtime.lock() else {
                return true;
            };
            rt.refresh_status();

            // Write completed-but-not-yet-persisted sessions to the DB.
            if rt.is_completed() && !rt.persisted {
                to_persist.push(rt.meta.clone());
                rt.persisted = true;
            }

            if !rt.is_completed() {
                return true;
            }
            let Some(completed_at) = rt.completed_at else {
                rt.completed_at = Some(now);
                return true;
            };
            if now.duration_since(completed_at) >= self.eviction_ttl {
                let _ = append_event(&rt.dir, "session evicted from memory");
                evicted_ids.push(id.clone());
                return false;
            }
            true
        });

        // Persist completed sessions outside the borrow of `self.sessions`.
        for meta in to_persist {
            debug!(session_id = %meta.id, status = meta.status.as_str(), "persisting completed session metadata");
            if let Err(err) = self.db.update_session(&meta).await {
                tracing::error!(%err, session_id = meta.id, "failed to persist completed session");
            }
        }

        for id in evicted_ids {
            debug!(session_id = %id, "session evicted from in-memory store");
            self.evicted_sessions.insert(id, now);
        }

        self.evict_old_tombstones(now);
    }

    fn evict_old_tombstones(&mut self, now: Instant) {
        self.evicted_sessions
            .retain(|_, evicted_at| now.duration_since(*evicted_at) < self.eviction_ttl);
    }

    /// Returns silent-notification candidates with session id, raw excerpt,
    /// and output epoch.
    pub fn silent_candidates(
        &self,
        suppression_window: Duration,
        min_notification_interval: Duration,
    ) -> Vec<SilentCandidate> {
        let now = Instant::now();
        self.sessions
            .values()
            .filter_map(|runtime| {
                let mut rt = runtime.lock().ok()?;
                if !matches!(rt.meta.status, super::SessionStatus::Running) {
                    return None;
                }

                // Suppress notification untill output advances
                // if there was attach activity in recent supression window.
                // Because normally every input may cause some output, which should not be notified in short time.
                if let Some(last_attach_activity) = rt.last_attach_activity_at {
                    if now.duration_since(last_attach_activity) < suppression_window {
                        rt.last_output_at = None;
                        return None;
                    }
                }

                // Silence condition: no visible output for `silence` duration.
                let last_output = rt.last_output_at?;

                // Drop short-age candidates to prevent repeated alerts in a
                // short interval even when monitor ticks every second.
                if let Some(last_notified_at) = rt.last_notified_at {
                    if now.duration_since(last_notified_at) < min_notification_interval {
                        return None;
                    }
                }
                // Already notified for this exact output epoch.
                // Suppress until new visible output advances the epoch.
                if rt.notified_output_epoch == Some(last_output) {
                    return None;
                }

                let limit = 20u16;
                let mut parser = vt100::Parser::new(limit, 2000, 0);
                let chunks: Vec<_> = rt.ring.all_chunks().collect();
                for chunk in chunks.iter().rev().take(limit as usize).rev() {
                    parser.process(chunk);
                }
                let excerpt = parser.screen().contents();

                Some(SilentCandidate {
                    session_id: rt.meta.id.clone(),
                    raw_excerpt: excerpt,
                    output_epoch: last_output,
                    notifications_enabled: rt.notifications_enabled,
                })
            })
            .collect()
    }

    /// Records a successful notification for `session_id` at `output_epoch`.
    /// Re-notification is suppressed until output advances to a new epoch.
    pub fn mark_notified(&mut self, session_id: &str, output_epoch: Instant, notified_at: Instant) {
        if let Some(runtime) = self.sessions.get(session_id) {
            if let Ok(mut rt) = runtime.lock() {
                rt.notified_output_epoch = Some(output_epoch);
                rt.last_notified_at = Some(notified_at);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionMeta, SessionStatus};
    use chrono::Utc;
    use std::sync::{Arc, Mutex};

    fn make_runtime(
        id: &str,
        status: SessionStatus,
        excerpt: &str,
        last_output_ago: Option<Duration>,
    ) -> Arc<Mutex<super::super::runtime::SessionRuntime>> {
        use crate::session::ring::RingBuffer;
        use bytes::Bytes;
        use tokio::sync::{broadcast, mpsc};

        let dir = std::env::temp_dir().join(format!("oly_store_test_{id}"));

        let meta = SessionMeta {
            id: id.to_string(),
            title: None,
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: None,
            status,
            pid: None,
            exit_code: None,
        };

        let last_output_at = last_output_ago.map(|ago| Instant::now() - ago);

        let mut ring = RingBuffer::new(4096);
        if !excerpt.is_empty() {
            ring.push(Bytes::from(excerpt.to_string()));
        }

        let (broadcast_tx, _rx) = broadcast::channel(4);
        let (writer_tx, _writer_rx) = mpsc::unbounded_channel();
        let (child, pty_master) = make_dummy_child();
        Arc::new(Mutex::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            ring,
            broadcast_tx,
            pty: super::super::pty::PtyHandle {
                child,
                writer_tx,
                pty_master: Some(pty_master),
            },
            completed_at: None,
            persisted: false,
            last_output_at,
            last_input_at: None,
            last_attach_presence_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            mode_tracker: super::super::mode_tracker::ModeTracker::new(),
            output_closed: false,
            notifications_enabled: true,
        }))
    }

    fn make_dummy_child() -> (
        super::super::pty::RuntimeChild,
        Box<dyn portable_pty::MasterPty + Send>,
    ) {
        // Spawn a long-running process so refresh_status() sees it still alive.
        // We must also return the PTY master to keep the child alive — dropping
        // the master sends EOF/HUP to the child, which would cause it to exit.
        #[cfg(target_os = "windows")]
        let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
        #[cfg(target_os = "windows")]
        cmd.args(["/c", "ping", "127.0.0.1", "-n", "120"]);
        #[cfg(not(target_os = "windows"))]
        let mut cmd = portable_pty::CommandBuilder::new("sleep");
        #[cfg(not(target_os = "windows"))]
        cmd.args(["60"]);

        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty in test");
        let child = pty.slave.spawn_command(cmd).expect("spawn in test");
        (super::super::pty::RuntimeChild::Pty(child), pty.master)
    }

    async fn make_test_db() -> Arc<Database> {
        // Use a unique per-test file-based DB in the temp directory so
        // concurrent tests don't interfere with each other.
        let path = std::env::temp_dir().join(format!(
            "oly_test_{}.db",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        Arc::new(
            Database::open(&path, std::env::temp_dir())
                .await
                .expect("open test DB"),
        )
    }

    fn store_with(
        runtimes: Vec<Arc<Mutex<super::super::runtime::SessionRuntime>>>,
        db: Arc<Database>,
    ) -> SessionStore {
        let mut store = SessionStore::new(900, db);
        for rt in runtimes {
            let id = rt.lock().unwrap().meta.id.clone();
            store.sessions.insert(id, rt);
        }
        store
    }

    // -----------------------------------------------------------------------
    // silent_candidates
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_silent_candidates_returns_running_past_silence() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        // last output was 10s ago → past silence
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "password: ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "abc1234");
    }

    #[tokio::test]
    async fn test_silent_candidates_allows_recent_output_when_not_suppressed() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        // Current implementation only requires an output epoch to exist.
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_millis(500)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "abc1234");
    }

    #[tokio::test]
    async fn test_silent_candidates_ignores_non_running_session() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Stopped,
            "prompt> ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_ignores_no_output_yet() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime("abc1234", SessionStatus::Running, "prompt> ", None);
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    // -----------------------------------------------------------------------
    // mark_notified + output-epoch gating
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_mark_notified_suppresses_until_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let mut store = store_with(vec![rt], make_test_db().await);

        // First call returns a candidate with an output epoch.
        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;

        // Mark as notified at this output epoch.
        store.mark_notified(id, epoch, Instant::now());

        // Second call: same output epoch → suppressed.
        let second = store.silent_candidates(silence, min_interval);
        assert!(
            second.is_empty(),
            "should suppress re-notification at same output epoch"
        );
    }

    #[tokio::test]
    async fn test_mark_notified_allows_after_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let mut store = store_with(vec![rt], make_test_db().await);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;
        store.mark_notified(id, epoch, Instant::now());

        // Simulate new output by advancing last_output_at on the runtime.
        {
            let runtime = store.sessions.get("abc1234").unwrap();
            let mut rt = runtime.lock().unwrap();
            // A new epoch strictly later than the notified one.
            rt.last_output_at = Some(Instant::now());
            // Move notification timestamp into the past so cooldown no longer blocks.
            rt.last_notified_at = Some(Instant::now() - Duration::from_secs(30));
        }

        // New output epoch + expired notification cooldown should re-qualify.
        let after_output = store.silent_candidates(silence, min_interval);
        assert_eq!(after_output.len(), 1);
    }

    #[tokio::test]
    async fn test_mark_notified_stays_suppressed_without_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let mut store = store_with(vec![rt], make_test_db().await);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;
        store.mark_notified(id, epoch, Instant::now());

        // Same output epoch -> suppressed.
        let suppressed = store.silent_candidates(silence, min_interval);
        assert!(suppressed.is_empty());

        // Simulate time passing without any new output.
        {
            let runtime = store.sessions.get("abc1234").unwrap();
            let mut rt = runtime.lock().unwrap();
            rt.last_notified_at = Some(Instant::now() - Duration::from_secs(31));
        }

        let still_suppressed = store.silent_candidates(silence, min_interval);
        assert!(
            still_suppressed.is_empty(),
            "should remain suppressed until new output advances epoch"
        );
    }

    #[tokio::test]
    async fn test_mark_notified_on_unknown_id_is_noop() {
        let mut store = SessionStore::new(900, make_test_db().await);
        // Should not panic.
        let now = Instant::now();
        store.mark_notified("does_not_exist", now, now);
    }

    #[tokio::test]
    async fn test_silent_candidates_suppressed_during_recent_attach_activity() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)), // output 5s ago (past silence)
        );
        // Recent attach activity should suppress notifications and clear output epoch.
        {
            let mut locked = rt.lock().unwrap();
            locked.last_attach_activity_at = Some(Instant::now());
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should suppress notification while attach activity is inside suppression window"
        );

        let runtime = store.sessions.get("abc1234").unwrap();
        let locked = runtime.lock().unwrap();
        assert!(
            locked.last_output_at.is_none(),
            "suppression path should clear output epoch until a new output arrives"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_drops_short_age_notifications() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        {
            let mut locked = rt.lock().unwrap();
            locked.last_notified_at = Some(Instant::now() - Duration::from_secs(3));
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should drop candidates inside cooldown window"
        );
    }

    // -----------------------------------------------------------------------
    // attach_input — data forwarding and last_input_at tracking
    // -----------------------------------------------------------------------

    fn make_runtime_writable(
        id: &str,
        status: SessionStatus,
    ) -> (
        Arc<Mutex<super::super::runtime::SessionRuntime>>,
        tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        use crate::session::ring::RingBuffer;
        use tokio::sync::{broadcast, mpsc};

        let dir = std::env::temp_dir().join(format!("oly_store_writable_{id}"));
        let meta = SessionMeta {
            id: id.to_string(),
            title: None,
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: None,
            status,
            pid: None,
            exit_code: None,
        };
        let (broadcast_tx, _rx) = broadcast::channel(4);
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let (child, pty_master) = make_dummy_child();
        let rt = Arc::new(Mutex::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            ring: RingBuffer::new(4096),
            broadcast_tx,
            pty: super::super::pty::PtyHandle {
                child,
                writer_tx,
                pty_master: Some(pty_master),
            },
            completed_at: None,
            persisted: false,
            last_output_at: None,
            last_input_at: None,
            last_attach_presence_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            mode_tracker: super::super::mode_tracker::ModeTracker::new(),
            output_closed: false,
            notifications_enabled: true,
        }));
        (rt, writer_rx)
    }

    fn make_test_config(max_running_sessions: usize) -> AppConfig {
        use std::path::PathBuf;
        AppConfig {
            ring_buffer_bytes: 4_194_304,
            silence_seconds: 10,
            stop_grace_seconds: 5,
            session_eviction_seconds: 15,
            http_port: 0,
            log_level: "info".into(),
            prompt_patterns: vec![],
            web_push_vapid_public_key: None,
            web_push_vapid_private_key: None,
            web_push_subject: None,
            state_dir: PathBuf::from("."),
            sessions_dir: PathBuf::from("."),
            db_file: PathBuf::from("."),
            socket_name: "test.sock".into(),
            socket_file: PathBuf::from("."),
            lock_file: PathBuf::from("."),
            max_running_sessions,
            notification_hook: None,
        }
    }

    #[tokio::test]
    async fn test_start_session_enforces_limit() {
        let config = make_test_config(1);
        // Create 1 running session
        let rt = make_runtime("s1", SessionStatus::Running, "", None);
        let mut store = store_with(vec![rt], make_test_db().await);

        // Try to start a 2nd session
        let spec = StartSpec {
            title: None,
            cmd: "echo".into(),
            args: vec![],
            cwd: None,
            rows: None,
            cols: None,
            notifications_enabled: true,
        };

        let result = store.start_session(&config, spec).await;

        // Assert it fails with MaxSessionsReached
        assert!(result.is_err());
        match result {
            Err(crate::error::AppError::MaxSessionsReached(limit)) => {
                assert_eq!(limit, 1);
            }
            _ => panic!("Expected MaxSessionsReached error, got {:?}", result),
        }
    }

    // -----------------------------------------------------------------------
    // attach_input — data forwarding and last_input_at tracking
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_input_writes_data_to_writer() {
        let (rt, mut writer_rx) = make_runtime_writable("inp0001", SessionStatus::Running);
        let mut store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0001", "hello\r")
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
        let mut store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0002", "x")
            .await
            .expect("attach_input should succeed");

        let locked = rt_clone.lock().unwrap();
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
            let mut locked = rt.lock().unwrap();
            locked.mode_tracker.process(b"\x1b[?1h");
        }
        let mut store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0003", "\x1b[A")
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
            let mut locked = rt.lock().unwrap();
            locked.mode_tracker.process(b"\x1b[?1h");
        }
        let mut store = store_with(vec![rt], make_test_db().await);

        // Send all four arrow sequences at once.
        store
            .attach_input("inp0004", "\x1b[A\x1b[B\x1b[C\x1b[D")
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
        let mut store = store_with(vec![rt], make_test_db().await);

        store
            .attach_input("inp0005", "\x1b[A\x1b[B")
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
        let mut store = SessionStore::new(900, make_test_db().await);
        let result = store.attach_input("no_such_id", "data").await;
        assert!(
            result.is_err(),
            "attach_input to unknown session should return an error"
        );
    }

    // -----------------------------------------------------------------------
    // attach_detach
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_detach_clears_presence_and_activity() {
        let rt = make_runtime("detach001", SessionStatus::Running, "$ prompt", None);
        let rt_clone = rt.clone();
        let mut store = store_with(vec![rt], make_test_db().await);

        {
            let mut locked = rt_clone.lock().unwrap();
            locked.mark_attach_activity();
        }

        store
            .attach_detach("detach001")
            .await
            .expect("detach should succeed");

        let locked = rt_clone.lock().unwrap();
        assert!(
            locked.last_attach_presence_at.is_none(),
            "detach should clear attach presence"
        );
        assert!(
            locked.last_attach_activity_at.is_none(),
            "detach should clear attach activity"
        );
    }
}
