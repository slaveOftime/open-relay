//! The session store: the daemon's registry of live [`SessionRuntime`]s.
//!
//! [`SessionStore`] is one type with a wide surface, so its methods are grouped
//! by concern into private submodules that each add an `impl SessionStore`
//! block. The shared state and helpers live here.

mod activity;
mod attach;
mod lifecycle;

#[allow(unused_imports)] // re-exported for sibling module AttachSource
pub(crate) use attach::scrollback_seed_bytes;
mod notify;
pub mod pump;
mod query;
#[cfg(test)]
pub(crate) mod testsupport;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use parking_lot::RwLock;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex as TokioMutex, broadcast};
use tracing::{debug, trace, warn};

use crate::{config::ResumePattern, db::Database, session::SessionEventTx};

use super::{SessionError, SessionMeta, SessionStatus, runtime::SessionRuntime};

// Application keystrokes on every platform, not process signals or a PTY
// close. Ctrl-Z is neither Ctrl-D nor a portable termination request.
pub(super) const SOFT_STOP_INPUTS: &[&[u8]] = &[&[0x03], &[0x03], &[0x04]];

/// Keep repeated Ctrl-C inside a short application confirmation window,
/// while reserving room for Ctrl-D, cleanup, and bounded signal escalation.
pub(super) const DOUBLE_CTRL_C_DELAY: Duration = Duration::from_millis(500);
pub(super) const DOUBLE_CTRL_C_DELAY_GRACE_DIVISOR: u32 = 4;

pub(super) const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Output landing this soon after user activity (text input, mouse
/// click/hover, resize, attach) is attributed to that activity — keystroke
/// echo, post-resize redraws and hover/click feedback all take a moment to
/// settle — instead of to the program asking for attention. Anything later
/// than this is considered self-driven program output.
pub(super) const USER_ACTIVITY_WINDOW: Duration = Duration::from_secs(3);
#[cfg(not(test))]
pub(super) const ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT: Duration = Duration::from_millis(3_000);
#[cfg(test)]
pub(super) const ATTACH_INPUT_OUTPUT_WAIT_TIMEOUT: Duration = Duration::from_millis(100);
/// How often `wait_for_change` input polls for the child's response.
///
/// This is pure added latency on every keystroke that asks to be acknowledged,
/// so it is kept well below the threshold at which typing feels laggy. The poll
/// is a single relaxed read of a counter behind a read lock, so a short
/// interval costs almost nothing.
pub(super) const ATTACH_INPUT_OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(4);

/// Entry bound for [`StoreMutableState::persisted_stream_len_cache`].
///
/// The cache is cleared wholesale when full, so a bound below the number of
/// offline sessions a `list` call touches makes every list re-decode every
/// journal past the bound. Large lists are normal (the TUI asks for up to
/// 5000 sessions every refresh), so the bound is sized for them; each entry
/// is a short id plus two integers (well under 100 bytes), keeping the
/// worst-case footprint under ~2 MB.
pub(super) const PERSISTED_STREAM_LEN_CACHE_ENTRIES: usize = 20_000;

pub(super) type SessionMap = HashMap<String, Arc<SessionHandle>>;

pub(super) struct StoreMutableState {
    pub(super) starting_sessions: HashSet<String>,
    pub(super) evicted_sessions: HashMap<String, Instant>,
    /// Persisted filtered-stream lengths for sessions without a live
    /// runtime, keyed by session id: `(incarnation, len)`. The incarnation
    /// component invalidates the entry when the journal advances
    /// (keeps `oly wait` polling cheap against
    /// completed sessions with large journals). The entry bound is
    /// [`PERSISTED_STREAM_LEN_CACHE_ENTRIES`].
    pub(super) persisted_stream_len_cache: HashMap<String, (u64, u64)>,
}

pub(super) struct SessionHandle {
    pub(super) runtime: Arc<RwLock<SessionRuntime>>,
}

impl SessionHandle {
    pub(super) fn new(runtime: Arc<RwLock<SessionRuntime>>) -> Self {
        Self { runtime }
    }

    pub(super) fn read(&self) -> parking_lot::RwLockReadGuard<'_, SessionRuntime> {
        self.runtime.read()
    }

    /// Non-blocking read: returns `None` when a writer currently owns
    /// the runtime. Used by the activity sampler so it can skip
    /// mid-write sessions without stalling the reader thread.
    pub(super) fn try_read(&self) -> Option<parking_lot::RwLockReadGuard<'_, SessionRuntime>> {
        self.runtime.try_read()
    }

    pub(super) fn write(&self) -> parking_lot::RwLockWriteGuard<'_, SessionRuntime> {
        self.runtime.write()
    }
}

pub struct SessionStore {
    pub(super) sessions: ArcSwap<SessionMap>,
    pub(super) mutable: TokioMutex<StoreMutableState>,
    /// TTL for evicting completed sessions, in seconds. Atomic so the
    /// daemon's config hot-reload can adjust it without a restart.
    pub(super) eviction_ttl_secs: std::sync::atomic::AtomicU64,
    /// Byte budget for a session's persisted journal, shared with
    /// `SessionRuntime` so every retention sweep across the process sees
    /// the same value (0 = unlimited). Production passes `0` (live
    /// deletion disabled); tests set a non-zero budget to exercise the
    /// checkpoint-gated retention primitives. There is no hot-reload
    /// setter: changing the cap requires a restart.
    pub(super) journal_byte_cap: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Reloadable rules applied when a completed journal is scanned.
    pub(super) resume_patterns: ArcSwap<Vec<ResumePattern>>,
    pub(super) db: Arc<Database>,
    /// Disk-guard observer, installed by the daemon after construction.
    /// Absent in unit tests, so admission behaves exactly as before there.
    pub(super) disk_monitor: std::sync::OnceLock<Arc<crate::disk::DiskMonitor>>,
    pub(super) event_tx: SessionEventTx,
}

#[derive(Debug, Clone)]
pub struct SilentCandidate {
    pub session_id: String,
    pub session_title: Option<String>,
    pub excerpt: String,
    pub program_status: Option<super::program_status::ProgramStatus>,
    pub output_epoch: Instant,
    /// User activity captured with the output epoch; fences off-lock evaluation.
    pub user_activity_epoch: Option<Instant>,
    pub silence_epoch: Instant,
    pub should_notify: bool,
    pub enabled_for_channels: bool,
    pub last_total_bytes: u64,
}

#[derive(Debug)]
pub(super) struct PreparedStart {
    pub(super) meta: SessionMeta,
    pub(super) session_dir: PathBuf,
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) notifications_enabled: bool,
}

impl SessionStore {
    pub fn with_journal_byte_cap(
        eviction_seconds: u64,
        journal_byte_cap: u64,
        db: Arc<Database>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            sessions: ArcSwap::from_pointee(HashMap::new()),
            mutable: TokioMutex::new(StoreMutableState {
                starting_sessions: HashSet::new(),
                evicted_sessions: HashMap::new(),
                persisted_stream_len_cache: HashMap::new(),
            }),
            eviction_ttl_secs: std::sync::atomic::AtomicU64::new(eviction_seconds.max(1)),
            journal_byte_cap: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                journal_byte_cap,
            )),
            resume_patterns: ArcSwap::from_pointee(Vec::new()),
            db,
            disk_monitor: std::sync::OnceLock::new(),
            event_tx,
        }
    }

    /// Install the daemon's disk observer. New sessions then consult it
    /// before they are admitted; existing sessions are never touched.
    pub fn set_disk_monitor(&self, monitor: Arc<crate::disk::DiskMonitor>) {
        let _ = self.disk_monitor.set(monitor);
    }

    /// Session ids reserved for a start that has not committed yet. Used by
    /// the diagnostic summary to point at starts interrupted by a crash.
    pub async fn starting_session_ids(&self) -> Vec<String> {
        self.mutable
            .lock()
            .await
            .starting_sessions
            .iter()
            .cloned()
            .collect()
    }

    /// Test-only: simulate a start reservation that never committed (the
    /// state a crash mid-start leaves behind), for diagnostic-report tests.
    #[cfg(test)]
    pub(crate) async fn reserve_starting_session_for_test(&self, id: &str) {
        self.mutable
            .lock()
            .await
            .starting_sessions
            .insert(id.to_string());
    }

    /// Current eviction TTL for completed sessions.
    pub(super) fn eviction_ttl(&self) -> Duration {
        Duration::from_secs(
            self.eviction_ttl_secs
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Hot-update the eviction TTL after a config reload.
    pub fn set_eviction_seconds(&self, seconds: u64) {
        self.eviction_ttl_secs
            .store(seconds.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// Handle to the shared journal byte budget so a retention sweep
    /// scheduled by one session can read it through the runtime's `Arc`
    /// clone. The handle stays valid for the daemon's lifetime
    /// (the `SessionStore` itself is `Arc`-shared, so this is just a
    /// borrow-of-`Arc` from `Arc::clone`).
    pub(super) fn journal_byte_cap(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        std::sync::Arc::clone(&self.journal_byte_cap)
    }

    pub fn set_resume_patterns(&self, patterns: Vec<ResumePattern>) {
        self.resume_patterns.store(Arc::new(patterns));
    }

    pub fn event_tx(&self) -> SessionEventTx {
        self.event_tx.clone()
    }

    pub(super) async fn lookup_runtime(
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
}

/// Send a soft-stop input to the PTY and log the result.
pub(super) fn log_soft_stop_send(
    pty: &super::pty::PtyHandle,
    session_id: &str,
    stage: usize,
    total_stages: usize,
    input: &[u8],
    start: &Instant,
) -> std::result::Result<(), TrySendError<Vec<u8>>> {
    let result = pty.try_write_input(input.to_vec());
    match &result {
        Ok(()) => {
            debug!(
                session_id,
                stage,
                total_stages,
                bytes = input.len(),
                elapsed_ms = start.elapsed().as_millis(),
                "queued soft-stop input"
            );
        }
        Err(TrySendError::Full(_)) => {
            debug!(
                session_id,
                stage,
                total_stages,
                elapsed_ms = start.elapsed().as_millis(),
                "soft-stop input deferred because PTY writer queue is full"
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
    result
}

pub(super) fn build_soft_stop_schedule(
    start: Instant,
    grace: Duration,
    requested_final_status: SessionStatus,
) -> Vec<(Instant, &'static [u8])> {
    if grace.is_zero() || !matches!(requested_final_status, SessionStatus::Stopped) {
        return Vec::new();
    }

    // All application inputs precede SIGTERM at 3/4 grace. Cap Ctrl-D as
    // well as the repeat so a long grace buys cleanup time, not delayed keys.
    let offsets = [
        Duration::ZERO,
        DOUBLE_CTRL_C_DELAY.min(grace / DOUBLE_CTRL_C_DELAY_GRACE_DIVISOR),
        Duration::from_secs(1).min(grace / 2),
    ];
    offsets
        .into_iter()
        .zip(SOFT_STOP_INPUTS.iter())
        .map(|(offset, input)| (start + offset, *input))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn schedule_for_grace(grace: Duration) -> Vec<(Instant, &'static [u8])> {
        let start = Instant::now();
        build_soft_stop_schedule(start, grace, SessionStatus::Stopped)
    }

    #[test]
    fn soft_stop_schedule_starts_immediately_with_ctrl_c() {
        let start = Instant::now();
        let sched = build_soft_stop_schedule(start, Duration::from_secs(5), SessionStatus::Stopped);
        assert_eq!(sched.len(), 3, "stages 1, 2, 3 should all be present");
        assert_eq!(sched[0].1, &[0x03][..]);
        let first_offset = sched[0].0.duration_since(start);
        assert_eq!(first_offset, Duration::ZERO, "stage 1 must fire at start");
    }

    #[test]
    fn soft_stop_schedule_second_ctrl_c_lands_inside_double_ctrl_c_window() {
        // A long grace must buy cleanup time, not push the repeated key
        // outside the application's confirmation window.
        for grace_seconds in [1u64, 5, 15] {
            let grace = Duration::from_secs(grace_seconds);
            let cap = DOUBLE_CTRL_C_DELAY.min(
                grace
                    .checked_div(DOUBLE_CTRL_C_DELAY_GRACE_DIVISOR)
                    .unwrap_or(Duration::ZERO),
            );
            let sched = schedule_for_grace(grace);
            assert_eq!(sched[1].1, &[0x03][..], "stage 2 must always be Ctrl-C");
            let offset = sched[1].0.duration_since(sched[0].0);
            assert_eq!(
                offset, cap,
                "second Ctrl-C must land at min(500ms, grace/4) for grace={grace_seconds}s, got {offset:?}"
            );
            assert!(
                offset <= Duration::from_millis(500),
                "second Ctrl-C must never exceed 500 ms"
            );
        }
    }

    #[test]
    fn soft_stop_schedule_gives_ctrl_d_time_before_signal_escalation() {
        for grace in [
            Duration::from_millis(20),
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(60),
        ] {
            let sched = schedule_for_grace(grace);
            assert_eq!(sched[2].1, &[0x04]);
            assert_eq!(
                sched[2].0 - sched[0].0,
                Duration::from_secs(1).min(grace / 2)
            );
            assert!(sched[0].0 < sched[1].0);
            assert!(sched[1].0 < sched[2].0);
            assert!(sched[2].0 < sched[0].0 + grace / 4 * 3);
            assert!(sched[0].0 + grace / 4 * 3 < sched[0].0 + grace);
        }
    }

    #[test]
    fn soft_stop_schedule_is_empty_for_kill_request() {
        let start = Instant::now();
        let sched = build_soft_stop_schedule(start, Duration::from_secs(5), SessionStatus::Killed);
        assert!(sched.is_empty(), "kill must skip soft-stop keystrokes");
    }

    #[test]
    fn soft_stop_schedule_zero_grace_skips_application_inputs() {
        assert!(schedule_for_grace(Duration::ZERO).is_empty());
    }
}
