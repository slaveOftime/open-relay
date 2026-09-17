//! M0 latency probes (PLAN.md §11.1): direct-PTY versus through-oly
//! key-to-echo round trips, measured in-process with a deterministic
//! immediate-echo fixture.
//!
//! These are ignored development probes, not CI gates. They establish the
//! M0 baseline for the §11.2 budgets ("backend input acceptance → writable
//! PTY write" p95 ≤ 2 ms; "PTY output read → local client application"
//! p95 ≤ 5 ms). Run with:
//!
//! ```text
//! cargo test --release -- --ignored --nocapture m0_probes
//! ```
//!
//! What each probe measures:
//!
//! - `probe_direct_pty_echo_roundtrip`: write one byte to a PTY running
//!   `cat` and read the line-discipline echo back — the floor any
//!   intermediary adds overhead onto.
//! - `probe_daemon_backend_echo_roundtrip`: the same byte through a real
//!   `spawn_session` runtime: writer queue → writer thread → PTY → echo →
//!   reader thread → scan → screen update → log append → broadcast →
//!   subscriber wakeup. This isolates the daemon backend; IPC framing and
//!   client rendering are measured separately (browser/CLI probes are M0
//!   follow-ups).

#![cfg(test)]
#![cfg(not(windows))] // ConPTY timing harness is a Windows-lane follow-up.

use std::{
    io::{Read, Write},
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};

use chrono::Utc;

use super::runtime::spawn_session;
use super::{SessionEvent, SessionMeta, SessionStatus};

const SAMPLES: usize = 300;
const ECHO_TIMEOUT: Duration = Duration::from_secs(5);

fn percentiles(label: &str, samples: &mut Vec<Duration>) {
    samples.sort();
    let pick = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize];
    eprintln!(
        "probe {label}: n={} p50={:?} p95={:?} p99={:?} max={:?}",
        samples.len(),
        pick(0.50),
        pick(0.95),
        pick(0.99),
        samples.last().unwrap(),
    );
}

fn session_meta(id: &str) -> SessionMeta {
    SessionMeta {
        id: id.to_string(),
        title: None,
        tags: vec![],
        command: "cat".to_string(),
        args: vec![],
        cwd: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        ended_at: None,
        status: SessionStatus::Created,
        pid: None,
        exit_code: None,
        notifications_enabled: false,
        foreground_color: None,
        background_color: None,
    }
}

#[test]
#[ignore = "M0 measurement probe (PLAN §11.1): baseline evidence, not a CI gate"]
fn probe_direct_pty_echo_roundtrip() {
    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let child = pty
        .slave
        .spawn_command(portable_pty::CommandBuilder::new("cat"))
        .expect("spawn cat");
    let mut child = child;
    let mut writer = pty.master.take_writer().expect("pty writer");
    let mut reader = pty.master.try_clone_reader().expect("pty reader");

    // Pump reads on a thread so each sample can wait with a timeout.
    let (echo_tx, echo_rx) = std_mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if echo_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        // Drain stale echo bytes, then time one keystroke round trip.
        while echo_rx.try_recv().is_ok() {}
        let started = Instant::now();
        writer.write_all(b"x").unwrap();
        writer.flush().unwrap();
        'wait: loop {
            let chunk = echo_rx
                .recv_timeout(ECHO_TIMEOUT)
                .expect("direct PTY echo timed out");
            if chunk.contains(&b'x') {
                break 'wait;
            }
        }
        samples.push(started.elapsed());
    }

    percentiles("direct-pty-echo", &mut samples);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
#[ignore = "M0 measurement probe (PLAN §11.1): baseline evidence, not a CI gate"]
fn probe_daemon_backend_echo_roundtrip() {
    let dir = std::env::temp_dir().join(format!("oly_probe_echo_{}", uuid::Uuid::new_v4()));
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel::<SessionEvent>(16);
    let mut meta = session_meta("probe01");
    let runtime = spawn_session(&mut meta, dir.clone(), 24, 80, false, 100, event_tx)
        .expect("spawn probe session");

    let mut broadcast_rx = runtime.read().broadcast_tx.subscribe();
    // Bridge the tokio broadcast receiver into a std channel so samples can
    // wait with a timeout from this sync test.
    let (echo_tx, echo_rx) = std_mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        loop {
            match broadcast_rx.blocking_recv() {
                Ok(bytes) => {
                    if echo_tx.send(bytes.to_vec()).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        while echo_rx.try_recv().is_ok() {}
        let started = Instant::now();
        runtime
            .read()
            .pty
            .try_write_input(b"x".to_vec())
            .expect("queue probe keystroke");
        'wait: loop {
            let chunk = echo_rx
                .recv_timeout(ECHO_TIMEOUT)
                .expect("daemon backend echo timed out");
            if chunk.contains(&b'x') {
                break 'wait;
            }
        }
        samples.push(started.elapsed());
    }

    percentiles("daemon-backend-echo", &mut samples);
    runtime.write().pty.kill().ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// M1 dev verification: with `OLY_JOURNAL=1` the reader thread journals
/// every output chunk it also appends to `output.log`, in order.
///
/// Ignored and run individually — it mutates the process-global
/// `OLY_JOURNAL` env var, which is only safe when this probe runs alone:
/// `cargo test --release -- --ignored --nocapture probe_shadow_journal`.
#[test]
#[ignore = "M1 dev verification: mutates OLY_JOURNAL; run individually"]
fn probe_shadow_journal_records_output_in_order() {
    // SAFETY: this probe is documented to run alone (see the ignore note),
    // so no other thread can observe the env mutation mid-test.
    unsafe { std::env::set_var("OLY_JOURNAL", "1") };

    let dir = std::env::temp_dir().join(format!("oly_probe_journal_{}", uuid::Uuid::new_v4()));
    let (event_tx, _event_rx) = tokio::sync::broadcast::channel::<SessionEvent>(16);
    let mut meta = session_meta("journal1");
    let runtime = spawn_session(&mut meta, dir.clone(), 24, 80, false, 100, event_tx)
        .expect("spawn probe session");

    let mut broadcast_rx = runtime.read().broadcast_tx.subscribe();
    let (echo_tx, echo_rx) = std_mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        loop {
            match broadcast_rx.blocking_recv() {
                Ok(bytes) => {
                    if echo_tx.send(bytes.to_vec()).is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Distinctive marker chunks, echoed back by `cat`.
    for marker in [b"JRN-AAAA\r", b"JRN-BBBB\r", b"JRN-CCCC\r"] {
        runtime
            .read()
            .pty
            .try_write_input(marker.to_vec())
            .expect("queue probe input");
        'wait: loop {
            let chunk = echo_rx.recv_timeout(ECHO_TIMEOUT).expect("echo timed out");
            if chunk
                .windows(marker.len() - 1)
                .any(|w| w == &marker[..marker.len() - 1])
            {
                break 'wait;
            }
        }
    }
    runtime.write().pty.kill().ok();

    let outcome = super::journal::scan_segment(
        &dir.join(super::journal::JOURNAL_DIR_NAME)
            .join("seg-00000001.ojrn"),
    )
    .expect("journal segment scans");
    assert!(outcome.is_clean(), "shadow journal segment must scan clean");
    assert!(
        outcome.records.len() >= 3,
        "expected at least the three marker echoes, got {}",
        outcome.records.len()
    );
    let seqs: Vec<u64> = outcome.records.iter().map(|r| r.seq).collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "seqs contiguous from 1"
    );

    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: same single-threaded-probe justification as the set above.
    unsafe { std::env::remove_var("OLY_JOURNAL") };
}
