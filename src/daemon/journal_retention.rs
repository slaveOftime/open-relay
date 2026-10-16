//! Wall-clock retention sweeper for stopped sessions.
//!
//! The active production lever is **wall-clock retention**: stop the
//! accumulation of killed/failed/stopped sessions by deleting a whole
//! session directory + DB row after a configurable number of days.
//!
//! A second, byte-budget lever exists only as internal plumbing:
//! `crate::session::runtime` still runs a checkpoint-gated sweep that
//! can trim sealed incarnations of a running session's journal. It is
//! disabled in production — the daemon constructs the store with
//! budget `0` and removed config keys fail loudly — because a byte cap
//! cannot bound the *current* incarnation. Do not describe it as a
//! supported live quota; see the compatibility policy and
//! `ARCHITECTURE.md` (“Retention”).
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

/// Delete one session's journal directory + DB row, directory first: if the
/// DB delete then fails, the row still selects the session on the next sweep
/// and the already-missing directory is tolerated (`NotFound`), so a retry
/// converges without ever presenting a half-deleted session as running.
///
/// The id is validated as a single normal path component before any removal:
/// retention must never recursively delete a directory that a corrupted row
/// merely appears to name. Failures are counted per stage so the diagnostic
/// surface can distinguish "could not delete the files" from "could not
/// delete the row".
async fn sweep_one(
    db: &Database,
    sessions_dir: &std::path::Path,
    session_id: &str,
) -> crate::error::Result<()> {
    let Some(journal_dir) = safe_session_dir(sessions_dir, session_id) else {
        crate::metrics::count("retention_sweep_failures", Some("unsafe_id"));
        return Err(crate::error::AppError::Protocol(format!(
            "refusing to sweep session with an unsafe id: {session_id:?}"
        )));
    };
    if let Err(err) = tokio::fs::remove_dir_all(&journal_dir).await {
        // ENOENT means a concurrent manual `oly rm` already cleaned
        // it up; that's the only error we treat as success-on-disk.
        if err.kind() != std::io::ErrorKind::NotFound {
            crate::metrics::count("retention_sweep_failures", Some("dir"));
            return Err(err.into());
        }
    }
    match db.delete_session_by_id(session_id).await {
        Ok(_) => {
            crate::metrics::count("retention_sessions_evicted", None);
            Ok(())
        }
        Err(err) => {
            crate::metrics::count("retention_sweep_failures", Some("db"));
            Err(err)
        }
    }
}

/// The session directory for `session_id`, but only when the id is a single
/// normal path component (no separators, `..`, roots, or prefixes). Anything
/// else is rejected rather than joined: `sessions_dir.join(id)` with a
/// traversal id would name a directory outside the sessions dir.
fn safe_session_dir(sessions_dir: &std::path::Path, session_id: &str) -> Option<PathBuf> {
    use std::path::Component;

    let mut components = std::path::Path::new(session_id).components();
    let first = components.next()?;
    if components.next().is_some() {
        return None;
    }
    if !matches!(first, Component::Normal(_))
        || first.as_os_str() != std::ffi::OsStr::new(session_id)
    {
        return None;
    }
    Some(sessions_dir.join(session_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionMeta, SessionStatus};
    use chrono::Utc;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            title: None,
            tags: vec![],
            command: "cmd".to_string(),
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
        }
    }

    struct Fixture {
        db: Arc<Database>,
        sessions_dir: PathBuf,
        root: PathBuf,
    }

    async fn fixture(name: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!(
            "oly_retention_{name}_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let sessions_dir = root.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let db = Arc::new(
            Database::open(&root.join("oly.db"), sessions_dir.clone())
                .await
                .expect("open test db"),
        );
        Fixture {
            db,
            sessions_dir,
            root,
        }
    }

    #[tokio::test]
    async fn sweep_removes_session_dir_and_row() {
        let fixture = fixture("happy").await;
        let id = "stopped111";
        fixture.db.insert_session(&meta(id)).await.unwrap();
        let session_dir = fixture.sessions_dir.join(id);
        std::fs::create_dir_all(session_dir.join("journal")).unwrap();
        std::fs::write(session_dir.join("journal").join("manifest.log"), b"x").unwrap();

        sweep_one(&fixture.db, &fixture.sessions_dir, id)
            .await
            .expect("sweep");

        assert!(!session_dir.exists());
        assert!(fixture.db.get_session(id).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&fixture.root);
    }

    #[tokio::test]
    async fn sweep_converges_when_the_directory_is_already_gone() {
        let fixture = fixture("retry").await;
        let id = "stopped222";
        fixture.db.insert_session(&meta(id)).await.unwrap();

        // Simulates a retry after a previous attempt deleted the directory
        // but failed before the row was removed.
        sweep_one(&fixture.db, &fixture.sessions_dir, id)
            .await
            .expect("retry must converge");

        assert!(fixture.db.get_session(id).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&fixture.root);
    }

    #[tokio::test]
    async fn sweep_failure_keeps_the_row_for_the_next_interval() {
        let fixture = fixture("dir_failure").await;
        let id = "stopped333";
        fixture.db.insert_session(&meta(id)).await.unwrap();
        // A file where the session directory belongs makes the directory
        // removal fail; the row must survive so the next sweep retries.
        std::fs::write(fixture.sessions_dir.join(id), b"obstacle").unwrap();

        let err = sweep_one(&fixture.db, &fixture.sessions_dir, id)
            .await
            .expect_err("directory removal must fail");
        let _ = err;
        assert!(
            fixture.db.get_session(id).await.unwrap().is_some(),
            "row must remain for the next retry"
        );
        assert!(fixture.sessions_dir.join(id).is_file());
        let _ = std::fs::remove_dir_all(&fixture.root);
    }

    #[tokio::test]
    async fn sweep_refuses_ids_that_escape_the_sessions_dir() {
        let fixture = fixture("traversal").await;
        let victim = fixture.root.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        let id = "../victim";
        fixture.db.insert_session(&meta(id)).await.unwrap();

        let err = sweep_one(&fixture.db, &fixture.sessions_dir, id)
            .await
            .expect_err("traversal id must be refused");
        assert!(err.to_string().contains("unsafe id"), "{err}");
        assert!(victim.is_dir(), "victim directory must survive");
        assert!(fixture.db.get_session(id).await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(&fixture.root);
    }

    #[test]
    fn safe_session_dir_accepts_only_single_normal_components() {
        let base = std::path::Path::new("base");
        assert_eq!(safe_session_dir(base, "abc123"), Some(base.join("abc123")));
        for bad in ["", ".", "..", "a/b", "/abs", "../x"] {
            assert_eq!(safe_session_dir(base, bad), None, "accepted {bad:?}");
        }
        // A backslash is a separator on Windows, so it must be rejected like
        // `/`; on Unix it is an ordinary character inside one normal
        // component and the join still stays inside `base`.
        #[cfg(windows)]
        assert_eq!(
            safe_session_dir(base, "a\\b"),
            None,
            "accepted a backslash separator"
        );
        #[cfg(not(windows))]
        assert_eq!(safe_session_dir(base, "a\\b"), Some(base.join("a\\b")));
    }
}
