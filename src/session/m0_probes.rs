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

/// M1 dev verification: with `OLY_JOURNAL=1` the runtime sequences every
/// canonical output chunk it also appends to `output.log`, plus the
/// initial geometry/start facts, mid-stream resizes and the end fact —
/// all in one contiguous per-incarnation order.
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

    // Distinctive marker chunks, echoed back by `cat`. Between the second
    // and third marker, resize the PTY: the geometry change must land in
    // the journal between the surrounding output records.
    for (index, marker) in [b"JRN-AAAA\r", b"JRN-BBBB\r", b"JRN-CCCC\r"]
        .iter()
        .enumerate()
    {
        if index == 2 {
            assert!(runtime.write().resize_pty(40, 120), "probe resize applies");
        }
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
    runtime.write().mark_completed(SessionStatus::Killed, None);

    use super::journal::{
        LifecycleCode, RecordKind, decode_lifecycle_payload, decode_resize_payload,
    };

    // I10: completion is only journaled after the output stream drains
    // (EOF processing lags the kill), and then the appender must journal
    // everything the core published. Poll until the segment ends with the
    // Killed record and the appender has caught up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let outcome = loop {
        let caught_up = {
            let rt = runtime.read();
            let journal = rt.journal.as_ref().expect("shadow journal enabled");
            let mut journal = journal.lock();
            journal.poll_acks();
            journal.core.journal_seq() >= journal.core.head_seq().unwrap_or(0)
        };
        if caught_up {
            let outcome = super::journal::scan_segment(
                &dir.join(super::journal::JOURNAL_DIR_NAME)
                    .join("seg-00000001-0001.ojrn"),
            )
            .expect("journal segment scans");
            let ends_killed = outcome.records.last().is_some_and(|r| {
                r.kind == RecordKind::Lifecycle
                    && decode_lifecycle_payload(&r.payload).map(|(code, ..)| code)
                        == Some(LifecycleCode::Killed)
            });
            if ends_killed {
                break outcome;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "journal drain timed out waiting for the ordered ending"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    assert!(outcome.is_clean(), "shadow journal segment must scan clean");
    let seqs: Vec<u64> = outcome.records.iter().map(|r| r.seq).collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "seqs contiguous from 1"
    );

    // The session opens with its initial geometry and the start fact.
    assert_eq!(outcome.records[0].kind, RecordKind::Resize);
    assert_eq!(
        decode_resize_payload(&outcome.records[0].payload),
        Some((24, 80))
    );
    assert_eq!(outcome.records[1].kind, RecordKind::Lifecycle);
    assert_eq!(
        decode_lifecycle_payload(&outcome.records[1].payload).map(|(code, ..)| code),
        Some(LifecycleCode::Started)
    );

    // Exactly one resize to 40x120, ordered between the marker echoes.
    let resize_seqs: Vec<u64> = outcome
        .records
        .iter()
        .filter(|r| {
            r.kind == RecordKind::Resize && decode_resize_payload(&r.payload) == Some((40, 120))
        })
        .map(|r| r.seq)
        .collect();
    assert_eq!(resize_seqs.len(), 1, "resize journaled exactly once");
    let marker_seq = |needle: &[u8]| {
        outcome
            .records
            .iter()
            .filter(|r| r.kind == RecordKind::Output)
            .find(|r| r.payload.windows(needle.len()).any(|w| w == needle))
            .map(|r| r.seq)
            .unwrap_or_else(|| panic!("marker {} not journaled", String::from_utf8_lossy(needle)))
    };
    let bbbb = marker_seq(b"JRN-BBBB");
    let cccc = marker_seq(b"JRN-CCCC");
    assert!(
        bbbb < resize_seqs[0] && resize_seqs[0] < cccc,
        "resize seq {} must sit between markers {} and {}",
        resize_seqs[0],
        bbbb,
        cccc
    );

    // I10 ordered ending: every output record precedes OutputClosed,
    // which immediately precedes the Killed completion record.
    let last = outcome.records.last().unwrap();
    assert_eq!(last.kind, RecordKind::Lifecycle);
    assert_eq!(
        decode_lifecycle_payload(&last.payload).map(|(code, ..)| code),
        Some(LifecycleCode::Killed)
    );
    let close = &outcome.records[outcome.records.len() - 2];
    assert_eq!(close.kind, RecordKind::Lifecycle);
    assert_eq!(
        decode_lifecycle_payload(&close.payload).map(|(code, ..)| code),
        Some(LifecycleCode::OutputClosed),
        "completion must be immediately preceded by OutputClosed"
    );
    let last_output_seq = outcome
        .records
        .iter()
        .filter(|r| r.kind == RecordKind::Output)
        .map(|r| r.seq)
        .max()
        .expect("probe journaled output");
    assert!(
        last_output_seq < close.seq,
        "all output must be sequenced before the stream close"
    );

    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: same single-threaded-probe justification as the set above.
    unsafe { std::env::remove_var("OLY_JOURNAL") };
}

/// M1 dev probe (ADR-0002's deferred 20/50/100 ms decision): measure
/// append→durable lag of the shadow journal at candidate group-sync
/// cadences. Each sample records one small event and waits for the
/// cadence-driven sync to cover it; expected lag is uniform in
/// roughly `[0, cadence]` plus sync cost.
///
/// No env mutation (the appender cadence is parameterized directly), but
/// still an ignored dev probe rather than a CI gate:
/// `cargo test --release -- --ignored --nocapture probe_journal_sync_cadence`.
#[test]
#[ignore = "M1 dev probe: journal sync-cadence measurement; run individually"]
fn probe_journal_sync_cadence_durable_lag() {
    use super::journal::ShadowJournal;

    const CADENCE_SAMPLES: usize = 60;
    for cadence_ms in [20u64, 50, 100] {
        let dir = std::env::temp_dir().join(format!("oly_probe_cadence_{}", uuid::Uuid::new_v4()));
        let (mut shadow, _, _) =
            ShadowJournal::open_with_sync_interval(&dir, Duration::from_millis(cadence_ms))
                .expect("open shadow journal");

        let mut lags_micros = Vec::with_capacity(CADENCE_SAMPLES);
        for _ in 0..CADENCE_SAMPLES {
            let start = Instant::now();
            let cursor = shadow
                .record_output(bytes::Bytes::from_static(b"cadence-sample"))
                .expect("record sample");
            let deadline = start + Duration::from_secs(5);
            while shadow.core.durable_seq() < cursor.seq {
                assert!(Instant::now() < deadline, "durable ack timed out");
                std::thread::sleep(Duration::from_millis(1));
                shadow.poll_acks();
            }
            lags_micros.push(start.elapsed().as_micros() as u64);
        }
        lags_micros.sort_unstable();
        let p50 = lags_micros[CADENCE_SAMPLES / 2];
        let p95 = lags_micros[CADENCE_SAMPLES * 95 / 100];
        let max = lags_micros[CADENCE_SAMPLES - 1];
        println!(
            "journal sync cadence {cadence_ms:>3} ms: append→durable lag p50={p50}µs p95={p95}µs max={max}µs (n={CADENCE_SAMPLES})"
        );
        assert_eq!(shadow.core.head_seq(), Some(CADENCE_SAMPLES as u64));

        drop(shadow);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
