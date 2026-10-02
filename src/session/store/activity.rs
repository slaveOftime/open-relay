//! Daemon-wide sampler for lightweight activity events.
//!
//! The sampler walks in-memory runtimes every `SAMPLER_INTERVAL` and emits
//! one [`SessionEvent::SessionActivity`] batch covering only the sessions
//! whose cumulative byte counter (`SessionRuntime::last_total_bytes`)
//! changed since the previous tick. The owning daemon produces activity;
//! the primary rebroadcasts it to its connected browsers unchanged. The
//! frontend applies activity in place to already-known summaries; the
//! sampler is not allowed to add or remove rows from the public
//! inventory.
//!
//! The sampler runs **once per daemon**, owned by the daemon lifecycle
//! module, so even a secondary started with `--no-http` keeps its event
//! channel alive and the federation relay can forward meaningful
//! output-derived events. Activity is intentionally cheap: a single
//! `parking_lot::RwLock` read on each runtime, no SQL, no journals, no
//! terminal engine snapshot.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::Utc;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info};

use crate::session::{
    SessionActivityBatch, SessionActivitySample, SessionEvent, SessionEventTx, SessionStore,
};

/// Sampling cadence. Chosen to match `SparklineStore::SPARKLINE_BUCKET_MS`
/// in the web client so each batch lands in a distinct visible bucket
/// without rounding skew.
pub const SAMPLER_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
struct ActivityFingerprint {
    last_total_bytes: u64,
    last_output_at: Option<std::time::Instant>,
}

impl SessionStore {
    /// Activity sampler loop. Designed to be spawned exactly once per
    /// daemon process. The loop never touches the database, the journal,
    /// or the terminal engine: it reads the per-runtime counters under
    /// their read lock and emits a `SessionActivity` batch only when at
    /// least one session changed. It is also resilient to broadcast lag
    /// — the sender is non-blocking, so a slow consumer never holds the
    /// sampler up.
    pub async fn run_activity_sampler(self: Arc<Self>, event_tx: SessionEventTx) {
        info!(
            interval_ms = SAMPLER_INTERVAL.as_millis() as u64,
            "session activity sampler started"
        );
        let mut ticker = tokio::time::interval(SAMPLER_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut last_seen: HashMap<String, ActivityFingerprint> = HashMap::new();

        loop {
            ticker.tick().await;
            let mut samples: Vec<SessionActivitySample> = Vec::new();
            let now_wall = Utc::now();
            let now_inst = std::time::Instant::now();

            {
                let sessions = self.sessions.load();
                last_seen.retain(|id, _| sessions.contains_key(id));
                for (id, handle) in sessions.iter() {
                    // CR-7: `try_read` (a.k.a. `attempt_read`) lets the
                    // sampler skip a session that is currently mid-write
                    // by the PTY reader instead of stalling for the writer
                    // to release. The next tick will catch it.
                    let Some(rt) = handle.try_read() else {
                        continue;
                    };
                    let fingerprint = ActivityFingerprint {
                        last_total_bytes: rt.last_total_bytes,
                        last_output_at: rt.last_output_epoch,
                    };
                    let changed = match last_seen.get(id) {
                        Some(prev) => {
                            prev.last_total_bytes != fingerprint.last_total_bytes
                                || prev.last_output_at != fingerprint.last_output_at
                        }
                        None => fingerprint.last_total_bytes > 0,
                    };
                    if changed {
                        samples.push(SessionActivitySample {
                            id: id.clone(),
                            last_total_bytes: fingerprint.last_total_bytes,
                            last_output_at: fingerprint.last_output_at.map(|t| {
                                now_wall
                                    - chrono::Duration::from_std(
                                        now_inst.saturating_duration_since(t),
                                    )
                                    .unwrap_or_default()
                            }),
                        });
                    }
                    last_seen.insert(id.clone(), fingerprint);
                }
            }

            if samples.is_empty() {
                continue;
            }

            debug!(samples = samples.len(), "session activity batch assembled");

            let batch = SessionActivityBatch {
                node: None,
                samples,
            };
            // The broadcast channel is bounded; if the consumer is so far
            // behind that `send` would fail, the receivers close the
            // connection and reconnect for a full resync. Activity must
            // NOT be force-pushed through the slow-receiver lag.
            if event_tx.send(SessionEvent::SessionActivity(batch)).is_err() {
                debug!("activity batch dropped — no live receivers (sender is benign)");
            }
        }
    }
}
