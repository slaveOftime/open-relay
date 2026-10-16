//! Bounded background sampling of the state volume.
//!
//! Disk capacity is never measured on a hot path: no list/health/status
//! request walks the state directory. A supervised background task samples
//! the volume (free bytes + a size breakdown of journal / uploads / SQLite /
//! logs), caches one [`DiskSample`] with its observation time, and every
//! consumer reads that cache.
//!
//! Unknown and stale samples are first-class states, never numbers: a probe
//! failure is cached with its reason and the free-space gauge is simply not
//! exported (no `0`, no "infinite"). The same rule governs admission — see
//! [`DiskGuardDecision`].

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{Duration, SystemTime},
};

use tracing::{debug, info, warn};

use crate::config::LiveConfig;

/// Upper bound on directory entries visited by one breakdown scan. The
/// sample stays bounded no matter how large the state directory grows; a
/// capped scan is reported as partial instead of pretending to be exact.
pub(crate) const MAX_BREAKDOWN_ENTRIES: usize = 250_000;

/// Minimum accepted background sampling cadence, in seconds.
pub(crate) const MIN_SAMPLE_INTERVAL_SECONDS: u64 = 5;

/// Size breakdown of the state volume, one category per bounded label.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DiskBreakdown {
    /// `sessions/<id>/journal` across all sessions (segments + manifest).
    pub(crate) journal_bytes: u64,
    /// `sessions/<id>/files` (user uploads) across all sessions.
    pub(crate) uploads_bytes: u64,
    /// The SQLite database file plus its `-wal` / `-shm` companions.
    pub(crate) sqlite_bytes: u64,
    /// `logs/` (daemon log + crash/stderr captures).
    pub(crate) logs_bytes: u64,
}

/// One cached observation of the state volume.
#[derive(Debug, Clone)]
pub(crate) struct DiskSample {
    pub(crate) sampled_at: SystemTime,
    /// Free bytes available to the daemon on the volume holding the state
    /// directory. `None` means unknown; [`Self::free_unknown_reason`] says why.
    pub(crate) free_bytes: Option<u64>,
    pub(crate) free_unknown_reason: Option<String>,
    pub(crate) breakdown: DiskBreakdown,
    /// Set when the size scan hit [`MAX_BREAKDOWN_ENTRIES`] or a directory
    /// could not be read; the breakdown is then a lower bound.
    pub(crate) breakdown_partial_reason: Option<String>,
}

impl DiskSample {
    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.sampled_at).unwrap_or_default()
    }
}

/// Free-space query for a directory's volume. Injectable so tests can drive
/// thresholds, unknown results, and staleness without touching a real disk.
pub(crate) trait SpaceProbe: Send + Sync {
    fn free_bytes(&self, path: &Path) -> std::result::Result<u64, String>;
}

/// Real OS probe (Windows `GetDiskFreeSpaceExW`, Unix `statvfs`).
pub(crate) struct OsSpaceProbe;

impl SpaceProbe for OsSpaceProbe {
    fn free_bytes(&self, path: &Path) -> std::result::Result<u64, String> {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);
            let mut free_to_caller: u64 = 0;
            // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
            // call; the only out-parameter we use points to a live local.
            let ok = unsafe {
                GetDiskFreeSpaceExW(
                    wide.as_ptr(),
                    &mut free_to_caller,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(format!(
                    "GetDiskFreeSpaceExW({}) failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            Ok(free_to_caller)
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;

            let mut c_path = path.as_os_str().as_bytes().to_vec();
            c_path.push(0);
            // SAFETY: `c_path` is a NUL-terminated byte string that outlives
            // the call; `stat` points to a live local of the right type.
            let (code, stat) = unsafe {
                let mut stat: libc::statvfs = std::mem::zeroed();
                let code = libc::statvfs(c_path.as_ptr().cast(), &mut stat);
                (code, stat)
            };
            if code != 0 {
                return Err(format!(
                    "statvfs({}) failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            // `f_bavail`/`f_frsize` are `u64` on 64-bit Unix targets and
            // narrower on some 32-bit libc targets: normalize explicitly.
            // The cast is a no-op where they are already `u64`, which is
            // exactly where clippy flags it.
            #[allow(clippy::unnecessary_cast)]
            let free_bytes = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
            Ok(free_bytes)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err("free-space probe is not implemented on this platform".to_string())
        }
    }
}

/// Why a new session would (or would not) be refused on disk grounds.
///
/// Computed from the cached sample only. `Disabled` is the default posture:
/// the guard is opt-in. When enabled, an unknown or stale sample refuses —
/// the guard never assumes free space it cannot observe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiskGuardDecision {
    Disabled,
    Allowed { free_bytes: u64 },
    Unknown { reason: String },
    Stale { age: Duration, max_age: Duration },
    BelowMinimum { free_bytes: u64, min_bytes: u64 },
}

impl DiskGuardDecision {
    /// Stable label for metrics and diagnostics (bounded set).
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Allowed { .. } => "allowed",
            Self::Unknown { .. } => "unknown",
            Self::Stale { .. } => "stale",
            Self::BelowMinimum { .. } => "below_minimum",
        }
    }

    /// Human-readable refusal reason, or `None` when the decision admits a
    /// new session. Unknown and stale samples refuse with their reason: the
    /// guard never assumes free space it cannot observe.
    pub(crate) fn refusal_reason(&self) -> Option<String> {
        match self {
            Self::Disabled | Self::Allowed { .. } => None,
            Self::Unknown { reason } => Some(format!(
                "state volume free space is unknown ({reason}); \
                 wait for a successful sample or disable disk_guard_enabled"
            )),
            Self::Stale { age, max_age } => Some(format!(
                "state volume sample is stale (age {}s, max {}s); \
                 wait for a fresh sample or raise disk_sample_max_age_seconds",
                age.as_secs(),
                max_age.as_secs()
            )),
            Self::BelowMinimum {
                free_bytes,
                min_bytes,
            } => Some(format!(
                "state volume free space {free_bytes} B is below the floor {min_bytes} B"
            )),
        }
    }
}

/// Cached state-volume observer. Cheap to clone via `Arc`.
pub(crate) struct DiskMonitor {
    config: LiveConfig,
    probe: Box<dyn SpaceProbe>,
    latest: Mutex<Option<DiskSample>>,
    /// Whether the last sample was below the warning threshold, so alerts
    /// fire on threshold crossings instead of once per sample.
    warned_low: Mutex<bool>,
}

impl DiskMonitor {
    pub(crate) fn new(config: LiveConfig, probe: Box<dyn SpaceProbe>) -> Self {
        Self {
            config,
            probe,
            latest: Mutex::new(None),
            warned_low: Mutex::new(false),
        }
    }

    fn latest_guard(&self) -> MutexGuard<'_, Option<DiskSample>> {
        self.latest
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The most recent sample, if the sampler ever ran.
    pub(crate) fn latest(&self) -> Option<DiskSample> {
        self.latest_guard().clone()
    }

    /// Sample now, export gauges, emit alert transitions, and cache the
    /// result. Blocking filesystem work; call off the async hot path.
    pub(crate) fn refresh(&self) -> DiskSample {
        let sample = self.sample();
        if let Some(free) = sample.free_bytes {
            crate::metrics::set_gauge("state_disk_free_bytes", None, free as f64);
        } else {
            // Unknown free space is intentionally not exported: a missing
            // series is honest, `0` reads as a full disk, and keeping the
            // last known number reads as a live measurement.
            crate::metrics::clear_gauge("state_disk_free_bytes", None);
        }
        for (category, value) in [
            ("journal", sample.breakdown.journal_bytes),
            ("uploads", sample.breakdown.uploads_bytes),
            ("sqlite", sample.breakdown.sqlite_bytes),
            ("logs", sample.breakdown.logs_bytes),
        ] {
            crate::metrics::set_gauge("state_disk_bytes", Some(category), value as f64);
        }
        self.emit_alerts(&sample);
        *self.latest_guard() = Some(sample.clone());
        sample
    }

    /// Admission decision for a new session, from the cached sample.
    pub(crate) fn guard_decision(&self) -> DiskGuardDecision {
        let config = self.config.get();
        if !config.limits.disk_guard_enabled {
            return DiskGuardDecision::Disabled;
        }
        let max_age = Duration::from_secs(config.limits.disk_sample_max_age_seconds);
        let min_bytes = config.limits.disk_min_free_bytes;
        let sample = self.latest_guard().clone();
        let Some(sample) = sample else {
            return DiskGuardDecision::Unknown {
                reason: "no disk sample has been taken yet".to_string(),
            };
        };
        let age = sample.age(SystemTime::now());
        if age > max_age {
            return DiskGuardDecision::Stale { age, max_age };
        }
        match sample.free_bytes {
            None => DiskGuardDecision::Unknown {
                reason: sample
                    .free_unknown_reason
                    .unwrap_or_else(|| "free space probe failed".to_string()),
            },
            Some(free_bytes) if free_bytes < min_bytes => DiskGuardDecision::BelowMinimum {
                free_bytes,
                min_bytes,
            },
            Some(free_bytes) => DiskGuardDecision::Allowed { free_bytes },
        }
    }

    /// Free bytes currently below the warning threshold, if the latest
    /// sample says so. Warn-level, guard-independent (always on).
    pub(crate) fn warning(&self) -> Option<u64> {
        let warn_below = self.config.get().limits.disk_warn_free_bytes;
        let sample = self.latest_guard().clone()?;
        sample.free_bytes.filter(|free| *free < warn_below)
    }

    /// Whether the admission guard is enabled in the live config.
    pub(crate) fn guard_enabled(&self) -> bool {
        self.config.get().limits.disk_guard_enabled
    }

    /// Current warning threshold from the live config.
    pub(crate) fn warn_threshold(&self) -> u64 {
        self.config.get().limits.disk_warn_free_bytes
    }

    #[cfg(test)]
    pub(crate) fn store_sample_for_test(&self, sample: DiskSample) {
        *self.latest_guard() = Some(sample);
    }

    fn sample(&self) -> DiskSample {
        let config = self.config.get();
        let (free_bytes, free_unknown_reason) = match self.probe.free_bytes(&config.paths.state_dir)
        {
            Ok(bytes) => (Some(bytes), None),
            Err(reason) => (None, Some(reason)),
        };
        let (breakdown, breakdown_partial_reason) = scan_breakdown(&config.paths);
        DiskSample {
            sampled_at: SystemTime::now(),
            free_bytes,
            free_unknown_reason,
            breakdown,
            breakdown_partial_reason,
        }
    }

    fn emit_alerts(&self, sample: &DiskSample) {
        let warn_below = self.config.get().limits.disk_warn_free_bytes;
        let low = sample.free_bytes.is_some_and(|free| free < warn_below);
        let mut warned = self
            .warned_low
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if low && !*warned {
            warn!(
                free_bytes = sample.free_bytes,
                warn_below_bytes = warn_below,
                "state volume free space is below the warning threshold; \
                 enable disk_guard_enabled to refuse new sessions at the floor"
            );
        } else if !low && *warned {
            info!("state volume free space recovered above the warning threshold");
        }
        *warned = low;
        if let Some(reason) = &sample.free_unknown_reason {
            debug!(reason = %reason, "state volume free space is unknown");
        }
        if let Some(reason) = &sample.breakdown_partial_reason {
            debug!(reason = %reason, "state volume size breakdown is partial");
        }
    }
}

/// Sampler loop: one supervised daemon component. Cadence is read from the
/// live config every tick, so `disk_sample_interval_seconds` hot-reloads.
/// Sampling itself runs on the blocking pool: directory walks must never
/// stall the async runtime.
pub(crate) async fn run_disk_sampler(
    monitor: std::sync::Arc<DiskMonitor>,
) -> std::result::Result<(), String> {
    loop {
        let monitor_for_scan = std::sync::Arc::clone(&monitor);
        if let Err(err) = tokio::task::spawn_blocking(move || monitor_for_scan.refresh()).await {
            return Err(format!("disk sample scan failed: {err}"));
        }
        let interval = monitor
            .config
            .get()
            .limits
            .disk_sample_interval_seconds
            .max(MIN_SAMPLE_INTERVAL_SECONDS);
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

// ---------------------------------------------------------------------------
// Bounded directory scans
// ---------------------------------------------------------------------------

fn scan_breakdown(paths: &crate::config::PathsConfig) -> (DiskBreakdown, Option<String>) {
    let mut breakdown = DiskBreakdown::default();
    let mut budget = MAX_BREAKDOWN_ENTRIES;
    let mut partial: Option<String> = None;

    // Journal + uploads live under one directory per session.
    match std::fs::read_dir(&paths.sessions_dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                if budget == 0 {
                    partial.get_or_insert_with(|| {
                        format!("scan capped at {MAX_BREAKDOWN_ENTRIES} entries")
                    });
                    break;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                let session_dir = entry.path();
                breakdown.journal_bytes = breakdown.journal_bytes.saturating_add(dir_size(
                    &session_dir.join(crate::session::journal::JOURNAL_DIR_NAME),
                    &mut budget,
                    &mut partial,
                ));
                breakdown.uploads_bytes = breakdown.uploads_bytes.saturating_add(dir_size(
                    &session_dir.join("files"),
                    &mut budget,
                    &mut partial,
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            partial.get_or_insert_with(|| format!("failed to read sessions dir: {err}"));
        }
    }

    // SQLite database plus WAL companions (missing ones are normal).
    breakdown.sqlite_bytes = file_size(&paths.db_file)
        .saturating_add(file_size(&sqlite_companion(&paths.db_file, "-wal")))
        .saturating_add(file_size(&sqlite_companion(&paths.db_file, "-shm")));

    breakdown.logs_bytes = dir_size(&paths.state_dir.join("logs"), &mut budget, &mut partial);

    (breakdown, partial)
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|meta| if meta.is_file() { meta.len() } else { 0 })
        .unwrap_or(0)
}

/// SQLite companion file path (`<db>-wal` / `<db>-shm`), which is a filename
/// suffix, not an extension replacement.
fn sqlite_companion(db_file: &Path, suffix: &str) -> PathBuf {
    let mut path = db_file.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn dir_size(path: &Path, budget: &mut usize, partial: &mut Option<String>) -> u64 {
    if *budget == 0 {
        partial.get_or_insert_with(|| format!("scan capped at {MAX_BREAKDOWN_ENTRIES} entries"));
        return 0;
    }
    let mut total: u64 = 0;
    let mut queue: Vec<PathBuf> = vec![path.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                partial.get_or_insert_with(|| {
                    format!("scan capped at {MAX_BREAKDOWN_ENTRIES} entries")
                });
                return total;
            }
            *budget -= 1;
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                queue.push(entry.path());
            } else if file_type.is_file()
                && let Ok(meta) = entry.metadata()
            {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DEFAULT_SCREEN_SCROLLBACK_ROWS};
    use std::sync::Arc;

    struct FixedProbe {
        result: std::result::Result<u64, String>,
    }

    impl SpaceProbe for FixedProbe {
        fn free_bytes(&self, _path: &Path) -> std::result::Result<u64, String> {
            self.result.clone()
        }
    }

    fn test_config(state_dir: &Path, limits: crate::config::LimitsConfig) -> AppConfig {
        AppConfig {
            paths: crate::config::PathsConfig {
                state_dir: state_dir.to_path_buf(),
                sessions_dir: state_dir.join("sessions"),
                db_file: state_dir.join("db"),
                lock_file: state_dir.join("daemon.lock"),
                info_file: state_dir.join("daemon.info"),
                socket_name: "test".into(),
                socket_file: PathBuf::from("."),
            },
            http: crate::config::HttpConfig {
                bind: "127.0.0.1".to_string(),
                port: 15443,
            },
            notify: crate::config::NotifyConfig {
                min_interval_seconds: 10,
                prompt_patterns: Vec::new(),
                hook: None,
            },
            limits,
            web_push: crate::config::WebPushConfig {
                subject: None,
                vapid_public_key: None,
                vapid_private_key: None,
                proxy: None,
            },
            resume: crate::config::ResumeConfig::default(),
            log_level: "info".to_string(),
            trusted_apps_enabled: false,
            runtime_overrides: Default::default(),
        }
    }

    fn limits(
        guard: bool,
        min: u64,
        warn: u64,
        interval: u64,
        max_age: u64,
    ) -> crate::config::LimitsConfig {
        crate::config::LimitsConfig {
            max_running_sessions: 50,
            session_eviction_seconds: 15,
            screen_scrollback_rows: DEFAULT_SCREEN_SCROLLBACK_ROWS,
            silence_seconds: 10,
            stop_grace_seconds: 5,
            journal_retention_days: 0,
            disk_guard_enabled: guard,
            disk_min_free_bytes: min,
            disk_warn_free_bytes: warn,
            disk_sample_interval_seconds: interval,
            disk_sample_max_age_seconds: max_age,
        }
    }

    fn build_monitor(
        state_dir: &Path,
        limits: crate::config::LimitsConfig,
        probe: std::result::Result<u64, String>,
    ) -> DiskMonitor {
        let live = LiveConfig::from_arc(Arc::new(test_config(state_dir, limits)));
        DiskMonitor::new(live, Box::new(FixedProbe { result: probe }))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oly_disk_{name}_{}_{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn breakdown_sums_known_categories() {
        let state = temp_dir("breakdown");
        let sessions = state.join("sessions").join("s1");
        std::fs::create_dir_all(sessions.join("journal")).unwrap();
        std::fs::create_dir_all(sessions.join("files")).unwrap();
        std::fs::create_dir_all(state.join("logs")).unwrap();
        std::fs::write(sessions.join("journal").join("manifest.log"), b"0123456789").unwrap();
        std::fs::write(sessions.join("journal").join("seg-00000001.ojrn"), b"01234").unwrap();
        std::fs::write(
            sessions.join("files").join("upload.bin"),
            b"0123456789abcdef",
        )
        .unwrap();
        std::fs::write(state.join("db"), b"0123456").unwrap();
        std::fs::write(state.join("logs").join("daemon.log"), b"0123456789").unwrap();

        let monitor = build_monitor(
            &state,
            limits(false, 1, u64::MAX - 1, 30, 120),
            Ok(500 * 1024 * 1024 * 1024),
        );
        let sample = monitor.refresh();
        assert_eq!(sample.free_bytes, Some(500 * 1024 * 1024 * 1024));
        assert_eq!(sample.breakdown.journal_bytes, 15);
        assert_eq!(sample.breakdown.uploads_bytes, 16);
        assert_eq!(sample.breakdown.sqlite_bytes, 7);
        assert_eq!(sample.breakdown.logs_bytes, 10);
        assert_eq!(sample.breakdown_partial_reason, None);
    }

    #[test]
    fn unknown_free_space_stays_unknown_with_reason() {
        let state = temp_dir("unknown");
        let monitor = build_monitor(
            &state,
            limits(false, 1, u64::MAX - 1, 30, 120),
            Err("probe exploded".to_string()),
        );
        let sample = monitor.refresh();
        assert_eq!(sample.free_bytes, None);
        assert_eq!(
            sample.free_unknown_reason.as_deref(),
            Some("probe exploded")
        );
        // With the guard off the unknown sample is irrelevant.
        assert_eq!(monitor.guard_decision(), DiskGuardDecision::Disabled);
    }

    #[test]
    fn guard_refuses_below_floor_and_when_sample_is_unknown() {
        let state = temp_dir("guard");
        // Below the floor with the guard on: refuse, and name the numbers.
        let monitor = build_monitor(&state, limits(true, 1024, 4096, 30, 120), Ok(512));
        monitor.refresh();
        assert_eq!(
            monitor.guard_decision(),
            DiskGuardDecision::BelowMinimum {
                free_bytes: 512,
                min_bytes: 1024,
            }
        );

        // Above the floor: allowed.
        let monitor = build_monitor(&state, limits(true, 1024, 4096, 30, 120), Ok(2048));
        monitor.refresh();
        assert_eq!(
            monitor.guard_decision(),
            DiskGuardDecision::Allowed { free_bytes: 2048 }
        );

        // Unknown sample with the guard on: refuse with the probe reason.
        let monitor = build_monitor(
            &state,
            limits(true, 1024, 4096, 30, 120),
            Err("no such mount".to_string()),
        );
        monitor.refresh();
        match monitor.guard_decision() {
            DiskGuardDecision::Unknown { reason } => assert_eq!(reason, "no such mount"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn guard_refuses_stale_and_missing_samples() {
        let state = temp_dir("stale");
        let monitor = build_monitor(&state, limits(true, 1024, 4096, 30, 120), Ok(8 * 1024));
        // No sample yet: unknown, not assumed-fine.
        match monitor.guard_decision() {
            DiskGuardDecision::Unknown { reason } => {
                assert!(reason.contains("no disk sample"))
            }
            other => panic!("expected Unknown, got {other:?}"),
        }

        monitor.store_sample_for_test(DiskSample {
            sampled_at: SystemTime::now() - Duration::from_secs(600),
            free_bytes: Some(8 * 1024),
            free_unknown_reason: None,
            breakdown: DiskBreakdown::default(),
            breakdown_partial_reason: None,
        });
        match monitor.guard_decision() {
            DiskGuardDecision::Stale { age, max_age } => {
                assert!(age >= Duration::from_secs(599));
                assert_eq!(max_age, Duration::from_secs(120));
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn guard_hot_reloads_thresholds_and_expiry() {
        let state = temp_dir("hot_reload");
        let live = LiveConfig::from_arc(Arc::new(test_config(
            &state,
            limits(true, 1024, 4096, 30, 120),
        )));
        let monitor = DiskMonitor::new(live.clone(), Box::new(FixedProbe { result: Ok(2048) }));
        monitor.refresh();
        assert_eq!(
            monitor.guard_decision(),
            DiskGuardDecision::Allowed { free_bytes: 2048 }
        );

        // Raise the floor above the observed free space and shrink the
        // freshness window: both apply without a restart.
        live.replace(test_config(&state, limits(true, 4096, 8192, 30, 5)));
        assert!(matches!(
            monitor.guard_decision(),
            DiskGuardDecision::BelowMinimum {
                free_bytes: 2048,
                min_bytes: 4096
            }
        ));

        monitor.store_sample_for_test(DiskSample {
            sampled_at: SystemTime::now() - Duration::from_secs(30),
            free_bytes: Some(8192),
            free_unknown_reason: None,
            breakdown: DiskBreakdown::default(),
            breakdown_partial_reason: None,
        });
        assert!(matches!(
            monitor.guard_decision(),
            DiskGuardDecision::Stale { .. }
        ));
    }

    #[test]
    fn warning_tracks_the_warn_threshold_only() {
        let state = temp_dir("warn");
        let monitor = build_monitor(&state, limits(false, 1024, 4096, 30, 120), Ok(2048));
        monitor.refresh();
        assert_eq!(monitor.warning(), Some(2048));
        // Warning stays advisory: the guard is still disabled.
        assert_eq!(monitor.guard_decision(), DiskGuardDecision::Disabled);
    }
}
