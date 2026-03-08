use std::{
    collections::HashMap,
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::Utc;
use tracing;

use crate::{
    config::AppConfig,
    db::Database,
    error::Result,
    protocol::{ListQuery, SessionSummary},
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
    /// Metadata for sessions that have been evicted from memory or loaded from DB.
    history: HashMap<String, SessionMeta>,
    /// SQLite database handle.  `None` in unit tests.
    db: Option<Arc<Database>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new(900)
    }
}

impl SessionStore {
    pub fn new(eviction_seconds: u64) -> Self {
        Self {
            sessions: HashMap::new(),
            evicted_sessions: HashMap::new(),
            eviction_ttl: Duration::from_secs(eviction_seconds.max(1)),
            history: HashMap::new(),
            db: None,
        }
    }

    /// Attach a database handle.  Call this once after construction in production.
    pub fn with_db(mut self, db: Arc<Database>) -> Self {
        self.db = Some(db);
        self
    }

    /// Load session history from the SQLite database on daemon startup.
    ///
    /// Any stale `running` / `stopping` sessions are reconciled to `failed`,
    /// persisted back to SQLite, and returned so callers can emit user-facing
    /// startup notifications.
    pub async fn load_running_stopping_sessions(&mut self) -> Vec<SessionMeta> {
        let Some(db) = self.db.clone() else {
            return Vec::new();
        };

        let mut startup_failed = Vec::new();

        match db
            .load_sessions_with_status(&[SessionStatus::Running, SessionStatus::Stopping])
            .await
        {
            Ok(rows) => {
                for (id, mut meta) in rows {
                    meta.status = SessionStatus::Failed;
                    meta.exit_code = None;
                    if let Err(err) = db.update_session(&meta).await {
                        tracing::warn!(
                            %err,
                            session_id = %meta.id,
                            "failed to persist startup stale-session reconciliation"
                        );
                    }
                    startup_failed.push(meta.clone());

                    if !self.sessions.contains_key(&id) {
                        self.history.insert(id, meta);
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, "failed to load startup stale-status sessions from DB");
            }
        }

        match db
            .load_sessions_with_status(&[
                SessionStatus::Created,
                SessionStatus::Stopped,
                SessionStatus::Failed,
            ])
            .await
        {
            Ok(rows) => {
                for (id, meta) in rows {
                    if !self.sessions.contains_key(&id) {
                        self.history.insert(id, meta);
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, "failed to load startup historical-status sessions from DB");
            }
        }

        startup_failed
    }

    pub async fn start_session(&mut self, config: &AppConfig, spec: StartSpec) -> Result<String> {
        let id = generate_session_id(|candidate| {
            self.sessions.contains_key(candidate) || self.history.contains_key(candidate)
        });

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

        let session_dir = config.sessions_dir.join(&id);
        let runtime = spawn_session(config, &mut meta, session_dir.clone(), rows, cols)?;
        self.sessions.insert(id.clone(), runtime);

        if let Some(db) = &self.db {
            if let Err(err) = db.insert_session(&meta).await {
                tracing::error!(%err, session_id = id, "failed to persist new session to DB");
            }
        }

        Ok(id)
    }

    pub async fn list_summaries(&mut self, query: &ListQuery) -> Vec<SessionSummary> {
        self.prune_evicted_sessions().await;

        let entries = self
            .sessions
            .values()
            .filter_map(|runtime| {
                let mut rt = runtime.lock().ok()?;
                rt.refresh_status();
                let input_needed = matches!(rt.meta.status, super::SessionStatus::Running)
                    && rt.notified_output_epoch.is_some()
                    && rt.notified_output_epoch == rt.last_output_at;
                Some((rt.meta.clone(), input_needed))
            })
            .collect::<Vec<_>>();

        let mut summaries = entries
            .into_iter()
            .map(|(meta, input_needed)| SessionSummary {
                id: meta.id,
                title: meta.title,
                command: meta.command,
                args: meta.args,
                pid: meta.pid,
                status: meta.status.as_str().to_string(),
                age: format_age(meta.created_at, meta.started_at, meta.ended_at),
                created_at: meta.created_at,
                cwd: meta.cwd,
                input_needed,
            })
            .collect::<Vec<_>>();

        // Also include sessions that were evicted from memory or loaded from disk.
        let extras: Vec<SessionSummary> = {
            let active_ids: std::collections::HashSet<&str> =
                summaries.iter().map(|s| s.id.as_str()).collect();
            self.history
                .values()
                .filter(|meta| !active_ids.contains(meta.id.as_str()))
                .map(|meta| SessionSummary {
                    id: meta.id.clone(),
                    title: meta.title.clone(),
                    command: meta.command.clone(),
                    args: meta.args.clone(),
                    pid: meta.pid,
                    status: meta.status.as_str().to_string(),
                    age: format_age(meta.created_at, meta.started_at, meta.ended_at),
                    created_at: meta.created_at,
                    cwd: meta.cwd.clone(),
                    input_needed: false,
                })
                .collect()
        };
        summaries.extend(extras);

        query.apply(summaries)
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

    pub fn get_exit_code(&self, id: &str) -> Option<i32> {
        let runtime = self.sessions.get(id)?;
        let rt = runtime.lock().ok()?;
        rt.meta.exit_code
    }

    pub async fn attach_snapshot(
        &mut self,
        id: &str,
    ) -> std::result::Result<(Vec<String>, usize, bool, bool, bool), SessionLookupError> {
        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::NotFound);
        };
        rt.refresh_status();
        let lines = rt.ring.iter().cloned().collect();
        let cursor = rt.output_lines.len();
        let running = rt.meta.status.as_str() == "running";
        let bracketed_paste_mode = rt.bracketed_paste_mode;
        let app_cursor_keys = rt.app_cursor_keys;
        Ok((
            lines,
            cursor,
            running,
            bracketed_paste_mode,
            app_cursor_keys,
        ))
    }

    pub async fn attach_poll(
        &mut self,
        id: &str,
        cursor: usize,
    ) -> std::result::Result<(Vec<String>, usize, bool, bool, bool), SessionLookupError> {
        let runtime = self.lookup_runtime(id).await?;
        let Ok(mut rt) = runtime.lock() else {
            return Err(SessionLookupError::NotFound);
        };
        rt.refresh_status();
        let lines = rt.output_lines.get(cursor..).unwrap_or(&[]).to_vec();
        let next_cursor = rt.output_lines.len();
        let running = rt.meta.status.as_str() == "running";
        let bracketed_paste_mode = rt.bracketed_paste_mode;
        let app_cursor_keys = rt.app_cursor_keys;
        Ok((
            lines,
            next_cursor,
            running,
            bracketed_paste_mode,
            app_cursor_keys,
        ))
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
            return Err(SessionLookupError::NotFound);
        };
        // When the child process has enabled DECCKM (application cursor key
        // mode via `\x1b[?1h`), arrow key sequences must use `\x1bO` prefix
        // instead of `\x1b[`.  Transform transparently here so both
        // `oly attach` and `oly input` always work, regardless of whether the
        // caller tracks DECCKM state itself.
        let cooked;
        let bytes = if rt.app_cursor_keys
            && (data.contains("\x1b[A")
                || data.contains("\x1b[B")
                || data.contains("\x1b[C")
                || data.contains("\x1b[D"))
        {
            cooked = data
                .replace("\x1b[A", "\x1bOA")
                .replace("\x1b[B", "\x1bOB")
                .replace("\x1b[C", "\x1bOC")
                .replace("\x1b[D", "\x1bOD");
            cooked.as_bytes()
        } else {
            data.as_bytes()
        };
        if rt.writer.write_all(bytes).is_ok() && rt.writer.flush().is_ok() {
            rt.mark_attach_activity();
            rt.last_input_at = Some(Instant::now());
            Ok(())
        } else {
            Err(SessionLookupError::NotFound)
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
            return Err(SessionLookupError::NotFound);
        };
        rt.mark_attach_activity();
        if rt.resize_pty(rows, cols) {
            let offset = current_output_offset(&rt.dir);
            let _ = append_resize_event(&rt.dir, offset, rows, cols);
            Ok(())
        } else {
            Err(SessionLookupError::NotFound)
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
            let _ = rt.writer.write_all(&[0x03]);
            let _ = rt.writer.flush();
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
        if rt.child.kill().is_ok() {
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
    ) -> Option<(Vec<String>, usize, bool)> {
        self.prune_evicted_sessions().await;

        let runtime = self.sessions.get(id)?;
        let mut rt = runtime.lock().ok()?;
        rt.refresh_status();
        let len = rt.output_lines.len();
        let start = len.saturating_sub(tail);
        let lines = rt.output_lines[start..].to_vec();
        let running = rt.meta.status.as_str() == "running";
        Some((lines, len, running))
    }

    pub async fn logs_poll(
        &mut self,
        id: &str,
        cursor: usize,
    ) -> Option<(Vec<String>, usize, bool)> {
        self.attach_poll(id, cursor)
            .await
            .ok()
            .map(|(lines, cur, running, _, _)| (lines, cur, running))
    }

    async fn lookup_runtime(
        &mut self,
        id: &str,
    ) -> std::result::Result<Arc<Mutex<SessionRuntime>>, SessionLookupError> {
        self.prune_evicted_sessions().await;

        if let Some(runtime) = self.sessions.get(id) {
            return Ok(runtime.clone());
        }

        if self.evicted_sessions.contains_key(id) {
            return Err(SessionLookupError::Evicted);
        }

        Err(SessionLookupError::NotFound)
    }

    async fn prune_evicted_sessions(&mut self) {
        let now = Instant::now();
        let mut to_persist: Vec<SessionMeta> = Vec::new();
        let mut evicted: Vec<(String, SessionMeta)> = Vec::new();

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
                evicted.push((id.clone(), rt.meta.clone()));
                return false;
            }
            true
        });

        // Persist completed sessions outside the borrow of `self.sessions`.
        if let Some(db) = &self.db {
            for meta in to_persist {
                if let Err(err) = db.update_session(&meta).await {
                    tracing::error!(%err, session_id = meta.id, "failed to persist completed session");
                }
            }
        }

        for (id, meta) in evicted {
            self.history.insert(id.clone(), meta);
            self.evicted_sessions.insert(id, now);
        }

        self.evict_old_tombstones(now);
    }

    fn evict_old_tombstones(&mut self, now: Instant) {
        self.evicted_sessions
            .retain(|_, evicted_at| now.duration_since(*evicted_at) < self.eviction_ttl);
    }

    /// Returns `(session_id, raw_excerpt, output_epoch)`
    pub fn silent_candidates(
        &self,
        suppression_window: Duration,
        min_notification_interval: Duration,
    ) -> Vec<(String, String, Instant)> {
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
                // Build excerpt from the last 10 ring entries.
                let n = rt.ring.len();
                let excerpt: String = rt.ring.iter().skip(n.saturating_sub(10)).cloned().collect();
                if excerpt.trim().is_empty() {
                    return None;
                }

                Some((rt.meta.id.clone(), excerpt, last_output))
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
        use std::collections::VecDeque;

        // Use a dummy path for dir (tests don't write to disk through this path)
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

        let mut ring = VecDeque::new();
        if !excerpt.is_empty() {
            ring.push_back(excerpt.to_string());
        }

        Arc::new(Mutex::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            output_lines: vec![],
            ring,
            ring_limit: 100,
            // We only need a writer stub; use a Vec sink.
            writer: Box::new(std::io::sink()),
            child: make_dummy_child(),
            completed_at: None,
            _pty_master: None,
            persisted: false,
            last_output_at,
            last_input_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            pending_cpr_prefix: String::new(),
            bracketed_paste_mode: false,
            pending_terminal_query_tail: String::new(),
            app_cursor_keys: false,
        }))
    }

    fn make_dummy_child() -> super::super::runtime::RuntimeChild {
        // We cannot construct a real PTY child in unit tests.
        // Use the internal enum variant with a stub. Since RuntimeChild::Pty wraps
        // a Box<dyn portable_pty::Child>, we need a concrete type.
        // Work around by constructing one via a tiny forked process.
        // On all platforms `cmd /c echo x` / `true` runs quickly and exits.
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
        super::super::runtime::RuntimeChild::Pty(child)
    }

    fn store_with(
        runtimes: Vec<Arc<Mutex<super::super::runtime::SessionRuntime>>>,
    ) -> SessionStore {
        let mut store = SessionStore::new(900);
        for rt in runtimes {
            let id = rt.lock().unwrap().meta.id.clone();
            store.sessions.insert(id, rt);
        }
        store
    }

    // -----------------------------------------------------------------------
    // silent_candidates
    // -----------------------------------------------------------------------

    #[test]
    fn test_silent_candidates_returns_running_past_silence() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        // last output was 10s ago → past silence
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "password: ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt]);
        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "abc1234");
    }

    #[test]
    fn test_silent_candidates_allows_recent_output_when_not_suppressed() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        // Current implementation only requires an output epoch to exist.
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_millis(500)),
        );
        let store = store_with(vec![rt]);
        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "abc1234");
    }

    #[test]
    fn test_silent_candidates_ignores_non_running_session() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Stopped,
            "prompt> ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt]);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[test]
    fn test_silent_candidates_ignores_no_output_yet() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime("abc1234", SessionStatus::Running, "prompt> ", None);
        let store = store_with(vec![rt]);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    // -----------------------------------------------------------------------
    // mark_notified + output-epoch gating
    // -----------------------------------------------------------------------

    #[test]
    fn test_mark_notified_suppresses_until_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let mut store = store_with(vec![rt]);

        // First call returns a candidate with an output epoch.
        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let (id, _, epoch) = &first[0];

        // Mark as notified at this output epoch.
        store.mark_notified(id, *epoch, Instant::now());

        // Second call: same output epoch → suppressed.
        let second = store.silent_candidates(silence, min_interval);
        assert!(
            second.is_empty(),
            "should suppress re-notification at same output epoch"
        );
    }

    #[test]
    fn test_mark_notified_allows_after_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let mut store = store_with(vec![rt]);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let (id, _, epoch) = &first[0];
        store.mark_notified(id, *epoch, Instant::now());

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

    #[test]
    fn test_mark_notified_stays_suppressed_without_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let mut store = store_with(vec![rt]);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let (id, _, epoch) = &first[0];
        store.mark_notified(id, *epoch, Instant::now());

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

    #[test]
    fn test_mark_notified_on_unknown_id_is_noop() {
        let mut store = SessionStore::new(900);
        // Should not panic.
        let now = Instant::now();
        store.mark_notified("does_not_exist", now, now);
    }

    #[test]
    fn test_silent_candidates_suppressed_during_recent_attach_activity() {
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
        let store = store_with(vec![rt]);
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

    #[test]
    fn test_silent_candidates_drops_short_age_notifications() {
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
        let store = store_with(vec![rt]);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should drop candidates inside cooldown window"
        );
    }

    // -----------------------------------------------------------------------
    // Capturing writer helper for attach_input tests
    // -----------------------------------------------------------------------

    struct CaptureWriter(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn make_runtime_writable(
        id: &str,
        status: SessionStatus,
        buf: std::sync::Arc<Mutex<Vec<u8>>>,
    ) -> Arc<Mutex<super::super::runtime::SessionRuntime>> {
        use std::collections::VecDeque;

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
        Arc::new(Mutex::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            output_lines: Vec::new(),
            ring: VecDeque::new(),
            ring_limit: 100,
            writer: Box::new(CaptureWriter(buf)),
            child: make_dummy_child(),
            completed_at: None,
            _pty_master: None,
            persisted: false,
            last_output_at: None,
            last_input_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            pending_cpr_prefix: String::new(),
            bracketed_paste_mode: false,
            pending_terminal_query_tail: String::new(),
            app_cursor_keys: false,
        }))
    }

    fn make_runtime_with_lines(
        id: &str,
        lines: Vec<String>,
    ) -> Arc<Mutex<super::super::runtime::SessionRuntime>> {
        use std::collections::VecDeque;

        let dir = std::env::temp_dir().join(format!("oly_store_lines_{id}"));
        let meta = SessionMeta {
            id: id.to_string(),
            title: None,
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            ended_at: None,
            status: SessionStatus::Running,
            pid: None,
            exit_code: None,
        };
        let mut ring = VecDeque::new();
        for line in &lines {
            ring.push_back(line.clone());
        }
        Arc::new(Mutex::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            output_lines: lines,
            ring,
            ring_limit: 100,
            writer: Box::new(std::io::sink()),
            child: make_dummy_child(),
            completed_at: None,
            _pty_master: None,
            persisted: false,
            last_output_at: None,
            last_input_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            pending_cpr_prefix: String::new(),
            bracketed_paste_mode: false,
            pending_terminal_query_tail: String::new(),
            app_cursor_keys: false,
        }))
    }

    // -----------------------------------------------------------------------
    // attach_input — data forwarding and last_input_at tracking
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_input_writes_data_to_writer() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rt = make_runtime_writable("inp0001", SessionStatus::Running, buf.clone());
        let mut store = store_with(vec![rt]);

        store
            .attach_input("inp0001", "hello\r")
            .await
            .expect("attach_input should succeed");

        let written = buf.lock().unwrap().clone();
        assert_eq!(
            written, b"hello\r",
            "expected exact bytes written to PTY writer"
        );
    }

    #[tokio::test]
    async fn test_attach_input_sets_last_input_at() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rt = make_runtime_writable("inp0002", SessionStatus::Running, buf.clone());
        let rt_clone = rt.clone();
        let mut store = store_with(vec![rt]);

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
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rt = make_runtime_writable("inp0003", SessionStatus::Running, buf.clone());
        {
            let mut locked = rt.lock().unwrap();
            locked.app_cursor_keys = true;
        }
        let mut store = store_with(vec![rt]);

        store
            .attach_input("inp0003", "\x1b[A")
            .await
            .expect("attach_input should succeed");

        let written = buf.lock().unwrap().clone();
        assert_eq!(
            written, b"\x1bOA",
            "arrow up should be translated to app-cursor-key form"
        );
    }

    #[tokio::test]
    async fn test_attach_input_decckm_transforms_all_arrows() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rt = make_runtime_writable("inp0004", SessionStatus::Running, buf.clone());
        {
            let mut locked = rt.lock().unwrap();
            locked.app_cursor_keys = true;
        }
        let mut store = store_with(vec![rt]);

        // Send all four arrow sequences at once.
        store
            .attach_input("inp0004", "\x1b[A\x1b[B\x1b[C\x1b[D")
            .await
            .expect("attach_input should succeed");

        let written = buf.lock().unwrap().clone();
        assert_eq!(
            written, b"\x1bOA\x1bOB\x1bOC\x1bOD",
            "all arrow sequences should be translated in DECCKM mode"
        );
    }

    #[tokio::test]
    async fn test_attach_input_no_transform_when_decckm_off() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let rt = make_runtime_writable("inp0005", SessionStatus::Running, buf.clone());
        // app_cursor_keys is false by default.
        let mut store = store_with(vec![rt]);

        store
            .attach_input("inp0005", "\x1b[A\x1b[B")
            .await
            .expect("attach_input should succeed");

        let written = buf.lock().unwrap().clone();
        assert_eq!(
            written, b"\x1b[A\x1b[B",
            "arrow sequences should pass through unchanged when DECCKM is off"
        );
    }

    #[tokio::test]
    async fn test_attach_input_not_found_for_unknown_session() {
        let mut store = SessionStore::new(900);
        let result = store.attach_input("no_such_id", "data").await;
        assert!(
            result.is_err(),
            "attach_input to unknown session should return an error"
        );
    }

    // -----------------------------------------------------------------------
    // attach_snapshot — marks attach activity, returns ring + cursor
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_snapshot_marks_attach_activity() {
        let rt = make_runtime("snap001", SessionStatus::Running, "$ prompt", None);
        let rt_clone = rt.clone();
        let mut store = store_with(vec![rt]);

        store
            .attach_snapshot("snap001")
            .await
            .expect("snapshot should succeed");

        let locked = rt_clone.lock().unwrap();
        assert!(
            locked.last_attach_activity_at.is_some(),
            "attach_snapshot should mark attach activity"
        );
    }

    #[tokio::test]
    async fn test_attach_snapshot_returns_ring_contents() {
        let rt = make_runtime("snap002", SessionStatus::Running, "output line", None);
        let mut store = store_with(vec![rt]);

        let (lines, cursor, running, _, _) = store
            .attach_snapshot("snap002")
            .await
            .expect("snapshot should succeed");

        assert_eq!(lines, vec!["output line".to_string()]);
        assert_eq!(cursor, 0, "cursor should equal output_lines.len()");
        assert!(running, "session should be reported as running");
    }

    #[tokio::test]
    async fn test_attach_snapshot_not_found_for_unknown_session() {
        let mut store = SessionStore::new(900);
        let result = store.attach_snapshot("no_such_id").await;
        assert!(result.is_err(), "snapshot of unknown session should fail");
    }

    // -----------------------------------------------------------------------
    // attach_poll — marks attach activity, returns lines from cursor
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_attach_poll_marks_attach_activity() {
        let rt = make_runtime_with_lines(
            "poll001",
            vec!["line1\n".to_string(), "line2\n".to_string()],
        );
        let rt_clone = rt.clone();
        let mut store = store_with(vec![rt]);

        store
            .attach_poll("poll001", 0)
            .await
            .expect("poll should succeed");

        let locked = rt_clone.lock().unwrap();
        assert!(
            locked.last_attach_activity_at.is_some(),
            "attach_poll should mark attach activity"
        );
    }

    #[tokio::test]
    async fn test_attach_poll_returns_lines_from_cursor() {
        let rt = make_runtime_with_lines(
            "poll002",
            vec![
                "line0\n".to_string(),
                "line1\n".to_string(),
                "line2\n".to_string(),
                "line3\n".to_string(),
            ],
        );
        let mut store = store_with(vec![rt]);

        let (lines, next_cursor, running, _, _) = store
            .attach_poll("poll002", 2)
            .await
            .expect("poll should succeed");

        assert_eq!(
            lines,
            vec!["line2\n".to_string(), "line3\n".to_string()],
            "poll from cursor=2 should return lines[2..]"
        );
        assert_eq!(next_cursor, 4, "next_cursor should be output_lines.len()");
        assert!(running);
    }

    #[tokio::test]
    async fn test_attach_poll_at_end_returns_empty() {
        let rt = make_runtime_with_lines(
            "poll003",
            vec!["line0\n".to_string(), "line1\n".to_string()],
        );
        let mut store = store_with(vec![rt]);

        let (lines, next_cursor, _, _, _) = store
            .attach_poll("poll003", 2)
            .await
            .expect("poll should succeed");

        assert!(lines.is_empty(), "no new lines when cursor is at end");
        assert_eq!(next_cursor, 2);
    }

    #[tokio::test]
    async fn test_attach_poll_not_found_for_unknown_session() {
        let mut store = SessionStore::new(900);
        let result = store.attach_poll("no_such_id", 0).await;
        assert!(result.is_err(), "poll of unknown session should fail");
    }
}
