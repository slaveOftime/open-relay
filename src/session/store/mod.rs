//! The session store: the daemon's registry of live [`SessionRuntime`]s.
//!
//! [`SessionStore`] is one type with a wide surface, so its methods are grouped
//! by concern into private submodules that each add an `impl SessionStore`
//! block. The shared state and helpers live here.

mod attach;
mod lifecycle;
mod notify;
mod query;
mod testsupport;

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

use crate::{db::Database, session::SessionEventTx};

use super::{SessionError, SessionMeta, SessionStatus, runtime::SessionRuntime};

#[cfg(target_os = "windows")]
pub(super) const SOFT_STOP_INPUTS: &[&[u8]] = &[&[0x03], &[0x03], &[0x1a, b'\r']];

#[cfg(not(target_os = "windows"))]
pub(super) const SOFT_STOP_INPUTS: &[&[u8]] = &[&[0x03], &[0x03], &[0x04]];

pub(super) const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// A PTY echoes back almost everything the user types, so output landing this
/// soon after a keystroke is attributed to that keystroke instead of to the
/// program. Anything later than this is considered self-driven program output.
pub(super) const INPUT_ECHO_WINDOW: Duration = Duration::from_secs(1);
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

pub(super) type SessionMap = HashMap<String, Arc<SessionHandle>>;

pub(super) struct StoreMutableState {
    pub(super) starting_sessions: HashSet<String>,
    pub(super) evicted_sessions: HashMap<String, Instant>,
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

    pub(super) fn write(&self) -> parking_lot::RwLockWriteGuard<'_, SessionRuntime> {
        self.runtime.write()
    }
}

pub struct SessionStore {
    pub(super) sessions: ArcSwap<SessionMap>,
    pub(super) mutable: TokioMutex<StoreMutableState>,
    pub(super) eviction_ttl: Duration,
    pub(super) db: Arc<Database>,
    pub(super) event_tx: SessionEventTx,
}

#[derive(Debug, Clone)]
pub struct SilentCandidate {
    pub session_id: String,
    pub session_title: Option<String>,
    pub excerpt: String,
    pub output_epoch: Instant,
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

pub(super) fn build_soft_stop_schedule(
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
