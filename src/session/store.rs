use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use chrono::Utc;
use futures_util::future::join_all;
use parking_lot::RwLock;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex as TokioMutex, broadcast};
use tracing::{debug, info, trace, warn};

use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    protocol::{ListQuery, SessionSummary},
    session::{SessionEvent, SessionEventTx, SessionLiveSummary, normalize_session_tags},
};

use super::{
    SessionError, SessionMeta, SessionStatus, StartSpec,
    logs::split_rendered_log_output,
    persist::{append_event, append_resize_event, current_output_offset, read_output_from},
    runtime::{SessionRuntime, generate_session_id, spawn_session},
};

#[cfg(target_os = "windows")]
const SOFT_STOP_INPUTS: &[&[u8]] = &[&[0x03], &[0x03], &[0x1a, b'\r']];

#[cfg(not(target_os = "windows"))]
const SOFT_STOP_INPUTS: &[&[u8]] = &[&[0x03], &[0x03], &[0x04]];

const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(not(test))]
const ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT: Duration = Duration::from_millis(3_000);
#[cfg(test)]
const ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT: Duration = Duration::from_millis(100);
const ATTACH_INPUT_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

type SessionMap = HashMap<String, Arc<SessionHandle>>;

struct StoreMutableState {
    starting_sessions: HashSet<String>,
    evicted_sessions: HashMap<String, Instant>,
}

struct SessionHandle {
    runtime: Arc<RwLock<SessionRuntime>>,
}

impl SessionHandle {
    fn new(runtime: Arc<RwLock<SessionRuntime>>) -> Self {
        Self { runtime }
    }

    fn read(&self) -> parking_lot::RwLockReadGuard<'_, SessionRuntime> {
        self.runtime.read()
    }

    fn write(&self) -> parking_lot::RwLockWriteGuard<'_, SessionRuntime> {
        self.runtime.write()
    }
}

pub struct SessionStore {
    sessions: ArcSwap<SessionMap>,
    mutable: TokioMutex<StoreMutableState>,
    eviction_ttl: Duration,
    db: Arc<Database>,
    event_tx: SessionEventTx,
}

#[derive(Debug, Clone)]
pub struct SilentCandidate {
    pub session_id: String,
    pub session_title: Option<String>,
    pub excerpt: String,
    pub output_epoch: Instant,
    pub enabled_for_channels: bool,
    pub last_total_bytes: u64,
}

#[derive(Debug)]
struct PreparedStart {
    meta: SessionMeta,
    session_dir: PathBuf,
    rows: u16,
    cols: u16,
    notifications_enabled: bool,
}

impl SessionStore {
    pub fn new(eviction_seconds: u64, db: Arc<Database>) -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            sessions: ArcSwap::from_pointee(HashMap::new()),
            mutable: TokioMutex::new(StoreMutableState {
                starting_sessions: HashSet::new(),
                evicted_sessions: HashMap::new(),
            }),
            eviction_ttl: Duration::from_secs(eviction_seconds.max(1)),
            db,
            event_tx,
        }
    }

    pub fn event_tx(&self) -> SessionEventTx {
        self.event_tx.clone()
    }

    /// Persist and evict completed sessions that have aged past the in-memory
    /// retention window.
    pub async fn run_maintenance(&self) {
        self.prune_evicted_sessions().await;
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
        let runtime = match spawn_session(&mut meta, session_dir, rows, cols, notifications_enabled)
        {
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

    async fn prepare_start_session(
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
        let tags = normalize_session_tags(spec.tags);

        let meta = SessionMeta {
            id: id.clone(),
            title: spec.title,
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

    async fn commit_started_session(
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

    async fn abort_started_session(&self, id: &str) -> Result<()> {
        self.mutable.lock().await.starting_sessions.remove(id);
        self.db.delete_session(id).await
    }

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

    /// Returns the current terminal mode snapshot for the session, if available.
    pub fn get_mode_snapshot(&self, id: &str) -> Option<crate::session::ModeSnapshot> {
        let sessions = self.sessions.load();
        sessions.get(id).map(|handle| handle.read().mode_snapshot())
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
            broadcast::Receiver<Arc<Bytes>>,
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
    ) -> std::result::Result<
        (Vec<u8>, u64, broadcast::Receiver<Arc<Bytes>>, bool, bool),
        SessionError,
    > {
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

    pub async fn stop_session(&self, id: &str, grace_seconds: u64) -> bool {
        self.terminate_session(id, grace_seconds, SessionStatus::Stopped)
            .await
    }

    pub async fn kill_session(&self, id: &str) -> bool {
        self.terminate_session(id, 0, SessionStatus::Killed).await
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

    async fn lookup_runtime(
        &self,
        id: &str,
    ) -> std::result::Result<Arc<SessionHandle>, SessionError> {
        let sessions = self.sessions.load();
        if let Some(runtime) = sessions.get(id) {
            trace!(
                session_id = id,
                "session runtime lookup hit in-memory runtime"
            );
            return Ok(runtime.clone());
        }

        if self.mutable.lock().await.evicted_sessions.contains_key(id) {
            debug!(
                session_id = id,
                "session runtime lookup hit evicted tombstone"
            );
            return Err(SessionError::Evicted);
        }

        debug!(session_id = id, "session runtime lookup missed");
        Err(SessionError::NotRunning)
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
                if now.duration_since(completed_at) >= self.eviction_ttl {
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
            Self::evict_old_tombstones(&mut state.evicted_sessions, now, self.eviction_ttl);
            return;
        }

        let mut state = self.mutable.lock().await;
        Self::evict_old_tombstones(&mut state.evicted_sessions, now, self.eviction_ttl);
    }

    fn evict_old_tombstones(
        evicted_sessions: &mut HashMap<String, Instant>,
        now: Instant,
        eviction_ttl: Duration,
    ) {
        evicted_sessions.retain(|_, evicted_at| now.duration_since(*evicted_at) < eviction_ttl);
    }

    /// Returns silent-notification candidates with session id, raw excerpt,
    /// and output epoch.
    pub fn silent_candidates(
        &self,
        attach_suppression_window: Duration,
        min_notification_interval: Duration,
    ) -> Vec<SilentCandidate> {
        let now = Instant::now();
        let sessions = self.sessions.load();
        sessions
            .values()
            .filter_map(|handle| {
                let rt = handle.read();
                if rt.is_completed() {
                    return None;
                }

                // If attach just happend, there is not need to notify in supression window, treat it as notified
                if let Some(last_attach_activity) = rt.last_attach_activity_at {
                    if now.duration_since(last_attach_activity) < attach_suppression_window {
                        trace!("silent becase recent attach activity");
                        drop(rt);
                        let mut rt = handle.write();
                        rt.last_notified_at = rt.last_output_epoch;
                        return None;
                    }
                }

                // If output just happend, there is not need to notify in supression window, treat it as notified
                let last_output = rt.last_output_epoch?;
                if now.duration_since(last_output) < attach_suppression_window {
                    trace!("silent becase recent output activity");
                    return None;
                }

                // If just notified in short time, the there is no need notify again
                if let Some(last_notified_at) = rt.last_notified_at {
                    if last_output - last_notified_at < min_notification_interval {
                        trace!("silent becase just notified since last output");
                        return None;
                    }
                }

                if rt.notified_output_epoch == Some(last_output) {
                    trace!("silent becase no changed since last nofification");
                    return None;
                }

                debug!(
                    session_id = rt.meta.id.as_str(),
                    last_input_at = ?rt.last_input_at,
                    last_attach_activity_at = ?rt.last_attach_activity_at,
                    last_output_epoch = ?rt.last_output_epoch,
                    last_notified_at = ?rt.last_notified_at,
                    "silent candidate ready"
                );

                // As this is most for matching some pattern from coding agent cli, most of them have input box under the bottom.
                // And most of them are using alt screen, it is more accurate to just use the live tail logs.
                // Silent can still be a fallback, just need to wait a little bit longer for the notification.
                let excerpt = rt.render_logs(15, false, u16::MAX);
                Some(SilentCandidate {
                    session_id: rt.meta.id.clone(),
                    session_title: rt.meta.title.clone(),
                    excerpt: String::from_utf8_lossy(&excerpt).into_owned(),
                    output_epoch: last_output,
                    enabled_for_channels: rt.notifications_enabled,
                    last_total_bytes: rt.last_total_bytes,
                })
            })
            .collect()
    }

    /// Records a successful notification for `session_id` at `output_epoch`.
    /// Re-notification is suppressed until output advances to a new epoch.
    pub fn mark_notified(&self, session_id: &str, output_epoch: Instant, notified_at: Instant) {
        let sessions = self.sessions.load();
        if let Some(handle) = sessions.get(session_id) {
            let mut rt = handle.write();
            rt.notified_output_epoch = Some(output_epoch);
            rt.last_notified_at = Some(notified_at);
        }
    }
}

/// Send a soft-stop input to the PTY and log the result.
fn log_soft_stop_send(
    pty: &super::pty::PtyHandle,
    session_id: &str,
    stage: usize,
    total_stages: usize,
    input: &[u8],
    start: &Instant,
) {
    match pty.try_write_input(input.to_vec()) {
        Ok(()) => {
            debug!(
                session_id,
                stage,
                total_stages,
                bytes = input.len(),
                elapsed_ms = start.elapsed().as_millis(),
                "sent soft-stop input"
            );
        }
        Err(TrySendError::Full(_)) => {
            warn!(
                session_id,
                stage,
                total_stages,
                bytes = input.len(),
                elapsed_ms = start.elapsed().as_millis(),
                "soft-stop input dropped because PTY writer queue is full"
            );
        }
        Err(TrySendError::Closed(_)) => {
            warn!(
                session_id,
                stage,
                total_stages,
                bytes = input.len(),
                elapsed_ms = start.elapsed().as_millis(),
                "soft-stop input failed because PTY writer is closed"
            );
        }
    }
}

fn build_soft_stop_schedule(
    start: Instant,
    grace: Duration,
    requested_final_status: SessionStatus,
) -> Vec<(Instant, &'static [u8])> {
    if !matches!(requested_final_status, SessionStatus::Stopped) {
        return Vec::new();
    }

    let stage_count = SOFT_STOP_INPUTS.len();
    SOFT_STOP_INPUTS
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let offset_millis = if index == 0 || grace.is_zero() {
                0
            } else {
                ((grace.as_millis() * index as u128) / stage_count as u128) as u64
            };
            (start + Duration::from_millis(offset_millis), *input)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionMeta, SessionStatus, pty::collect_chunk_bytes};
    use chrono::Utc;
    use std::sync::Arc;

    fn make_runtime(
        id: &str,
        status: SessionStatus,
        excerpt: &str,
        last_output_ago: Option<Duration>,
    ) -> Arc<RwLock<super::super::runtime::SessionRuntime>> {
        use tokio::sync::{broadcast, mpsc};

        let dir =
            std::env::temp_dir().join(format!("oly_store_test_{id}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create runtime test dir");
        if !excerpt.is_empty() {
            crate::session::persist::append_output_raw(&dir, excerpt.as_bytes())
                .expect("persist runtime excerpt");
        }

        let meta = SessionMeta {
            id: id.to_string(),
            title: None,
            tags: vec![],
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
        let mut screen_parser = vt100::Parser::new(24, 80, 0);
        if !excerpt.is_empty() {
            screen_parser.process(excerpt.as_bytes());
        }

        let (broadcast_tx, _rx) = broadcast::channel(4);
        let (resize_tx, _resize_rx) = broadcast::channel(4);
        let (writer_tx, _writer_rx) = mpsc::channel(8);
        let (child, pty_master) = make_dummy_child();
        Arc::new(RwLock::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            last_total_bytes: excerpt.as_bytes().len() as u64,
            raw_total_bytes: excerpt.as_bytes().len() as u64,
            broadcast_tx,
            resize_tx,
            pty: super::super::pty::PtyHandle {
                child,
                writer_tx,
                pty_master: parking_lot::Mutex::new(Some(pty_master)),
            },
            pty_size: None,
            resize_history: Vec::new(),
            completed_at: None,
            persisted: false,
            requested_final_status: None,
            last_output_epoch: last_output_at,
            last_input_at: None,
            last_attach_activity_at: None,
            attach_count: 0,
            last_notified_at: None,
            notified_output_epoch: None,
            screen_parser,
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
        let path = std::env::temp_dir().join(format!("oly_test_{}.db", uuid::Uuid::new_v4()));
        Arc::new(
            Database::open(&path, std::env::temp_dir())
                .await
                .expect("open test DB"),
        )
    }

    fn store_with(
        runtimes: Vec<Arc<RwLock<super::super::runtime::SessionRuntime>>>,
        db: Arc<Database>,
    ) -> SessionStore {
        let store = SessionStore::new(900, db);
        for rt in runtimes {
            let id = rt.read().meta.id.clone();
            let handle = Arc::new(SessionHandle::new(rt));
            store.sessions.rcu(|current| {
                let mut next = (**current).clone();
                next.insert(id.clone(), handle.clone());
                next
            });
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
    async fn test_silent_candidates_suppresses_recent_output_within_attach_window() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_millis(500)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_respects_min_notification_interval() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        {
            let sessions = store.sessions.load();
            let handle = sessions.get("abc1234").unwrap();
            let mut rt = handle.write();
            rt.last_notified_at = Some(Instant::now() - Duration::from_secs(3));
            rt.notified_output_epoch = None;
        }

        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
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

    #[tokio::test]
    async fn test_silent_candidates_includes_screen_excerpt() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(silence, min_interval);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "abc1234");
        assert!(
            candidates[0].excerpt.contains("prompt>"),
            "excerpt should contain rendered screen content"
        );
    }

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
    async fn test_mark_notified_suppresses_until_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![rt], make_test_db().await);

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
        let store = store_with(vec![rt], make_test_db().await);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;
        store.mark_notified(id, epoch, Instant::now());

        // Simulate new output by advancing last_output_at on the runtime.
        {
            let sessions = store.sessions.load();
            let handle = sessions.get("abc1234").unwrap();
            let mut rt = handle.write();
            // A new epoch strictly later than the notified one, but old enough
            // to be outside the attach suppression window.
            rt.last_output_epoch = Some(Instant::now() - Duration::from_secs(2));
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
        let store = store_with(vec![rt], make_test_db().await);

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
            let sessions = store.sessions.load();
            let handle = sessions.get("abc1234").unwrap();
            let mut rt = handle.write();
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
        let store = SessionStore::new(900, make_test_db().await);
        // Should not panic.
        let now = Instant::now();
        store.mark_notified("does_not_exist", now, now);
    }

    #[tokio::test]
    async fn test_set_notifications_enabled_updates_runtime_and_snapshot() {
        let runtime = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![runtime], make_test_db().await);

        store
            .set_notifications_enabled("abc1234", false)
            .await
            .expect("disable notifications");

        let sessions = store.sessions.load();
        let handle = sessions.get("abc1234").expect("runtime should exist");
        let rt = handle.read();
        assert!(!rt.to_summary().notifications_enabled);
        assert!(!rt.notifications_enabled);
    }

    #[tokio::test]
    async fn test_set_notifications_enabled_unknown_id_returns_error() {
        let store = SessionStore::new(900, make_test_db().await);
        let result = store.set_notifications_enabled("missing", false).await;
        assert!(matches!(result, Err(SessionError::NotRunning)));
    }

    #[tokio::test]
    async fn test_set_notifications_enabled_rejects_completed_session() {
        let runtime = make_runtime(
            "done123",
            SessionStatus::Stopped,
            "",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![runtime], make_test_db().await);

        let result = store.set_notifications_enabled("done123", false).await;
        assert!(matches!(result, Err(SessionError::NotRunning)));
    }

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

        store.run_maintenance().await;

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
    async fn test_silent_candidates_suppressed_during_recent_attach_activity() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)), // output 5s ago (past silence)
        );
        // Recent attach activity should suppress notifications without mutating runtime state.
        {
            let mut locked = rt.write();
            locked.last_attach_activity_at = Some(Instant::now());
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should suppress notification while attach activity is inside suppression window"
        );

        let sessions = store.sessions.load();
        let handle = sessions.get("abc1234").unwrap();
        let locked = handle.read();
        assert!(
            locked.last_output_epoch.is_some(),
            "suppression path should not mutate output epoch — it must remain intact"
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
            let mut locked = rt.write();
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
        Arc<RwLock<super::super::runtime::SessionRuntime>>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        make_runtime_writable_with_capacity(id, status, 8)
    }

    fn make_runtime_writable_with_capacity(
        id: &str,
        status: SessionStatus,
        capacity: usize,
    ) -> (
        Arc<RwLock<super::super::runtime::SessionRuntime>>,
        tokio::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        use tokio::sync::{broadcast, mpsc};

        let dir =
            std::env::temp_dir().join(format!("oly_store_writable_{id}_{}", uuid::Uuid::new_v4()));
        let meta = SessionMeta {
            id: id.to_string(),
            title: None,
            tags: vec![],
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
        let (resize_tx, _resize_rx) = broadcast::channel(4);
        let (writer_tx, writer_rx) = mpsc::channel(capacity.max(1));
        let (child, pty_master) = make_dummy_child();
        let rt = Arc::new(RwLock::new(super::super::runtime::SessionRuntime {
            meta,
            dir,
            last_total_bytes: 0,
            raw_total_bytes: 0,
            broadcast_tx,
            resize_tx,
            pty: super::super::pty::PtyHandle {
                child,
                writer_tx,
                pty_master: parking_lot::Mutex::new(Some(pty_master)),
            },
            pty_size: None,
            resize_history: Vec::new(),
            completed_at: None,
            persisted: false,
            requested_final_status: None,
            last_output_epoch: None,
            last_input_at: None,
            last_attach_activity_at: None,
            attach_count: 0,
            last_notified_at: None,
            notified_output_epoch: None,
            screen_parser: vt100::Parser::new(24, 80, 0),
            output_closed: false,
            notifications_enabled: true,
        }));
        (rt, writer_rx)
    }

    #[test]
    fn instant_to_utc_reconstructs_recent_wall_clock_time() {
        use super::super::runtime::instant_to_utc;
        let before = Utc::now();
        let instant = Instant::now() - Duration::from_secs(2);
        let converted = instant_to_utc(instant).expect("conversion should succeed");
        let after = Utc::now();

        assert!(converted >= before - chrono::TimeDelta::seconds(3));
        assert!(converted <= after - chrono::TimeDelta::seconds(1));
    }

    fn make_test_config(max_running_sessions: usize) -> AppConfig {
        use std::path::PathBuf;
        AppConfig {
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
            info_file: PathBuf::from("."),
            lock_file: PathBuf::from("."),
            max_running_sessions,
            notification_hook: None,
        }
    }

    #[cfg(target_os = "windows")]
    fn expected_soft_stop_inputs() -> Vec<Vec<u8>> {
        vec![vec![0x03], vec![0x03], vec![0x1a, b'\r']]
    }

    #[cfg(not(target_os = "windows"))]
    fn expected_soft_stop_inputs() -> Vec<Vec<u8>> {
        vec![vec![0x03], vec![0x03], vec![0x04]]
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
