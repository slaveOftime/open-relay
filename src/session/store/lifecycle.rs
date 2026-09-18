//! Session lifecycle: starting, stopping and evicting runtimes.
//!
//! These are the only methods that create or destroy a [`SessionRuntime`], so
//! every mutation of the session map lives here.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use chrono::Utc;
use futures_util::future::join_all;
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    session::{SessionEvent, validate_session_metadata},
};

use super::super::{
    SessionMeta, SessionStatus, StartSpec,
    persist::append_event,
    runtime::{SessionRuntime, generate_session_id, spawn_session},
};
use super::{
    PreparedStart, SessionHandle, SessionStore, TERMINATE_POLL_INTERVAL, build_soft_stop_schedule,
    log_soft_stop_send,
};

impl SessionStore {
    /// Persist and evict completed sessions that have aged past the in-memory
    /// retention window, and cap oversized live output logs.
    ///
    /// `max_output_log_bytes` of `0` disables the size cap.
    pub async fn run_maintenance(&self, max_output_log_bytes: u64) {
        if max_output_log_bytes > 0 {
            self.truncate_oversized_logs(max_output_log_bytes);
        }
        self.prune_evicted_sessions().await;
    }

    /// Truncate any session `output.log` that has grown past
    /// `max_output_log_bytes`, resetting the persisted log index and the
    /// runtime byte counters so live attach/tail/pagination stay consistent.
    ///
    /// The in-memory rendered screen (`screen_parser`) is untouched, so an
    /// attached client keeps seeing the correct current screen; only the
    /// on-disk scrollback history is dropped.
    fn truncate_oversized_logs(&self, max_output_log_bytes: u64) {
        use crate::session::persist::{append_event, current_output_offset, truncate_output_log};

        let sessions = self.sessions.load_full();
        for (id, handle) in sessions.iter() {
            let mut rt = handle.write();
            // Only cap sessions that are still producing output; a completed
            // session's log is frozen and about to be evicted anyway.
            if rt.is_completed() {
                continue;
            }
            if current_output_offset(&rt.dir) <= max_output_log_bytes {
                continue;
            }

            if let Err(err) = truncate_output_log(&rt.dir) {
                warn!(session_id = %id, %err, "failed to truncate oversized output.log");
                continue;
            }
            crate::session::logs::discard_persisted_log_index(&rt.dir);
            rt.filtered_total_bytes = 0;
            rt.last_total_bytes = 0;
            rt.resize_history.clear();
            let _ = append_event(&rt.dir, "output.log truncated (size cap reached)");
            info!(
                session_id = %id,
                max_output_log_bytes,
                "truncated oversized output.log"
            );
        }
    }

    /// Load session history from the SQLite database on daemon startup.
    ///
    /// Any stale `running` / `stopping` sessions are reconciled to `failed`,
    /// persisted back to SQLite, and returned so callers can emit user-facing
    /// startup notifications.
    pub async fn load_running_stopping_sessions(&self) -> Vec<SessionMeta> {
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

    pub async fn start_session_via_handle(
        store_handle: &Arc<Self>,
        config: &AppConfig,
        spec: StartSpec,
    ) -> Result<String> {
        let prepared = store_handle.prepare_start_session(config, spec).await?;

        let PreparedStart {
            mut meta,
            session_dir,
            rows,
            cols,
            notifications_enabled,
        } = prepared;
        let session_id = meta.id.clone();
        let runtime = match spawn_session(
            &mut meta,
            session_dir,
            rows,
            cols,
            notifications_enabled,
            config.screen_scrollback_rows,
            store_handle.event_tx.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                let _ = store_handle.abort_started_session(&session_id).await;
                return Err(err);
            }
        };
        let cleanup_runtime = Arc::clone(&runtime);

        let result = store_handle.commit_started_session(meta, runtime).await;

        if result.is_err() {
            {
                let mut rt = cleanup_runtime.write();
                let _ = rt.pty.kill();
                rt.mark_completed(SessionStatus::Failed, None);
            }
            let _ = store_handle.abort_started_session(&session_id).await;
        } else if let Some(summary) = store_handle.get_summary(&session_id) {
            let _ = store_handle
                .event_tx
                .send(SessionEvent::SessionCreated(summary));
        }

        result
    }

    /// Create a fresh session from a source session's persisted launch metadata.
    /// The source row, directory, and logs are retained under the original ID.
    pub async fn restart_session_via_handle(
        store_handle: &Arc<Self>,
        config: &AppConfig,
        source_id: &str,
        force: bool,
    ) -> Result<String> {
        let live_handle = store_handle.sessions.load().get(source_id).cloned();
        let (source, needs_termination) = if let Some(handle) = live_handle.as_ref() {
            let mut runtime = handle.write();
            // Restarting a session that is still being created is never allowed,
            // regardless of what refresh_status observes about the child process.
            if runtime.meta.status == SessionStatus::Created {
                return Err(AppError::Protocol(format!(
                    "session is still being created and cannot be restarted: {source_id}"
                )));
            }
            runtime.refresh_status();
            match runtime.meta.status {
                SessionStatus::Running | SessionStatus::Stopping if !force => {
                    return Err(AppError::Protocol(format!(
                        "session is still running: {source_id}. Stop it first, or pass --force to kill it before restarting."
                    )));
                }
                SessionStatus::Running | SessionStatus::Stopping => (runtime.meta.clone(), true),
                SessionStatus::Stopped | SessionStatus::Killed | SessionStatus::Failed => {
                    (runtime.meta.clone(), false)
                }
                SessionStatus::Created => {
                    // `refresh_status` didn't advance the status; treat as
                    // still-being-created and reject.
                    return Err(AppError::Protocol(format!(
                        "session is still being created and cannot be restarted: {source_id}"
                    )));
                }
            }
        } else {
            let Some(source) = store_handle.db.get_session(source_id).await? else {
                return Err(AppError::Protocol(format!(
                    "session not found: {source_id}"
                )));
            };
            match source.status {
                SessionStatus::Created => {
                    return Err(AppError::Protocol(format!(
                        "session is still being created and cannot be restarted: {source_id}"
                    )));
                }
                SessionStatus::Running | SessionStatus::Stopping => {
                    return Err(AppError::Protocol(format!(
                        "session is marked {} but has no live runtime: {source_id}",
                        source.status.as_str()
                    )));
                }
                SessionStatus::Stopped | SessionStatus::Killed | SessionStatus::Failed => {
                    (source, false)
                }
            }
        };

        if let Some(cwd) = source.cwd.as_deref() {
            let path = std::path::Path::new(cwd);
            if !path.exists() {
                return Err(AppError::Protocol(format!(
                    "working directory does not exist: {cwd}"
                )));
            }
            if !path.is_dir() {
                return Err(AppError::Protocol(format!(
                    "working directory is not a directory: {cwd}"
                )));
            }
        }

        if needs_termination {
            let handle = live_handle.expect("live source handle checked above");
            if !Self::terminate_runtime(source_id.to_string(), handle, 0, SessionStatus::Killed)
                .await
            {
                return Err(AppError::Protocol(format!(
                    "failed to kill source session before restart: {source_id}"
                )));
            }
            if let Some(summary) = store_handle.get_summary(source_id) {
                let _ = store_handle
                    .event_tx
                    .send(SessionEvent::SessionUpdated(summary));
            }
        }

        let spec = StartSpec {
            title: source.title,
            tags: source.tags,
            cmd: source.command,
            args: source.args,
            cwd: source.cwd,
            rows: None,
            cols: None,
            notifications_enabled: source.notifications_enabled,
        };
        match Self::start_session_via_handle(store_handle, config, spec).await {
            Ok(session_id) => Ok(session_id),
            Err(err) if needs_termination => Err(AppError::Protocol(format!(
                "source session {source_id} was killed, but its replacement failed to start: {err}"
            ))),
            Err(err) => Err(err),
        }
    }

    pub(super) async fn prepare_start_session(
        &self,
        config: &AppConfig,
        spec: StartSpec,
    ) -> Result<PreparedStart> {
        let sessions = self.sessions.load();
        let running_count = sessions
            .values()
            .filter(|handle| !handle.read().is_completed())
            .count();

        let mut state = self.mutable.lock().await;
        if running_count + state.starting_sessions.len() >= config.max_running_sessions {
            return Err(AppError::MaxSessionsReached(config.max_running_sessions));
        }

        let id = generate_session_id(|candidate| {
            sessions.contains_key(candidate) || state.starting_sessions.contains(candidate)
        });

        let rows = spec.rows.unwrap_or(24).max(1);
        let cols = spec.cols.unwrap_or(80).max(1);
        let created_at = Utc::now();
        let (title, tags) = validate_session_metadata(spec.title, spec.tags)?;

        let meta = SessionMeta {
            id: id.clone(),
            title,
            tags,
            command: spec.cmd,
            args: spec.args,
            cwd: spec.cwd,
            created_at,
            started_at: Some(created_at),
            ended_at: None,
            status: SessionStatus::Running,
            pid: None,
            exit_code: None,
            notifications_enabled: spec.notifications_enabled,
            foreground_color: None,
            background_color: None,
        };

        state.starting_sessions.insert(id.clone());
        drop(state);

        if let Err(err) = self.db.insert_session(&meta).await {
            self.mutable.lock().await.starting_sessions.remove(&id);
            return Err(err);
        }

        Ok(PreparedStart {
            meta,
            session_dir: config.sessions_dir.join(&id),
            rows,
            cols,
            notifications_enabled: spec.notifications_enabled,
        })
    }

    pub(super) async fn commit_started_session(
        &self,
        meta: SessionMeta,
        runtime: Arc<RwLock<SessionRuntime>>,
    ) -> Result<String> {
        let id = meta.id.clone();
        let update_result = self.db.update_session(&meta).await;
        self.mutable.lock().await.starting_sessions.remove(&id);
        update_result?;
        let handle = Arc::new(SessionHandle::new(runtime));
        self.sessions.rcu(|current| {
            let mut next = (**current).clone();
            next.insert(id.clone(), handle.clone());
            next
        });
        Ok(id)
    }

    pub(super) async fn abort_started_session(&self, id: &str) -> Result<()> {
        self.mutable.lock().await.starting_sessions.remove(id);
        if let Some(dir) = self.db.get_session_dir(id).await? {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(AppError::Io(err)),
            }
        }
        match self.db.delete_session(id).await {
            Ok(()) => Ok(()),
            Err(persist_err) => {
                // Directory was removed but the row survived. Mark it Failed
                // so it does not look like a live process and can be cleaned
                // up by maintenance / a retry.
                let mark_failed = async {
                    if let Some(mut meta) = self.db.get_session(id).await? {
                        meta.status = SessionStatus::Failed;
                        meta.pid = None;
                        meta.ended_at = Some(Utc::now());
                        self.db.update_session(&meta).await?;
                    }
                    Ok::<(), AppError>(())
                }
                .await;
                match mark_failed {
                    Ok(()) => Err(persist_err),
                    Err(mark_err) => Err(AppError::Protocol(format!(
                        "deleted session directory but failed to delete DB row for {id}: {persist_err}; additionally failed to mark it failed: {mark_err}"
                    ))),
                }
            }
        }
    }

    pub async fn stop_session(&self, id: &str, grace_seconds: u64) -> bool {
        self.terminate_session(id, grace_seconds, SessionStatus::Stopped)
            .await
    }

    pub async fn kill_session(&self, id: &str) -> bool {
        self.terminate_session(id, 0, SessionStatus::Killed).await
    }

    /// Delete a session entirely: its DB row, its on-disk directory, its
    /// in-memory runtime handle, and any eviction tombstone.
    ///
    /// A still-running session is only removed when `force` is set (it is
    /// killed first); otherwise `AppError::Protocol` is returned so the caller
    /// can surface the `--force` hint. Returns `false` when the id is unknown
    /// (not in memory and not in the database).
    pub async fn delete_session(&self, id: &str, force: bool) -> Result<bool> {
        // If a live runtime exists and is still running, respect the force gate.
        let handle = self.sessions.load().get(id).cloned();
        if let Some(handle) = &handle {
            let running = {
                let mut rt = handle.write();
                rt.refresh_status();
                !rt.is_completed()
            };
            if running {
                if !force {
                    return Err(AppError::Protocol(format!(
                        "session is still running: {id}. Stop it first, or pass --force to kill and delete it."
                    )));
                }
                Self::terminate_runtime(id.to_string(), handle.clone(), 0, SessionStatus::Killed)
                    .await;
            }
        } else if !self.db.session_exists(id).await {
            // Not in memory and not in the DB: nothing to delete.
            let is_tombstoned = self.mutable.lock().await.evicted_sessions.contains_key(id);
            if !is_tombstoned {
                return Ok(false);
            }
        }

        // Remove the on-disk session directory first. If this fails for a
        // real reason (permissions, EBUSY, read-only mount), abort before
        // touching the DB row: deleting the row while the directory survives
        // would orphan the files with no session referencing them, and no
        // later `oly rm` could reach them — recreating the very accumulation
        // this command exists to fix.
        // A failure to look up the directory is itself a reason to abort: if we
        // cannot tell where the files live, deleting the DB row would orphan
        // them just the same. Propagate it instead of silently skipping removal.
        if let Some(dir) = self.db.get_session_dir(id).await? {
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    warn!(session_id = id, %err, "failed to remove session directory during delete");
                    return Err(AppError::Io(err));
                }
            }
        }

        // Remove the DB row.
        self.db.delete_session(id).await?;

        // Drop the in-memory handle and any eviction tombstone.
        if handle.is_some() {
            self.sessions.rcu(|current| {
                let mut next = (**current).clone();
                next.remove(id);
                next
            });
        }
        self.mutable.lock().await.evicted_sessions.remove(id);

        info!(session_id = id, force, "session deleted");
        let _ = self.event_tx.send(SessionEvent::SessionDeleted {
            id: id.to_string(),
            node: None,
        });

        Ok(true)
    }

    async fn terminate_session(
        &self,
        id: &str,
        grace_seconds: u64,
        requested_final_status: SessionStatus,
    ) -> bool {
        let Ok(runtime) = self.lookup_runtime(id).await else {
            debug!(
                session_id = id,
                requested_final_status = requested_final_status.as_str(),
                "terminate session lookup missed"
            );
            return false;
        };

        let terminated = Self::terminate_runtime(
            id.to_string(),
            runtime,
            grace_seconds,
            requested_final_status,
        )
        .await;

        if terminated {
            if let Some(summary) = self.get_summary(id) {
                let _ = self.event_tx.send(SessionEvent::SessionUpdated(summary));
            }
        }

        terminated
    }

    async fn terminate_runtime(
        session_id: String,
        handle: Arc<SessionHandle>,
        grace_seconds: u64,
        requested_final_status: SessionStatus,
    ) -> bool {
        let grace = Duration::from_secs(grace_seconds);
        let start = Instant::now();
        let deadline = start + grace;
        let soft_stop_schedule = build_soft_stop_schedule(start, grace, requested_final_status);
        let mut next_soft_stop_index = 0usize;
        debug!(
            session_id = %session_id,
            requested_final_status = requested_final_status.as_str(),
            grace_seconds,
            soft_stop_attempts = soft_stop_schedule.len(),
            "session termination requested"
        );

        // Begin a soft-stop sequence and let the child exit on its own before
        // escalating to a forced kill when the grace window expires.
        {
            // Brief write lock: check/update status.
            let mut rt = handle.write();
            if rt.refresh_status() {
                debug!(
                    session_id = %session_id,
                    status = rt.meta.status.as_str(),
                    exit_code = ?rt.meta.exit_code,
                    "session already completed before termination started"
                );
                return true;
            }
            rt.requested_final_status = Some(requested_final_status);
            rt.meta.status = SessionStatus::Stopping;
        }
        // Read lock: send first soft-stop input (channel send is &self).
        if let Some((_, input)) = soft_stop_schedule.first() {
            let rt = handle.read();
            log_soft_stop_send(
                &rt.pty,
                &session_id,
                1,
                soft_stop_schedule.len(),
                input,
                &start,
            );
            next_soft_stop_index = 1;
        }

        while Instant::now() < deadline {
            {
                // Brief write lock: poll child exit status.
                let mut rt = handle.write();
                if rt.refresh_status() {
                    debug!(
                        session_id = %session_id,
                        elapsed_ms = start.elapsed().as_millis(),
                        status = rt.meta.status.as_str(),
                        exit_code = ?rt.meta.exit_code,
                        "session exited during grace window"
                    );
                    return true;
                }
            }
            // Read lock: send any due staged soft-stop inputs.
            {
                let rt = handle.read();
                while let Some((at, input)) = soft_stop_schedule.get(next_soft_stop_index) {
                    if Instant::now() < *at {
                        break;
                    }
                    log_soft_stop_send(
                        &rt.pty,
                        &session_id,
                        next_soft_stop_index + 1,
                        soft_stop_schedule.len(),
                        input,
                        &start,
                    );
                    next_soft_stop_index += 1;
                }
            }
            tokio::time::sleep(TERMINATE_POLL_INTERVAL).await;
        }

        let mut rt = handle.write();
        if rt.refresh_status() {
            info!(
                session_id = %session_id,
                elapsed_ms = start.elapsed().as_millis(),
                status = rt.meta.status.as_str(),
                exit_code = ?rt.meta.exit_code,
                "session exited at grace deadline"
            );
            return true;
        }
        debug!(
            session_id = %session_id,
            requested_final_status = requested_final_status.as_str(),
            grace_seconds,
            "session did not stop within grace window; forcing termination"
        );
        if rt.pty.kill().is_ok() {
            let _ = rt.refresh_status();
            info!(
                session_id = %session_id,
                status = rt.meta.status.as_str(),
                exit_code = ?rt.meta.exit_code,
                "forced termination completed"
            );
            true
        } else {
            warn!(
                session_id = %session_id,
                "failed to force terminate session process"
            );
            false
        }
    }

    pub async fn stop_all_sessions(&self, grace_seconds: u64) -> bool {
        let sessions = self.sessions.load();
        let runtimes: Vec<_> = sessions
            .iter()
            .map(|(id, runtime)| (id.clone(), runtime.clone()))
            .collect();

        info!(
            session_count = runtimes.len(),
            grace_seconds, "stopping all sessions"
        );

        let results = join_all(runtimes.into_iter().map(|(session_id, runtime)| {
            Self::terminate_runtime(session_id, runtime, grace_seconds, SessionStatus::Stopped)
        }))
        .await;

        let stopped_count = results.iter().filter(|stopped| **stopped).count();

        info!(
            stopped_count,
            total_sessions = results.len(),
            grace_seconds,
            "completed stop-all session termination pass"
        );
        results.into_iter().all(|stopped| stopped)
    }

    async fn prune_evicted_sessions(&self) {
        let now = Instant::now();
        let mut to_persist: Vec<SessionMeta> = Vec::new();
        let mut evicted_ids: Vec<String> = Vec::new();
        let sessions = self.sessions.load_full();

        for (id, handle) in sessions.iter() {
            let mut rt = handle.write();
            rt.refresh_status();

            if rt.is_completed() && !rt.persisted {
                to_persist.push(rt.meta.clone());
                rt.persisted = true;
            }

            if rt.is_completed() {
                let Some(completed_at) = rt.completed_at else {
                    rt.completed_at = Some(now);
                    continue;
                };
                if now.duration_since(completed_at) >= self.eviction_ttl() {
                    tracing::info!(
                        session_id = id,
                        age_seconds = now.duration_since(completed_at).as_secs(),
                        "evicting completed session from memory after eviction TTL"
                    );
                    let _ = append_event(&rt.dir, "session evicted from memory");
                    evicted_ids.push(id.clone());
                }
            }
        }

        // Persist completed sessions outside the borrow of `self.sessions`.
        for meta in to_persist {
            debug!(session_id = %meta.id, status = meta.status.as_str(), "persisting completed session metadata");
            if let Err(err) = self.db.update_session(&meta).await {
                tracing::error!(%err, session_id = meta.id, "failed to persist completed session");
            }
        }

        if !evicted_ids.is_empty() {
            let evicted_set: HashSet<_> = evicted_ids.iter().cloned().collect();
            self.sessions.rcu(|current| {
                let mut next = (**current).clone();
                next.retain(|id, _| !evicted_set.contains(id));
                next
            });

            let mut state = self.mutable.lock().await;
            for id in evicted_ids {
                debug!(session_id = %id, "session evicted from in-memory store");
                state.evicted_sessions.insert(id, now);
            }
            Self::evict_old_tombstones(&mut state.evicted_sessions, now, self.eviction_ttl());
            return;
        }

        let mut state = self.mutable.lock().await;
        Self::evict_old_tombstones(&mut state.evicted_sessions, now, self.eviction_ttl());
    }

    fn evict_old_tombstones(
        evicted_sessions: &mut HashMap<String, Instant>,
        now: Instant,
        eviction_ttl: Duration,
    ) {
        evicted_sessions.retain(|_, evicted_at| now.duration_since(*evicted_at) < eviction_ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport::*;
    use super::*;
    use crate::session::SessionStatus;
    use chrono::Utc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    #[tokio::test]
    async fn test_run_maintenance_evicts_completed_session_after_ttl() {
        let rt = make_runtime("evict001", SessionStatus::Stopped, "", None);
        {
            let mut locked = rt.write();
            locked.meta.exit_code = Some(0);
            locked.meta.ended_at = Some(Utc::now());
            locked.completed_at = Some(Instant::now() - Duration::from_secs(2));
        }

        let db = make_test_db().await;
        let store = SessionStore::new(1, db);
        let handle = Arc::new(SessionHandle::new(rt));
        store.sessions.rcu(|current| {
            let mut next = (**current).clone();
            next.insert("evict001".to_string(), handle.clone());
            next
        });

        store.run_maintenance(0).await;

        let sessions = store.sessions.load();
        assert!(
            !sessions.contains_key("evict001"),
            "completed sessions older than the eviction TTL should be removed from memory"
        );
        assert!(
            store
                .mutable
                .lock()
                .await
                .evicted_sessions
                .contains_key("evict001"),
            "evicted sessions should leave a tombstone for follow-up lookups"
        );
    }

    #[tokio::test]
    async fn test_run_maintenance_truncates_oversized_live_log() {
        use crate::session::persist::{append_output_raw, current_output_offset};

        let rt = make_runtime("big01", SessionStatus::Running, "", None);
        let dir = rt.read().dir.clone();
        // Write well past the cap we will set.
        append_output_raw(&dir, &vec![b'x'; 4096]).expect("seed oversized log");
        assert!(current_output_offset(&dir) >= 4096);

        let store = store_with(vec![rt.clone()], make_test_db().await);
        store.run_maintenance(1024).await;

        assert_eq!(
            current_output_offset(&dir),
            0,
            "oversized live log should be truncated to zero"
        );
        let locked = rt.read();
        assert_eq!(locked.filtered_total_bytes, 0, "attach offset should reset");
        assert_eq!(
            locked.last_total_bytes, 0,
            "meaningful counter should reset"
        );
    }

    #[tokio::test]
    async fn test_run_maintenance_leaves_small_log_untouched() {
        use crate::session::persist::{append_output_raw, current_output_offset};

        let rt = make_runtime("small1", SessionStatus::Running, "", None);
        let dir = rt.read().dir.clone();
        append_output_raw(&dir, b"hello").expect("seed small log");

        let store = store_with(vec![rt], make_test_db().await);
        store.run_maintenance(1024).await;

        assert_eq!(
            current_output_offset(&dir),
            5,
            "a log under the cap must not be truncated"
        );
    }

    #[tokio::test]
    async fn test_start_session_enforces_limit() {
        let config = make_test_config(1);
        // Create 1 running session
        let rt = make_runtime("s1", SessionStatus::Running, "", None);
        let store = store_with(vec![rt], make_test_db().await);

        // Try to start a 2nd session
        let spec = StartSpec {
            title: None,
            tags: vec![],
            cmd: "echo".into(),
            args: vec![],
            cwd: None,
            rows: None,
            cols: None,
            notifications_enabled: true,
        };

        let result = store.prepare_start_session(&config, spec).await;

        // Assert it fails with MaxSessionsReached
        assert!(result.is_err());
        match result {
            Err(crate::error::AppError::MaxSessionsReached(limit)) => {
                assert_eq!(limit, 1);
            }
            _ => panic!("Expected MaxSessionsReached error, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn restart_rejects_running_source_without_force() {
        let rt = make_runtime("run0001", SessionStatus::Running, "", None);
        let store = Arc::new(store_with(vec![rt], make_test_db().await));
        let err = SessionStore::restart_session_via_handle(
            &store,
            &make_test_config(2),
            "run0001",
            false,
        )
        .await
        .expect_err("running source should require force");
        assert!(err.to_string().contains("pass --force"));
    }

    #[tokio::test]
    async fn restart_rejects_created_source() {
        let rt = make_runtime("new0001", SessionStatus::Created, "", None);
        let store = Arc::new(store_with(vec![rt], make_test_db().await));
        let err = SessionStore::restart_session_via_handle(
            &store,
            &make_test_config(2),
            "new0001",
            false,
        )
        .await
        .expect_err("created source should be rejected");
        assert!(err.to_string().contains("still being created"));
    }

    #[tokio::test]
    async fn restart_rejects_missing_source() {
        let store = Arc::new(SessionStore::new(900, make_test_db().await));
        let err = SessionStore::restart_session_via_handle(
            &store,
            &make_test_config(2),
            "missing",
            false,
        )
        .await
        .expect_err("missing source should be rejected");
        assert!(err.to_string().contains("session not found: missing"));
    }

    #[tokio::test]
    async fn test_prepare_start_session_reserves_capacity_until_abort() {
        let config = make_test_config(1);
        let db = make_test_db().await;
        let store = SessionStore::new(900, db.clone());
        let spec = StartSpec {
            title: None,
            tags: vec![],
            cmd: "echo".into(),
            args: vec![],
            cwd: None,
            rows: None,
            cols: None,
            notifications_enabled: true,
        };

        let prepared = store
            .prepare_start_session(&config, spec)
            .await
            .expect("first reservation should succeed");
        assert!(
            db.session_exists(&prepared.meta.id).await,
            "reservation should persist a placeholder session row"
        );

        let result = store
            .prepare_start_session(
                &config,
                StartSpec {
                    title: None,
                    tags: vec![],
                    cmd: "echo".into(),
                    args: vec![],
                    cwd: None,
                    rows: None,
                    cols: None,
                    notifications_enabled: true,
                },
            )
            .await;

        assert!(matches!(
            result,
            Err(crate::error::AppError::MaxSessionsReached(1))
        ));

        store
            .abort_started_session(&prepared.meta.id)
            .await
            .expect("aborting reservation should succeed");
        assert!(
            !db.session_exists(&prepared.meta.id).await,
            "aborting reservation should clean up the placeholder session row"
        );
    }

    // -----------------------------------------------------------------------
    // attach_input — data forwarding and last_input_at tracking
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stop_session_preserves_completed_failure() {
        let (rt, mut writer_rx) = make_runtime_writable("stp0001", SessionStatus::Failed);
        let rt_clone = rt.clone();
        {
            let mut locked = rt.write();
            locked.meta.exit_code = Some(42);
            locked.meta.ended_at = Some(Utc::now());
            locked.completed_at = Some(Instant::now());
        }
        let store = store_with(vec![rt], make_test_db().await);

        assert!(
            store.stop_session("stp0001", 0).await,
            "completed session should still be treated as found"
        );

        let locked = rt_clone.read();
        assert!(matches!(locked.meta.status, SessionStatus::Failed));
        assert_eq!(locked.meta.exit_code, Some(42));
        assert!(
            writer_rx.try_recv().is_err(),
            "completed sessions should not receive a synthetic Ctrl-C"
        );
    }

    #[tokio::test]
    async fn test_kill_session_preserves_completed_failure() {
        let (rt, mut writer_rx) = make_runtime_writable("kil0001", SessionStatus::Failed);
        let rt_clone = rt.clone();
        {
            let mut locked = rt.write();
            locked.meta.exit_code = Some(99);
            locked.meta.ended_at = Some(Utc::now());
            locked.completed_at = Some(Instant::now());
        }
        let store = store_with(vec![rt], make_test_db().await);

        assert!(
            store.kill_session("kil0001").await,
            "completed session should still be treated as found"
        );

        let locked = rt_clone.read();
        assert!(matches!(locked.meta.status, SessionStatus::Failed));
        assert_eq!(locked.meta.exit_code, Some(99));
        assert!(
            writer_rx.try_recv().is_err(),
            "completed sessions should not receive synthetic input during kill"
        );
    }

    #[tokio::test]
    async fn test_kill_session_terminates_running_session() {
        let (rt, _writer_rx) = make_runtime_writable("kilbasic", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        assert!(
            store.kill_session("kilbasic").await,
            "kill should succeed for a running session"
        );

        let sessions = store.sessions.load();
        let handle = sessions
            .get("kilbasic")
            .expect("runtime should remain addressable");
        let rt = handle.read();
        assert!(matches!(
            rt.meta.status,
            SessionStatus::Killed | SessionStatus::Failed
        ));
        assert!(
            rt.is_completed(),
            "killed session should be marked completed"
        );
    }

    #[tokio::test]
    async fn test_delete_session_removes_stopped_session() {
        let rt = make_runtime("del0001", SessionStatus::Stopped, "", None);
        {
            let mut locked = rt.write();
            locked.meta.exit_code = Some(0);
            locked.meta.ended_at = Some(Utc::now());
            locked.completed_at = Some(Instant::now());
        }
        let db = make_test_db().await;
        db.insert_session(&rt.read().meta).await.expect("insert");
        let store = store_with(vec![rt], db);

        // The delete path removes the canonical `<sessions_dir>/<id>` directory,
        // so seed that (the in-memory test fixture uses an unrelated temp path).
        let canonical_dir = store
            .db
            .get_session_dir("del0001")
            .await
            .expect("get_session_dir")
            .expect("session dir path");
        std::fs::create_dir_all(&canonical_dir).expect("seed session dir");

        let removed = store
            .delete_session("del0001", false)
            .await
            .expect("delete should succeed");
        assert!(removed, "stopped session should be deleted");
        assert!(
            !store.sessions.load().contains_key("del0001"),
            "handle should be dropped from memory"
        );
        assert!(
            !store.db.session_exists("del0001").await,
            "db row should be gone"
        );
        assert!(
            !canonical_dir.exists(),
            "canonical session directory should be removed"
        );
    }

    #[tokio::test]
    async fn test_delete_running_session_requires_force() {
        let (rt, _writer_rx) = make_runtime_writable("del0002", SessionStatus::Running);
        let db = make_test_db().await;
        db.insert_session(&rt.read().meta).await.expect("insert");
        let store = store_with(vec![rt], db);

        let err = store
            .delete_session("del0002", false)
            .await
            .expect_err("running session without force should error");
        assert!(
            matches!(err, AppError::Protocol(ref m) if m.contains("--force")),
            "error should hint at --force, got {err:?}"
        );
        assert!(
            store.sessions.load().contains_key("del0002"),
            "running session must survive a non-forced delete"
        );
    }

    #[tokio::test]
    async fn test_delete_running_session_with_force_kills_and_removes() {
        let (rt, _writer_rx) = make_runtime_writable("del0003", SessionStatus::Running);
        let db = make_test_db().await;
        db.insert_session(&rt.read().meta).await.expect("insert");
        let store = store_with(vec![rt], db);

        let removed = store
            .delete_session("del0003", true)
            .await
            .expect("forced delete should succeed");
        assert!(removed, "forced delete should report removal");
        assert!(
            !store.sessions.load().contains_key("del0003"),
            "handle should be dropped after forced delete"
        );
        assert!(
            !store.db.session_exists("del0003").await,
            "db row should be gone after forced delete"
        );
    }

    #[tokio::test]
    async fn test_delete_unknown_session_returns_false() {
        let store = store_with(vec![], make_test_db().await);
        let removed = store
            .delete_session("nope", false)
            .await
            .expect("delete of unknown id should not error");
        assert!(!removed, "unknown id should report not-found");
    }

    #[tokio::test]
    async fn test_delete_aborts_and_keeps_row_when_dir_removal_fails() {
        let rt = make_runtime("del0004", SessionStatus::Stopped, "", None);
        {
            let mut locked = rt.write();
            locked.meta.exit_code = Some(0);
            locked.meta.ended_at = Some(Utc::now());
            locked.completed_at = Some(Instant::now());
        }
        let db = make_test_db().await;
        db.insert_session(&rt.read().meta).await.expect("insert");
        let store = store_with(vec![rt], db);

        // Seed the canonical `<sessions_dir>/<id>` path as a regular FILE, so
        // `remove_dir_all` fails with a non-NotFound error and the delete must
        // abort before removing the DB row.
        let canonical = store
            .db
            .get_session_dir("del0004")
            .await
            .expect("get_session_dir")
            .expect("session dir path");
        if let Some(parent) = canonical.parent() {
            std::fs::create_dir_all(parent).expect("create sessions dir");
        }
        std::fs::write(&canonical, b"not a directory").expect("seed file at dir path");

        let err = store
            .delete_session("del0004", false)
            .await
            .expect_err("delete should fail when the session dir cannot be removed");
        assert!(
            matches!(err, AppError::Io(_)),
            "dir-removal failure should surface as an I/O error, got {err:?}"
        );
        assert!(
            store.db.session_exists("del0004").await,
            "DB row must survive when the directory could not be removed"
        );

        let _ = std::fs::remove_file(&canonical);
    }

    #[tokio::test]
    async fn test_stop_session_uses_staged_soft_shutdown_inputs() {
        let (rt, mut writer_rx) = make_runtime_writable("stp0002", SessionStatus::Running);
        let store = store_with(vec![rt], make_test_db().await);

        assert!(
            store.stop_session("stp0002", 1).await,
            "running session should be stoppable"
        );

        let mut writes = Vec::new();
        while let Ok(bytes) = writer_rx.try_recv() {
            writes.push(bytes);
        }

        assert_eq!(writes, expected_soft_stop_inputs());
    }

    #[tokio::test]
    async fn test_stop_all_sessions_runs_in_parallel() {
        let (rt1, _writer_rx1) = make_runtime_writable("stp1001", SessionStatus::Running);
        let (rt2, _writer_rx2) = make_runtime_writable("stp1002", SessionStatus::Running);
        let (rt3, _writer_rx3) = make_runtime_writable("stp1003", SessionStatus::Running);
        let store = store_with(vec![rt1, rt2, rt3], make_test_db().await);

        let started = Instant::now();
        assert!(
            store.stop_all_sessions(1).await,
            "all running sessions should be stoppable"
        );

        assert!(
            started.elapsed() < Duration::from_millis(2_500),
            "stop_all_sessions should stop multiple sessions concurrently"
        );
    }

    // -----------------------------------------------------------------------
    // attach_detach
    // -----------------------------------------------------------------------

    #[test]
    fn instant_to_utc_reconstructs_recent_wall_clock_time() {
        use crate::session::runtime::instant_to_utc;
        let before = Utc::now();
        let instant = Instant::now() - Duration::from_secs(2);
        let converted = instant_to_utc(instant).expect("conversion should succeed");
        let after = Utc::now();

        assert!(converted >= before - chrono::TimeDelta::seconds(3));
        assert!(converted <= after - chrono::TimeDelta::seconds(1));
    }
}
