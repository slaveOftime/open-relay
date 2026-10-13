//! Wall-clock retention sweeper for stopped sessions.
//!
//! Companion to the live-session byte-budget retention in
//! `crate::session::runtime` — that one trims the oldest incarnations
//! of a running session's journal at every checkpoint; this one
//! deletes the whole journal + DB row of a stopped session after a
//! configurable number of days.
//!
//! Two orthogonal levers for two different problems:
//!
//! - **byte budget**: cap on persisted journal size, checked at each
//!   checkpoint, so a long-running session cannot fill the disk while running.
//! - **wall-clock retention**: cap on stopped-session age, so killed/failed
//!   CI runs do not accumulate indefinitely.
//!
//! The sweeper runs every [`JOURNAL_RETENTION_SWEEP_INTERVAL`]
//! (one hour by default) and is intentionally synchronous against
//! SQLite so a config hot-reload, a concurrent manual `oly rm`, or a
//! process restart cannot see a half-deleted session. Live sessions
//! are filtered out at the SQL layer (`status IN ('stopped', 'killed',
//! 'failed')`); running work is never auto-deleted because the user
//! did not consent to that.

use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use tracing::{debug, info, warn};

use crate::{config::LiveConfig, db::Database};

/// Default sweep cadence. One hour keeps the sweep itself cheap
/// (status + ended_at index lookups) while still bounding the age of
/// any single stopped session to `journal_retention_days + 1 hour` in
/// the absolute worst case.
pub(super) const JOURNAL_RETENTION_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

pub(super) async fn run_journal_retention_sweeper(
    live: LiveConfig,
    db: Arc<Database>,
    sessions_dir: PathBuf,
) {
    loop {
        tokio::time::sleep(JOURNAL_RETENTION_SWEEP_INTERVAL).await;

        let config = live.get();
        let retention_days = config.limits.journal_retention_days;
        if retention_days == 0 {
            debug!("journal retention sweeper: disabled (0 days)");
            continue;
        }

        // Cutoff is computed against the daemon's wall clock at the
        // sweep instant, not against `created_at`: the cap applies to
        // session *age* in the sense of "how long has this session
        // been finished", which is `ended_at` for stopped sessions.
        let cutoff: DateTime<Utc> = Utc::now() - chrono::Duration::days(retention_days.into());

        match db.list_stopped_sessions_older_than(cutoff).await {
            Ok(candidates) if candidates.is_empty() => {
                debug!(retention_days, "journal retention sweep: nothing to evict");
            }
            Ok(candidates) => {
                let total = candidates.len();
                let mut evicted: usize = 0;
                let mut skipped: usize = 0;
                for (id, meta) in candidates {
                    if meta.notifications_enabled
                        && !matches!(
                            meta.status,
                            crate::session::SessionStatus::Killed
                                | crate::session::SessionStatus::Failed
                                | crate::session::SessionStatus::Stopped
                        )
                    {
                        // Defence-in-depth: SQL already filters these
                        // statuses, but if the schema ever loosens the
                        // constraint we still won't sweep a live
                        // session. The notification flag itself is
                        // irrelevant to retention — what matters is
                        // status.
                        skipped += 1;
                        continue;
                    }
                    match sweep_one(&db, &sessions_dir, &id).await {
                        Ok(()) => evicted += 1,
                        Err(err) => {
                            warn!(
                                session_id = %id,
                                error = %err,
                                "journal retention sweeper failed for session; \
                                 will retry next interval"
                            );
                            skipped += 1;
                        }
                    }
                }
                info!(
                    retention_days,
                    cutoff = %cutoff.to_rfc3339(),
                    total,
                    evicted,
                    skipped,
                    "journal retention sweep complete"
                );
            }
            Err(err) => {
                warn!(
                    error = %err,
                    "journal retention sweeper: DB query failed; \
                     skipping this interval"
                );
            }
        }
    }
}

/// Delete one session's journal directory + DB row. The journal
/// directory is removed first because it's the bulk of the disk cost;
/// if the DB delete afterwards fails, the next sweep re-runs and
/// treats the lingering journal as orphaned (no DB row means no
/// candidate selection, so the orphan is harmless — it just wastes
/// disk until the operator reruns the sweep with `journal_retention_days=0`
/// followed by a manual cleanup, or removes it by hand).
async fn sweep_one(
    db: &Database,
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> crate::error::Result<()> {
    let journal_dir = sessions_dir.join(session_id);
    if let Err(err) = tokio::fs::remove_dir_all(&journal_dir).await {
        // ENOENT means a concurrent manual `oly rm` already cleaned
        // it up; that's the only error we treat as success-on-disk.
        if err.kind() != std::io::ErrorKind::NotFound {
            return Err(err.into());
        }
    }
    db.delete_session_by_id(session_id).await?;
    Ok(())
}
