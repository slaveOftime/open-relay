//! A minimal always-on metrics registry for the critical data paths.
//!
//! Design constraints (PLAN "bounded everything" applies to observability
//! too): no new dependencies, no dynamic label cardinality (names and
//! labels are `&'static str` or small closed sets from code), lock held
//! only for the map update after the measured work — never around it.
//! Metric names are lowercase with underscores and get an `oly_` prefix
//! plus a Prometheus suffix (`_seconds`, `_total`) at render time.
//!
//! Everything is exported as Prometheus text format at `GET /api/metrics`
//! (behind the normal auth layer). Numbers are recorded from the first
//! daemon start; there is no on/off switch because the overhead is a
//! mutex per instrumented event, which is noise next to the work measured.
//!
//! The tracked metric families mirror the data flows in ARCHITECTURE.md:
//!
//! - `oly_ipc_request_seconds{method}` — daemon-side handling time of
//!   every one-shot IPC RPC (what `oly ls` and friends wait on).
//! - `oly_http_request_seconds{route}` — HTTP request duration per
//!   matched route (long-lived SSE responses are excluded; they would
//!   measure connection lifetime, not work).
//! - `oly_attach_init_seconds{path}` — time from attach subscribe to the
//!   init frame being ready (`local` = journal snapshot + engine state +
//!   scrollback seed; `proxied` = same on the owning node plus relay).
//! - `oly_attach_resyncs_total`, `oly_attach_clients_total` — stream
//!   health counters (a healthy system resyncs ~never).
//! - `oly_journal_append_seconds`, `oly_journal_sync_seconds` — the
//!   appender thread's write() and fsync() latencies. These are the
//!   usual suspects when *everything* gets slow at once: a stalling
//!   disk shows up here first.
//! - `oly_journal_bytes_total`, `oly_journal_failures_total` — volume
//!   and durability degradation counters.

use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Histogram bucket upper bounds in seconds. Covers 100 µs … 30 s, which
/// spans "socket echo" to "the daemon mysteriously stalls for seconds".
/// A stall that a user notices always lands well inside this range; the
/// +Inf bucket catches the rest.
const BUCKETS_SECS: [f64; 16] = [
    0.000_1, 0.000_5, 0.001, 0.002_5, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
    10.0, 30.0,
];

#[derive(Default)]
struct Histogram {
    count: u64,
    sum_secs: f64,
    max_secs: f64,
    /// One entry per BUCKETS_SECS bound: observations <= bound.
    /// Cumulative by construction (`observe` bumps every passing bound),
    /// which is exactly what Prometheus `_bucket` series expect.
    counts: [u64; BUCKETS_SECS.len()],
}

impl Histogram {
    fn observe(&mut self, secs: f64) {
        self.count += 1;
        self.sum_secs += secs;
        if secs > self.max_secs {
            self.max_secs = secs;
        }
        for (i, bound) in BUCKETS_SECS.iter().enumerate() {
            if secs <= *bound {
                self.counts[i] += 1;
            }
        }
    }
}

#[derive(Default)]
struct Registry {
    histograms: BTreeMap<String, Histogram>,
    counters: BTreeMap<String, u64>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(Mutex::default)
}

fn key(name: &str, label: Option<&str>) -> String {
    match label {
        Some(l) if !l.is_empty() => format!("{name}|{l}"),
        _ => name.to_string(),
    }
}

/// Record one duration observation for `name` (e.g. `attach_init`).
/// The Prometheus unit suffix (`_seconds`) is added at render time; keep
/// names unit-free so counters and histograms of the same subject can
/// share the base name.
pub fn observe(name: &'static str, label: Option<&str>, elapsed: Duration) {
    let secs = elapsed.as_secs_f64();
    let mut reg = registry().lock();
    reg.histograms
        .entry(key(name, label))
        .or_default()
        .observe(secs);
}

/// Add `delta` to a counter (rendered with a `_total` suffix).
pub fn add(name: &'static str, label: Option<&str>, delta: u64) {
    let mut reg = registry().lock();
    *reg.counters.entry(key(name, label)).or_default() += delta;
}

/// Bump a counter by one.
pub fn count(name: &'static str, label: Option<&str>) {
    add(name, label, 1);
}

/// A start/stop guard that observes its lifetime when dropped.
pub struct Timer {
    name: &'static str,
    label: Option<&'static str>,
    start: Instant,
}

impl Timer {
    pub fn start(name: &'static str, label: Option<&'static str>) -> Self {
        Timer {
            name,
            label,
            start: Instant::now(),
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        observe(self.name, self.label, self.start.elapsed());
    }
}

/// Split a stored `name|label` key.
fn split_key(key: &str) -> (&str, Option<&str>) {
    match key.split_once('|') {
        Some((n, l)) => (n, Some(l)),
        None => (key, None),
    }
}

fn labels_suffix(label: Option<&str>) -> String {
    match label {
        Some(l) => format!(",label=\"{l}\""),
        None => String::new(),
    }
}

/// Render the whole registry as Prometheus text format (0.0.4).
pub fn render_prometheus() -> String {
    let reg = registry().lock();
    let mut out = String::with_capacity(8 * 1024);
    let mut names: Vec<&str> = reg.histograms.keys().map(|k| split_key(k).0).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        out.push_str(&format!("# TYPE oly_{name}_seconds histogram\n"));
        for (k, h) in &reg.histograms {
            let (base, label) = split_key(k);
            if base != name {
                continue;
            }
            let suffix = labels_suffix(label);
            for (i, bound) in BUCKETS_SECS.iter().enumerate() {
                out.push_str(&format!(
                    "oly_{name}_seconds_bucket{suffix} le=\"{bound}\" {}\n",
                    h.counts[i]
                ));
            }
            out.push_str(&format!(
                "oly_{name}_seconds_bucket{suffix} le=\"+Inf\" {}\n",
                h.count
            ));
            out.push_str(&format!(
                "oly_{name}_seconds_sum{suffix} {}\noly_{name}_seconds_count{suffix} {}\noly_{name}_seconds_max{suffix} {}\n",
                h.sum_secs, h.count, h.max_secs
            ));
        }
    }
    let mut counters: Vec<(&String, &u64)> = reg.counters.iter().collect();
    counters.sort_unstable_by_key(|(k, _)| k.as_str());
    let mut emitted_type = std::collections::HashSet::new();
    for (k, v) in counters {
        let (base, label) = split_key(k);
        if emitted_type.insert(base.to_string()) {
            out.push_str(&format!("# TYPE oly_{base}_total counter\n"));
        }
        let suffix = labels_suffix(label);
        out.push_str(&format!("oly_{base}_total{suffix} {v}\n"));
    }
    out
}

/// Whether the process should print client-side timing lines to stderr
/// (`OLY_TIMING=1`). Cached; read once.
static TIMING_ENABLED: OnceLock<bool> = OnceLock::new();

pub fn timing_enabled() -> bool {
    *TIMING_ENABLED.get_or_init(|| {
        std::env::var("OLY_TIMING")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Marks are only printed when `OLY_TIMING=1`; each call prints the
/// elapsed time since process start, so the sequence of marks
/// reconstructs where a CLI invocation spent its wall time (startup →
/// connect → RPC → render).
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

pub fn process_start() -> Instant {
    *PROCESS_START.get_or_init(Instant::now)
}

/// Print `oly-timing: <label> <ms since process start>` when enabled.
pub fn mark(label: &str) {
    if timing_enabled() {
        eprintln!(
            "oly-timing: {label} {:.1}ms",
            process_start().elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// One-shot mark helper: prints only the first time it is used, for
/// per-RPC code paths that should annotate the process just once
/// (e.g. "ipc connected" before the first request completed).
pub struct FirstTime(AtomicBool);

pub static FIRST_MARK: FirstTime = FirstTime(AtomicBool::new(true));

impl FirstTime {
    pub fn mark_first(&self, label: &str) {
        if self.0.swap(false, Ordering::Relaxed) {
            mark(label);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative_at_export() {
        let reg = registry();
        {
            let mut r = reg.lock();
            r.histograms
                .entry("test_metric|case".to_string())
                .or_default()
                .observe(0.000_2);
            r.counters.insert("test_counter|case".to_string(), 3);
        }
        let text = render_prometheus();
        assert!(
            text.contains("oly_test_metric_seconds_bucket,label=\"case\" le=\"0.0005\" 1"),
            "{text}"
        );
        assert!(
            text.contains("oly_test_metric_seconds_bucket,label=\"case\" le=\"0.0001\" 0"),
            "{text}"
        );
        assert!(text.contains("oly_test_counter_total,label=\"case\" 3"));
        assert!(text.contains("# TYPE oly_test_metric_seconds histogram"));
    }

    #[test]
    fn timer_observes_on_drop() {
        let t = Timer::start("timer_test", None);
        std::thread::sleep(Duration::from_millis(2));
        drop(t);
        let text = render_prometheus();
        assert!(text.contains("oly_timer_test_seconds_count 1"), "{text}");
    }

    #[test]
    fn counters_accumulate() {
        add("counter_test", Some("x"), 2);
        count("counter_test", Some("x"));
        let reg = registry().lock();
        assert_eq!(reg.counters["counter_test|x"], 3);
    }
}
