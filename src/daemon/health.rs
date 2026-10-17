//! Daemon health: the read-only diagnostic summary shared by local IPC,
//! HTTP (`/api/ready`), and federated status requests.
//!
//! Liveness (the process answers) and readiness (supervised components are
//! working, disk state is known when the guard is on) are separate answers.
//! Nothing in this module mutates daemon state and nothing collects terminal
//! content, input, environment, identity seeds, or credentials.

use std::sync::Arc;
use std::time::SystemTime;

use crate::{
    config::AppConfig,
    db::Database,
    disk::DiskMonitor,
    protocol::{
        AffectedSession, ComponentStatus, DaemonStatusReport, DiskStatus, ListQuery, ListSortField,
        SortOrder,
    },
    session::SessionStore,
    storage,
};
use chrono::{DateTime, Utc};

use super::{
    components::{ComponentRegistry, Readiness},
    crash,
};

/// Maximum number of affected sessions listed in one report.
pub(crate) const MAX_AFFECTED_SESSIONS: usize = 20;

/// Handles every diagnostic surface needs. Constructed once by the daemon
/// at startup and cloned into the IPC dispatch and the HTTP state.
#[derive(Clone)]
pub(crate) struct DaemonHealth {
    pub(crate) components: Arc<ComponentRegistry>,
    pub(crate) disk: Arc<DiskMonitor>,
}

impl DaemonHealth {
    pub(crate) fn new(components: Arc<ComponentRegistry>, disk: Arc<DiskMonitor>) -> Self {
        Self { components, disk }
    }

    pub(crate) fn readiness(&self) -> Readiness {
        self.with_disk_readiness(self.components.readiness(), true)
    }

    pub(crate) fn public_readiness(&self) -> Readiness {
        self.with_disk_readiness(self.components.public_readiness(), false)
    }

    fn with_disk_readiness(&self, mut readiness: Readiness, include_details: bool) -> Readiness {
        let decision = self.disk.guard_decision();
        if let Some(reason) = decision.refusal_reason() {
            readiness.ready = false;
            readiness.reasons.push(if include_details {
                format!("disk guard: {reason}")
            } else {
                format!("disk guard is {}", decision.label())
            });
        }
        readiness
    }

    /// Assemble the diagnostic summary. Read-only; degraded inputs are
    /// reported as diagnostic_errors, never a success-shaped empty result.
    pub(crate) async fn report(
        &self,
        config: &AppConfig,
        store: &SessionStore,
        db: &Database,
    ) -> DaemonStatusReport {
        let readiness = self.readiness();
        let components = self
            .components
            .reports()
            .iter()
            .map(component_status)
            .collect();
        let disk = self.disk_status();
        let mut diagnostic_errors = Vec::new();
        let started_at = match storage::read_daemon_info(&config.paths.info_file) {
            Ok(info) => info.map(|info| info.started_at),
            Err(err) => {
                diagnostic_errors.push(format!("could not read daemon info: {err}"));
                None
            }
        };
        let failure_records = vec![
            config.paths.state_dir.join("logs").display().to_string(),
            crash::crash_log_path(&config.paths.state_dir)
                .display()
                .to_string(),
            crash::stderr_log_path(&config.paths.state_dir)
                .display()
                .to_string(),
        ];
        let (affected_sessions, affected_sessions_total) =
            match collect_affected_sessions(store, db).await {
                Ok((sessions, total)) => (sessions, Some(total)),
                Err(err) => {
                    diagnostic_errors.push(format!("could not query affected sessions: {err}"));
                    (Vec::new(), None)
                }
            };

        DaemonStatusReport {
            daemon_pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at,
            alive: true,
            ready: readiness.ready,
            unready_reasons: readiness.reasons,
            components,
            disk,
            failure_records,
            affected_sessions,
            affected_sessions_total,
            diagnostic_errors,
        }
    }

    /// Disk portion of the report. Unknown and never-sampled states stay
    /// `None`/absent instead of becoming plausible numbers.
    fn disk_status(&self) -> DiskStatus {
        let sample = self.disk.latest();
        let decision = self.disk.guard_decision();
        let stale = sample
            .as_ref()
            .is_some_and(|sample| self.disk.sample_is_stale(sample));
        let fresh = sample.as_ref().filter(|_| !stale);
        DiskStatus {
            free_bytes: fresh.and_then(|sample| sample.free_bytes),
            free_unknown_reason: if stale {
                Some("state volume sample is stale; wait for a fresh sample".to_string())
            } else {
                sample
                    .as_ref()
                    .and_then(|sample| sample.free_unknown_reason.clone())
            },
            sampled_at: sample.as_ref().map(|sample| to_utc(sample.sampled_at)),
            journal_bytes: fresh.map(|sample| sample.breakdown.journal_bytes),
            uploads_bytes: fresh.map(|sample| sample.breakdown.uploads_bytes),
            sqlite_bytes: fresh.map(|sample| sample.breakdown.sqlite_bytes),
            logs_bytes: fresh.map(|sample| sample.breakdown.logs_bytes),
            breakdown_partial_reason: sample
                .as_ref()
                .and_then(|sample| sample.breakdown_partial_reason.clone()),
            guard_enabled: self.disk.guard_enabled(),
            guard_decision: decision.label().to_string(),
            guard_refusal_reason: decision.refusal_reason(),
            warn_below_bytes: self.disk.warn_threshold(),
            warning_active: self.disk.warning().is_some(),
        }
    }
}

fn component_status(report: &super::components::ComponentReport) -> ComponentStatus {
    ComponentStatus {
        name: report.name.to_string(),
        state: report.state.as_str().to_string(),
        ready_critical: report.ready_critical,
        failures: report.failures,
        last_success_at: report.last_success_at.map(to_utc),
        last_failure: report
            .last_failure
            .as_ref()
            .map(|failure| failure.reason.clone()),
        last_failure_at: report
            .last_failure
            .as_ref()
            .map(|failure| to_utc(failure.at)),
    }
}

fn to_utc(time: SystemTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(time)
}

/// In-flight start reservations, then failed sessions newest first.
/// Reservations are volatile, not evidence that a previous daemon crashed.
/// Bounded; query failures stay errors rather than becoming a zero total.
async fn collect_affected_sessions(
    store: &SessionStore,
    db: &Database,
) -> crate::error::Result<(Vec<AffectedSession>, usize)> {
    let mut affected = Vec::new();

    let mut starting = store.starting_session_ids().await;
    starting.sort();
    let starting_total = starting.len();
    affected.extend(
        starting
            .into_iter()
            .take(MAX_AFFECTED_SESSIONS)
            .map(|id| AffectedSession {
                id,
                status: "starting".to_string(),
                reason: "start is in progress (reserved, not yet committed)".to_string(),
            }),
    );

    let query = ListQuery {
        search: None,
        tags: Vec::new(),
        statuses: vec!["failed".to_string()],
        since: None,
        until: None,
        limit: MAX_AFFECTED_SESSIONS,
        offset: 0,
        sort: ListSortField::CreatedAt,
        order: SortOrder::Desc,
    };
    let summaries = db.list_summaries_without_offsets(&query).await?;
    affected.extend(summaries.into_iter().map(|summary| AffectedSession {
        id: summary.id,
        status: summary.status,
        reason: "session ended in failure".to_string(),
    }));
    let failed_total = db.count_sessions_by_status("failed").await? as usize;

    let total = starting_total + failed_total;
    affected.truncate(MAX_AFFECTED_SESSIONS);
    Ok((affected, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{LimitsConfig, LiveConfig},
        disk::{DiskMonitor, DiskSample, SpaceProbe},
        session::{
            SessionMeta, SessionStatus,
            store::testsupport::{make_test_config, make_test_db, store_with},
        },
    };
    use chrono::Utc;
    use std::{path::Path, sync::Arc, time::SystemTime};

    struct FixedProbe {
        result: std::result::Result<u64, String>,
    }

    impl SpaceProbe for FixedProbe {
        fn free_bytes(&self, _path: &Path) -> std::result::Result<u64, String> {
            self.result.clone()
        }
    }

    fn test_limits() -> LimitsConfig {
        let mut limits = make_test_config(50).limits;
        limits.disk_guard_enabled = false;
        limits.disk_min_free_bytes = 1024;
        limits.disk_warn_free_bytes = 2 * 1024;
        limits.disk_sample_interval_seconds = 30;
        limits.disk_sample_max_age_seconds = 120;
        limits
    }

    fn monitor(limits: LimitsConfig, probe: std::result::Result<u64, String>) -> Arc<DiskMonitor> {
        let mut config = make_test_config(50);
        config.limits = limits;
        Arc::new(DiskMonitor::new(
            LiveConfig::from_arc(Arc::new(config)),
            Box::new(FixedProbe { result: probe }),
        ))
    }

    fn failed_meta(id: &str) -> SessionMeta {
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
            status: SessionStatus::Failed,
            pid: None,
            exit_code: Some(1),
            notifications_enabled: true,
            foreground_color: None,
            background_color: None,
        }
    }

    #[tokio::test]
    async fn report_names_failed_components_and_their_reason() {
        // The supervisor's own tests cover bounded restarts and panics; this
        // drives the registry directly into the states the daemon would
        // report: one failed ready-critical component and one healthy one.
        let registry = Arc::new(ComponentRegistry::new());
        registry.register("http", true);
        registry.register("notification_monitor", true);
        registry.mark_running_for_test("notification_monitor");
        registry.fail_for_test(
            "http",
            "failed to bind HTTP server at 127.0.0.1:15443: address in use",
        );

        let health = DaemonHealth::new(
            Arc::clone(&registry),
            monitor(test_limits(), Ok(512 * 1024 * 1024 * 1024)),
        );
        let config = make_test_config(50);
        let db = make_test_db().await;
        let store = Arc::new(store_with(vec![], db.clone()));
        let report = health.report(&config, &store, &db).await;

        assert!(report.alive, "the daemon answered, so liveness is true");
        assert!(
            !report.ready,
            "a failed ready-critical component is not ready"
        );
        assert!(
            report
                .unready_reasons
                .iter()
                .any(|reason| reason.contains("http") && reason.contains("failed to bind")),
            "unready reasons must name the component and the reason: {:?}",
            report.unready_reasons
        );

        let http = report
            .components
            .iter()
            .find(|component| component.name == "http")
            .expect("http component reported");
        assert_eq!(http.state, "failed");
        assert!(http.failures >= 1);
        assert!(
            http.last_failure
                .as_deref()
                .is_some_and(|reason| reason.contains("address in use"))
        );

        let notifier = report
            .components
            .iter()
            .find(|component| component.name == "notification_monitor")
            .expect("notification component reported");
        assert_eq!(notifier.state, "running");
        assert_eq!(notifier.failures, 0);
        assert!(notifier.last_success_at.is_some());
    }

    #[tokio::test]
    async fn report_keeps_unknown_disk_states_unknown_until_sampled() {
        let db = make_test_db().await;
        let store = Arc::new(store_with(vec![], db.clone()));
        let config = make_test_config(50);

        // Guard on + probe failure: the report must say unknown, with the
        // probe's reason, and never a plausible number.
        let mut guarding = test_limits();
        guarding.disk_guard_enabled = true;
        let unknown = monitor(guarding.clone(), Err("no such mount".to_string()));
        unknown.refresh();
        let health = DaemonHealth::new(Arc::new(ComponentRegistry::new()), unknown);
        let report = health.report(&config, &store, &db).await;
        assert_eq!(report.disk.free_bytes, None);
        assert_eq!(
            report.disk.free_unknown_reason.as_deref(),
            Some("no such mount")
        );
        assert_eq!(report.disk.guard_decision, "unknown");
        assert!(report.disk.guard_enabled);
        assert!(
            report
                .disk
                .guard_refusal_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("no such mount"))
        );

        // Successful sample: numbers appear, warning is independent of the
        // guard, and the breakdown is exact (an empty state dir is 0, not
        // "unknown").
        let healthy = monitor(test_limits(), Ok(1024));
        let sample = healthy.refresh();
        assert_eq!(sample.breakdown_partial_reason, None);
        let health = DaemonHealth::new(Arc::new(ComponentRegistry::new()), healthy);
        let report = health.report(&config, &store, &db).await;
        assert_eq!(report.disk.free_bytes, Some(1024));
        assert_eq!(report.disk.breakdown_partial_reason, None);
        assert_eq!(report.disk.journal_bytes, Some(0));
        assert_eq!(report.disk.guard_decision, "disabled");
        assert!(!report.disk.guard_enabled);
        assert!(report.disk.guard_refusal_reason.is_none());
        assert!(
            report.disk.warning_active,
            "1024 free is below the 2048 warning line"
        );
        assert_eq!(report.disk.warn_below_bytes, 2 * 1024);
    }

    #[tokio::test]
    async fn report_points_at_uncommitted_starts_and_failed_sessions() {
        let db = make_test_db().await;
        db.insert_session(&failed_meta("bad1234")).await.unwrap();
        let store = Arc::new(store_with(vec![], db.clone()));
        store.reserve_starting_session_for_test("stuck56").await;
        let config = make_test_config(50);
        let health = DaemonHealth::new(
            Arc::new(ComponentRegistry::new()),
            monitor(test_limits(), Ok(8 * 1024 * 1024 * 1024)),
        );

        let report = health.report(&config, &store, &db).await;
        assert_eq!(report.affected_sessions_total, Some(2));
        assert!(report.affected_sessions.iter().any(|session| {
            session.id == "stuck56"
                && session.status == "starting"
                && session.reason.contains("not yet committed")
        }));
        assert!(report.affected_sessions.iter().any(|session| {
            session.id == "bad1234"
                && session.status == "failed"
                && session.reason.contains("ended in failure")
        }));

        // Failure records point at the real log locations, and the version
        // is the daemon's own.
        assert_eq!(report.version, env!("CARGO_PKG_VERSION"));
        assert!(
            report
                .failure_records
                .iter()
                .any(|path| path.ends_with("daemon-crash.log"))
        );
        assert!(
            report
                .failure_records
                .iter()
                .any(|path| path.ends_with("daemon-stderr.log"))
        );
    }

    #[tokio::test]
    async fn report_caps_affected_sessions_and_reports_the_total() {
        let db = make_test_db().await;
        for index in 0..(MAX_AFFECTED_SESSIONS + 3) {
            db.insert_session(&failed_meta(&format!("failed{index:02}")))
                .await
                .unwrap();
        }
        let store = Arc::new(store_with(vec![], db.clone()));
        let config = make_test_config(50);
        let health = DaemonHealth::new(
            Arc::new(ComponentRegistry::new()),
            monitor(test_limits(), Ok(8 * 1024 * 1024 * 1024)),
        );

        let report = health.report(&config, &store, &db).await;
        assert_eq!(report.affected_sessions.len(), MAX_AFFECTED_SESSIONS);
        assert_eq!(
            report.affected_sessions_total,
            Some(MAX_AFFECTED_SESSIONS + 3)
        );
    }

    #[test]
    fn stale_disk_is_unknown_even_with_guard_disabled() {
        let disk = monitor(test_limits(), Ok(1));
        disk.store_sample_for_test(DiskSample {
            sampled_at: SystemTime::now() - std::time::Duration::from_secs(600),
            free_bytes: Some(1),
            free_unknown_reason: None,
            breakdown: Default::default(),
            breakdown_partial_reason: None,
        });
        let health = DaemonHealth::new(Arc::new(ComponentRegistry::new()), disk);
        let status = health.disk_status();
        assert_eq!(status.free_bytes, None);
        assert!(status.free_unknown_reason.unwrap().contains("stale"));
        assert_eq!(status.journal_bytes, None);
        assert!(!status.warning_active);
        assert_eq!(status.guard_decision, "disabled");
    }

    #[test]
    fn enabled_disk_guard_gates_readiness_with_redacted_public_reasons() {
        let mut limits = test_limits();
        limits.disk_guard_enabled = true;
        let disk = monitor(limits, Err("private/path: probe failed".into()));
        disk.refresh();
        let health = DaemonHealth::new(Arc::new(ComponentRegistry::new()), disk);
        assert!(!health.readiness().ready);
        assert!(health.readiness().reasons[0].contains("private/path"));
        let public = health.public_readiness();
        assert!(!public.ready);
        assert_eq!(public.reasons, ["disk guard is unknown"]);
    }

    #[tokio::test]
    async fn failed_session_queries_are_unknown_not_a_fabricated_zero() {
        let root = std::env::temp_dir().join(format!("oly_bad_health_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("oly.db");
        let db = Database::open(&path, root.join("sessions")).await.unwrap();
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        sqlx::query("DROP TABLE sessions")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let db = Arc::new(db);
        let store = store_with(vec![], db.clone());
        let health = DaemonHealth::new(
            Arc::new(ComponentRegistry::new()),
            monitor(test_limits(), Ok(8192)),
        );
        let report = health.report(&make_test_config(50), &store, &db).await;
        assert!(report.alive);
        assert_eq!(report.affected_sessions_total, None);
        assert!(
            report
                .diagnostic_errors
                .iter()
                .any(|err| err.contains("affected sessions"))
        );
    }

    #[test]
    fn disk_sample_age_is_zero_for_future_timestamps() {
        let sample = DiskSample {
            sampled_at: SystemTime::now() + std::time::Duration::from_secs(30),
            free_bytes: Some(1),
            free_unknown_reason: None,
            breakdown: Default::default(),
            breakdown_partial_reason: None,
        };
        assert_eq!(sample.age(SystemTime::now()), std::time::Duration::ZERO);
    }
}
