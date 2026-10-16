//! Supervision registry for the daemon's long-lived background tasks.
//!
//! Daemon health answers two different questions: is the process alive and
//! answering IPC (`RpcRequest::Health`, `/api/health`), and are the
//! long-lived components actually working (this registry, `/api/ready`,
//! `oly daemon status`). Bare `tokio::spawn` answered neither: a task that
//! panicked or exited early vanished into the logs while health kept
//! reporting success. Every daemon-lifetime component now registers here
//! with its state, last healthy time, and failure reason; a component that
//! stops is restarted a bounded number of times, then marked failed.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime},
};

use tracing::{error, warn};

/// Restarts attempted after the initial run before a component is declared
/// failed. Bounded so a deterministically failing component cannot loop
/// forever.
pub(crate) const MAX_COMPONENT_RESTARTS: u32 = 3;

/// What a supervised component reports when it stops. `Ok(())` means the
/// future returned on its own — for daemon-lifetime loops that is itself
/// abnormal; `Err(reason)` carries why it stopped.
pub(crate) type ComponentRun = std::result::Result<(), String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComponentState {
    /// Registered but not started yet; readiness waits for it.
    Starting,
    Running,
    /// Stopped/failed and waiting for a restart attempt.
    Restarting,
    /// Restart budget exhausted.
    Failed,
    /// Intentionally absent in this daemon configuration (e.g. HTTP under
    /// `--no-http`); excluded from readiness.
    Disabled,
}

impl ComponentState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
        }
    }

    /// Running or intentionally disabled components satisfy readiness.
    pub(crate) fn satisfied(self) -> bool {
        matches!(self, Self::Running | Self::Disabled)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ComponentFailure {
    pub(crate) reason: String,
    pub(crate) at: SystemTime,
}

#[derive(Debug, Clone)]
pub(crate) struct ComponentReport {
    pub(crate) name: &'static str,
    pub(crate) state: ComponentState,
    /// Whether a non-satisfied state degrades daemon readiness.
    pub(crate) ready_critical: bool,
    /// Last time the component started healthy (initial start or restart).
    pub(crate) last_success_at: Option<SystemTime>,
    pub(crate) failures: u32,
    pub(crate) last_failure: Option<ComponentFailure>,
}

#[derive(Debug)]
struct ComponentSlot {
    state: ComponentState,
    ready_critical: bool,
    last_success_at: Option<SystemTime>,
    failures: u32,
    last_failure: Option<ComponentFailure>,
}

impl ComponentSlot {
    fn new(ready_critical: bool) -> Self {
        Self {
            state: ComponentState::Starting,
            ready_critical,
            last_success_at: None,
            failures: 0,
            last_failure: None,
        }
    }

    fn report(&self, name: &'static str) -> ComponentReport {
        ComponentReport {
            name,
            state: self.state,
            ready_critical: self.ready_critical,
            last_success_at: self.last_success_at,
            failures: self.failures,
            last_failure: self.last_failure.clone(),
        }
    }
}

/// Registry of supervised daemon components. Cheap to clone via `Arc`;
/// snapshots are plain data.
#[derive(Debug, Default)]
pub(crate) struct ComponentRegistry {
    slots: Mutex<BTreeMap<&'static str, ComponentSlot>>,
}

#[derive(Debug, Clone)]
pub(crate) struct Readiness {
    pub(crate) ready: bool,
    /// One entry per ready-critical component that is not satisfied.
    pub(crate) reasons: Vec<String>,
}

impl ComponentRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn slots(&self) -> MutexGuard<'_, BTreeMap<&'static str, ComponentSlot>> {
        self.slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Register a component. Background components register disabled/on-demand.
    pub(crate) fn register(&self, name: &'static str, ready_critical: bool) {
        self.slots()
            .entry(name)
            .or_insert_with(|| ComponentSlot::new(ready_critical));
    }

    /// Mark a component intentionally absent (e.g. HTTP under `--no-http`).
    pub(crate) fn mark_disabled(&self, name: &'static str) {
        let mut slots = self.slots();
        let slot = slots
            .entry(name)
            .or_insert_with(|| ComponentSlot::new(false));
        slot.state = ComponentState::Disabled;
    }

    fn mark_running(&self, name: &'static str) {
        let mut slots = self.slots();
        let slot = slots
            .entry(name)
            .or_insert_with(|| ComponentSlot::new(true));
        slot.state = ComponentState::Running;
        slot.last_success_at = Some(SystemTime::now());
    }

    fn record_failure(&self, name: &'static str, reason: String) {
        let mut slots = self.slots();
        let slot = slots
            .entry(name)
            .or_insert_with(|| ComponentSlot::new(true));
        slot.failures = slot.failures.saturating_add(1);
        slot.last_failure = Some(ComponentFailure {
            reason,
            at: SystemTime::now(),
        });
        slot.state = ComponentState::Restarting;
    }

    fn mark_failed(&self, name: &'static str, reason: String) {
        self.record_failure(name, reason);
        let mut slots = self.slots();
        if let Some(slot) = slots.get_mut(name) {
            slot.state = ComponentState::Failed;
        }
    }

    /// Snapshot every registered component, ordered by name.
    pub(crate) fn reports(&self) -> Vec<ComponentReport> {
        let slots = self.slots();
        slots.iter().map(|(name, slot)| slot.report(name)).collect()
    }

    /// Test-only: put a component in `Running` without spawning it.
    #[cfg(test)]
    pub(crate) fn mark_running_for_test(&self, name: &'static str) {
        self.mark_running(name);
    }

    /// Test-only: drive a component into `Failed` with a reason.
    #[cfg(test)]
    pub(crate) fn fail_for_test(&self, name: &'static str, reason: &str) {
        self.mark_failed(name, reason.to_string());
    }

    /// Readiness: every ready-critical component is running (or disabled by
    /// configuration). Liveness is a separate question and stays true while
    /// this registry reports failures.
    pub(crate) fn readiness(&self) -> Readiness {
        let slots = self.slots();
        let reasons = slots
            .iter()
            .filter(|(_, slot)| slot.ready_critical && !slot.state.satisfied())
            .map(|(name, slot)| {
                let mut reason = format!("component '{name}' is {}", slot.state.as_str());
                if let Some(failure) = &slot.last_failure {
                    reason.push_str(&format!(": {}", failure.reason));
                }
                reason
            })
            .collect::<Vec<_>>();
        Readiness {
            ready: reasons.is_empty(),
            reasons,
        }
    }
}

/// Spawn a daemon-lifetime component under supervision: register it, run
/// the (re-creatable) future, restart with linear backoff on exit/panic,
/// and mark it failed with the reason once the restart budget is spent.
pub(crate) fn spawn_supervised<F, Fut>(
    registry: Arc<ComponentRegistry>,
    name: &'static str,
    ready_critical: bool,
    mut run: F,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ComponentRun> + Send + 'static,
{
    registry.register(name, ready_critical);
    // Invoke the factory once at wiring time, before the supervisor task is
    // spawned: component setup that must be synchronous (the SSE encoder
    // subscribing to the event broadcast) is then done before the caller
    // continues, so no event published after wiring can be dropped by a
    // not-yet-polled task. Restarts invoke the factory again.
    let mut next = Some(run());
    tokio::spawn(async move {
        let mut restarts: u32 = 0;
        loop {
            registry.mark_running(name);
            let future = match next.take() {
                Some(future) => future,
                None => run(),
            };
            let reason = match tokio::spawn(future).await {
                Ok(Ok(())) => "component exited unexpectedly".to_string(),
                Ok(Err(reason)) => reason,
                Err(join) => join_failure_reason(join),
            };
            if restarts >= MAX_COMPONENT_RESTARTS {
                registry.mark_failed(name, reason.clone());
                error!(
                    component = name,
                    reason = %reason,
                    "component failed permanently; daemon readiness degraded"
                );
                return;
            }
            restarts += 1;
            registry.record_failure(name, reason.clone());
            warn!(
                component = name,
                reason = %reason,
                restarts,
                "component failed; restarting"
            );
            tokio::time::sleep(restart_backoff(restarts)).await;
        }
    });
}

fn restart_backoff(restarts: u32) -> Duration {
    Duration::from_millis(500 * (1 << restarts.min(3)))
}

fn join_failure_reason(join: tokio::task::JoinError) -> String {
    if join.is_panic() {
        let payload = join.into_panic();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string());
        format!("panicked: {message}")
    } else {
        "task cancelled".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn readiness_tracks_only_ready_critical_components() {
        let registry = ComponentRegistry::new();
        registry.register("activity_sampler", true);
        registry.register("optional", false);

        // Starting is not satisfied yet.
        let readiness = registry.readiness();
        assert!(!readiness.ready);
        assert_eq!(readiness.reasons.len(), 1);

        registry.mark_running("activity_sampler");
        assert!(registry.readiness().ready);

        registry.record_failure("activity_sampler", "tick exploded".to_string());
        let readiness = registry.readiness();
        assert!(!readiness.ready);
        assert!(readiness.reasons[0].contains("activity_sampler"));
        assert!(readiness.reasons[0].contains("restarting"));
        assert!(readiness.reasons[0].contains("tick exploded"));

        // A non-critical failure never gates readiness.
        registry.record_failure("optional", "decorative".to_string());
        registry.mark_running("activity_sampler");
        assert!(registry.readiness().ready);
    }

    #[test]
    fn disabled_components_do_not_gate_readiness() {
        let registry = ComponentRegistry::new();
        registry.register("http", true);
        registry.mark_disabled("http");
        let reports = registry.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, ComponentState::Disabled);
        assert!(registry.readiness().ready);
    }

    #[tokio::test(start_paused = true)]
    async fn supervised_component_restarts_with_backoff_and_recovers() {
        let registry = Arc::new(ComponentRegistry::new());
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        spawn_supervised(registry.clone(), "flaky", true, move || {
            let seen = Arc::clone(&seen);
            async move {
                let attempt = seen.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    return Err("first attempt failed".to_string());
                }
                std::future::pending::<()>().await;
                Ok(())
            }
        });

        // First attempt fails, one bounded restart succeeds and then pends.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let reports = registry.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].state, ComponentState::Running);
        assert_eq!(reports[0].failures, 1);
        assert!(
            reports[0]
                .last_failure
                .as_ref()
                .is_some_and(|failure| failure.reason.contains("first attempt failed"))
        );
        assert!(reports[0].last_success_at.is_some());
        assert!(registry.readiness().ready);
    }

    #[tokio::test(start_paused = true)]
    async fn factory_runs_at_wiring_time_not_at_first_poll() {
        // The SSE encoder subscribes to the event broadcast inside its
        // factory; that setup must be complete when `spawn_supervised`
        // returns, not when the supervisor task first runs.
        let registry = Arc::new(ComponentRegistry::new());
        let invocations = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&invocations);
        spawn_supervised(registry.clone(), "eager_wiring", true, move || {
            seen.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<ComponentRun>()
        });
        assert_eq!(invocations.load(Ordering::SeqCst), 1);
        // Wiring-time setup does not fake readiness: the component is still
        // `starting` until the supervisor task actually runs it.
        let reports = registry.reports();
        assert_eq!(reports[0].state, ComponentState::Starting);
    }

    #[tokio::test(start_paused = true)]
    async fn supervised_panic_is_recorded_then_fails_after_bounded_restarts() {
        let registry = Arc::new(ComponentRegistry::new());
        spawn_supervised(registry.clone(), "panicky", true, || async {
            panic!("deliberate test panic");
        });

        tokio::time::sleep(Duration::from_secs(30)).await;
        let reports = registry.reports();
        assert_eq!(reports[0].state, ComponentState::Failed);
        assert_eq!(reports[0].failures, MAX_COMPONENT_RESTARTS + 1);
        assert!(
            reports[0]
                .last_failure
                .as_ref()
                .is_some_and(|failure| failure.reason.contains("deliberate test panic"))
        );
        let readiness = registry.readiness();
        assert!(!readiness.ready);
        assert!(readiness.reasons[0].contains("failed"));
    }
}
