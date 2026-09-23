//! Segmented session journal: typed, checksummed records with stable
//! sequence numbers (PLAN.md §6.1, invariants I1/I3/I8/I10).
//!
//! Every raw PTY chunk, resize, mode revision (Policy) and lifecycle fact
//! is sequenced under the session write lock and appended by a dedicated
//! thread with a group-sync cadence. The journal is the canonical store;
//! everything else is derived at read time (ADR-0002).
//!
//! One incarnation spans bounded parts (`seg-NNNNNNNN-PPPP.ojrn`) with a
//! continuous sequence — cursors never name parts. Only the newest part
//! can be torn by a crash, so recovery scans just it (O(part) stats scan,
//! never O(history), never buffering the recording). Tail reads select
//! sealed parts newest-first and sparse-seek (1 MiB stride) into the
//! oldest selected part, bounding memory and work by the budget, not by
//! total history. Reads validate cross-part continuity, fail loudly on
//! cursors past the recovered tail (incomplete capture), and never see a
//! hole from concurrent retention (newest-part-first deletion).
//!
//! Format (all integers little-endian):
//!
//! ```text
//! record := magic(4) version(u16) kind(u16) flags(u32)
//!           seq(u64) elapsed_ms(u64) payload_len(u32) crc32(u32) payload
//! ```
//!
//! `crc32` (IEEE) covers the header bytes before the CRC field plus the
//! payload. `seq` is strictly monotonic and never reused **within an
//! incarnation**; the full cursor is `{session_id, incarnation, seq}` —
//! unlike the `output.log` offsets that 0.x size-cap truncation reused.
//! `elapsed_ms` is assigned by the sequencer (monotonic time since the
//! incarnation started); the session's wall-clock launch time lives in the
//! manifest, not per record.

#[cfg(test)]
use std::io::Write;
#[cfg(all(test, target_os = "linux"))]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)] // used inside `mod tests`
use std::{
    fs,
    io::{self, Read, Seek},
    path::Path,
};

// `record` holds the on-disk record format, primary types and CRC-32
// (PLAN2 S1.1). Re-exported here so existing callers
// (`crate::session::journal::*`) keep the same paths after the split.
pub(crate) mod record;
#[cfg(test)]
pub(crate) use record::crc32;
pub use record::{Crc32, Record, RecordKind};
pub(crate) use record::{
    HEADER_LEN, MAX_PAYLOAD_LEN, RECORD_MAGIC, RECORD_VERSION, encode_record_header,
};

// Typed payload codecs for non-output records (PLAN2 S1.5 step 1).
pub(crate) mod codec;
pub use codec::{
    LifecycleCode, decode_resize_payload, encode_lifecycle_payload, encode_resize_payload,
    policy_payload,
};
#[cfg(test)]
pub use codec::{decode_lifecycle_payload, parse_policy};

// Sealed-part manifest (M3-6) (PLAN2 S1.5 step 2).
pub(crate) mod manifest;
#[cfg(test)]
pub(crate) use manifest::{ManifestLine, read_manifest_lines, verify_manifest};
pub(crate) use manifest::{
    RetiredIncarnation, SegmentManifestEntry, read_manifest, retired_incarnations,
};

// Reader / torn-tail recovery (PLAN2 S1.5 step 3 + S1.6 step 1 internals).
pub(crate) mod scan;
#[allow(unused_imports)]
// re-exported for sibling modules; consumed via short name by `stream.rs`.
pub(crate) use scan::{
    ReadPiece, SPARSE_INDEX_STRIDE_BYTES, ScanMode, ScanStart, ScanStop, read_exact_or_partial,
    scan_impl, scan_segment_stats_from,
};
#[cfg(test)]
pub(crate) use scan::{scan_segment, scan_segment_stats};

// Stream / range / history / tail read APIs (PLAN2 S1.5 step 4).
pub(crate) mod stream;
pub(crate) use stream::{CollectWindow, IndexEntry, SegmentStream, incarnation_parts};
#[cfg(test)]
pub(crate) use stream::{read_history, read_range, read_tail};

// Checkpoints (RecordKind::CheckpointRef) + retention (PLAN2 S1.6 step 2).
pub(crate) mod checkpoint;
pub(crate) use checkpoint::{
    Checkpoint, CheckpointAnchor, checkpoint_anchors, encode_checkpoint,
    min_incarnation_for_byte_budget, retain_before,
};
// `CHECKPOINT_MAGIC` is reached via `use super::*` inside `mod tests` only — gate the re-export.
#[allow(unused_imports)]
pub(crate) use checkpoint::CHECKPOINT_MAGIC;
#[cfg(test)]
pub(crate) use checkpoint::{
    decode_checkpoint, latest_checkpoint_incarnation, retain_before_unchecked,
};

// On-disk segment writer and segment-file helpers (PLAN2 S1.5 step 5).
pub(crate) mod segment;
pub(crate) use segment::{
    DEFAULT_SEGMENT_MAX_BYTES, JOURNAL_DIR_NAME, MANIFEST_FILE_NAME, SegmentWriter,
    list_incarnations, list_segments, part_first_seq, segment_path,
};

// Open-with-recovery: validates the journal dir, rewinds torn tails,
// returns `OpenedJournal` plus the recovery report (PLAN2 S1.5 step 6).
pub(crate) mod open;
pub(crate) use open::{JournalCursor, OrderedEvent, RecoveryReport, open, sync_dir};

// Daemon-side record pipeline: sequencer core, appender loop and
// rolling-segment writer (PLAN2 S1.5 step 7).
pub(crate) mod appender;
#[cfg(test)]
pub(crate) use appender::{
    AppenderMsg, DEFAULT_QUEUE_BUDGET_BYTES, RollingSegmentWriter, appender_loop,
};
pub(crate) use appender::{
    DEFAULT_SYNC_INTERVAL, JournalAck, JournalAppender, JournalSubmitError, SequencerCore,
};

// Shadow journal: bundles the sequencing core with the appender for
// the M1 shadow wiring (PLAN2 S1.5 step 8).
pub(crate) mod shadow;
pub(crate) use shadow::ShadowJournal;

// ---------------------------------------------------------------------------
// Reader / torn-tail recovery
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oly_journal_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn test_session_dir(name: &str) -> std::path::PathBuf {
        let dir = test_path(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn crc32_matches_the_standard_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn written_records_scan_back_identically() {
        let path = test_path("roundtrip.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        writer
            .append_record(RecordKind::Output, 7, 0, b"hello")
            .unwrap();
        writer
            .append_record(RecordKind::Resize, 8, 1, &24u16.to_le_bytes())
            .unwrap();
        writer
            .append_record(RecordKind::Lifecycle, 9, 1, b"")
            .unwrap();
        writer.sync().unwrap();
        drop(writer);

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::CleanEof);
        assert_eq!(scanned.valid_len, fs::metadata(&path).unwrap().len());
        assert_eq!(scanned.records.len(), 3);
        assert_eq!(scanned.records[0].seq, 7);
        assert_eq!(scanned.records[0].kind, RecordKind::Output);
        assert_eq!(scanned.records[0].payload, b"hello");
        assert_eq!(scanned.records[1].kind, RecordKind::Resize);
        assert_eq!(scanned.records[2].seq, 9);
        assert!(scanned.records[2].payload.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn torn_tail_recovers_to_the_last_valid_record() {
        let path = test_path("torn.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        writer
            .append_record(RecordKind::Output, 1, 0, b"first")
            .unwrap();
        writer
            .append_record(RecordKind::Output, 2, 0, b"second")
            .unwrap();
        let valid_len = writer.len();
        writer
            .append_record(RecordKind::Output, 3, 0, b"third-torn")
            .unwrap();
        drop(writer);

        // Simulate a crash mid-write: keep only half of the third record.
        let file_len = fs::metadata(&path).unwrap().len();
        let torn_len = valid_len + (file_len - valid_len) / 2;
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(torn_len).unwrap();
        drop(file);

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::PartialTail);
        assert!(scanned.stop.may_rewind(), "an active torn tail may rewind");
        assert_eq!(scanned.valid_len, valid_len);
        assert_eq!(scanned.records.len(), 2);
        assert_eq!(scanned.records[1].payload, b"second");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupted_payload_stops_the_scan() {
        let path = test_path("corrupt.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        writer
            .append_record(RecordKind::Output, 1, 0, b"good")
            .unwrap();
        let first_len = writer.len();
        writer
            .append_record(RecordKind::Output, 2, 0, b"bad")
            .unwrap();
        writer
            .append_record(RecordKind::Output, 3, 0, b"after")
            .unwrap();
        drop(writer);

        // Flip one payload byte of the middle record.
        let mut bytes = fs::read(&path).unwrap();
        bytes[first_len as usize + HEADER_LEN] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::CrcMismatch);
        assert!(
            !scanned.stop.may_rewind(),
            "CRC corruption must be quarantined, not silently truncated"
        );
        assert_eq!(scanned.valid_len, first_len);
        assert_eq!(scanned.records.len(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sequence_gap_stops_the_scan_without_a_hole() {
        let path = test_path("seqgap.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        writer
            .append_record(RecordKind::Output, 1, 0, b"a")
            .unwrap();
        let first_len = writer.len();
        writer
            .append_record(RecordKind::Output, 2, 0, b"b")
            .unwrap();
        drop(writer);

        // Rewrite the second record's seq as if a later segment tail had
        // been aliased into place: the scan must stop, not skip ahead.
        let mut bytes = fs::read(&path).unwrap();
        let seq_at = first_len as usize + 12;
        bytes[seq_at..seq_at + 8].copy_from_slice(&42u64.to_le_bytes());
        // Re-seal the CRC so only the continuity check can catch this.
        let header_end = first_len as usize + 32;
        let payload = bytes[first_len as usize + HEADER_LEN..].to_vec();
        let crc = super::record::crc32_two(&bytes[first_len as usize..header_end], &payload);
        bytes[first_len as usize + 32..first_len as usize + 36].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::SequenceDiscontinuity);
        assert!(!scanned.stop.may_rewind());
        assert_eq!(scanned.records.len(), 1);
        assert_eq!(scanned.valid_len, first_len);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_fails_loudly_when_the_journal_dir_is_not_a_directory() {
        // M3-1 (ADR-0006): sessions fail to start when their journal cannot
        // be opened. Pin the open-level error the runtime propagates.
        let dir = std::env::temp_dir().join(format!("oly-jopen-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(JOURNAL_DIR_NAME), b"not a directory").unwrap();

        assert!(ShadowJournal::open(&dir).is_err());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn oversized_payload_is_rejected_before_writing() {
        let path = test_path("oversized.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        let huge = vec![0u8; MAX_PAYLOAD_LEN as usize + 1];
        assert!(
            writer
                .append_record(RecordKind::Output, 1, 0, &huge)
                .is_err()
        );
        assert_eq!(writer.len(), 0, "rejected record must not write bytes");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn garbage_length_field_does_not_allocate() {
        // A torn/aliased tail can present an enormous payload_len; the scan
        // must reject it against the limit before sizing a buffer.
        let path = test_path("garbage_len.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        writer
            .append_record(RecordKind::Output, 1, 0, b"only")
            .unwrap();
        let valid_len = writer.len();
        drop(writer);

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(RECORD_MAGIC);
        header[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&(RecordKind::Output as u16).to_le_bytes());
        header[12..20].copy_from_slice(&1u64.to_le_bytes());
        header[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&header).unwrap();
        drop(file);

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::OversizeLength);
        assert!(!scanned.stop.may_rewind());
        assert_eq!(scanned.valid_len, valid_len);
        assert_eq!(scanned.records.len(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn writer_records_the_sequencer_assigned_elapsed_ms() {
        // The writer never invents event timing (reopening a segment must
        // not move event time backwards); the sequencer owns the clock.
        let path = test_path("elapsed.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path).unwrap();
        for elapsed in 0..16u64 {
            writer
                .append_record(RecordKind::Output, elapsed + 1, elapsed, b"x")
                .unwrap();
        }
        drop(writer);

        let scanned = scan_segment(&path).unwrap();
        for (index, record) in scanned.records.iter().enumerate() {
            assert_eq!(record.elapsed_ms, index as u64);
        }

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn core_allocates_monotonic_seq_and_elapsed_before_publication() {
        let mut core = SequencerCore::new(1);
        assert_eq!(core.head_seq(), None);

        let first = core.publish(RecordKind::Output, bytes::Bytes::from_static(b"one"));
        let second = core.publish(RecordKind::Output, bytes::Bytes::from_static(b"two"));
        assert_eq!(
            first.cursor,
            JournalCursor {
                incarnation: 1,
                seq: 1
            }
        );
        assert_eq!(
            second.cursor,
            JournalCursor {
                incarnation: 1,
                seq: 2
            }
        );
        assert!(second.elapsed_ms >= first.elapsed_ms);
        assert_eq!(core.head_seq(), Some(2));
        assert_eq!(core.journal_seq(), 0);
        assert_eq!(core.durable_seq(), 0);
        assert_eq!(core.cached_events(), 2);
        assert!(!core.is_degraded());
    }

    #[test]
    fn cache_eviction_respects_journal_availability() {
        // Tiny budget forces eviction attempts on every publish.
        let mut core = SequencerCore::with_budget(1, 10);
        for seq in 1..=4u64 {
            core.publish(RecordKind::Output, bytes::Bytes::from_static(b"12345"));
            assert_eq!(core.head_seq(), Some(seq));
        }
        // Nothing is journaled yet: eviction must not drop unjournaled
        // events even over budget — it signals backpressure instead.
        assert!(core.over_budget());
        assert_eq!(core.cached_events(), 4);

        core.note_journaled(3);
        core.publish(RecordKind::Output, bytes::Bytes::from_static(b"12345"));
        // The publish re-evicted records 1..=3; 4 and 5 remain.
        assert_eq!(core.cached_events(), 2);
        assert!(!core.over_budget());
        assert_eq!(core.journal_seq(), 3);
        // Cursors never move backwards.
        core.note_journaled(1);
        assert_eq!(core.journal_seq(), 3);
    }

    #[test]
    fn degradation_is_explicit_and_sticky() {
        let mut core = SequencerCore::new(1);
        core.degrade("journal append failed: disk full");
        core.degrade("a second failure must not hide the first");
        assert!(core.is_degraded());
        assert_eq!(
            core.degraded_reason(),
            Some("journal append failed: disk full")
        );
        // Publication continues to sequence — never a silent gap.
        let event = core.publish(RecordKind::Output, bytes::Bytes::from_static(b"x"));
        assert_eq!(event.cursor.seq, 1);
    }

    #[test]
    fn open_recovers_previous_incarnation_and_starts_a_new_one() {
        let dir = test_session_dir("seq_reopen");
        let mut first = open(&dir).unwrap();
        assert_eq!(first.incarnation, 1);
        assert!(first.report.is_none());
        first
            .writer
            .append_record(RecordKind::Output, 1, 0, b"a")
            .unwrap();
        first
            .writer
            .append_record(RecordKind::Resize, 2, 0, b"80x24")
            .unwrap();
        first
            .writer
            .append_record(RecordKind::Output, 3, 0, b"b")
            .unwrap();
        drop(first);

        let mut second = open(&dir).unwrap();
        let report = second
            .report
            .take()
            .expect("previous incarnation must be recovered");
        assert_eq!(report.incarnation, 1);
        assert_eq!(report.records, 3);
        assert_eq!(report.last_seq, Some(3));
        assert_eq!(report.stop, ScanStop::CleanEof);
        assert!(!report.rewound);
        assert_eq!(second.incarnation, 2);

        second
            .writer
            .append_record(RecordKind::Output, 1, 0, b"c")
            .unwrap();
        drop(second);
        let outcome =
            scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000002-0001.ojrn")).unwrap();
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(
            outcome.records[0].seq, 1,
            "seq restarts within the new incarnation"
        );
        assert_eq!(outcome.records[0].payload, b"c".to_vec());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_rewinds_a_torn_tail_before_starting_the_new_incarnation() {
        let dir = test_session_dir("seq_torn");
        let mut first = open(&dir).unwrap();
        first
            .writer
            .append_record(RecordKind::Output, 1, 0, b"kept")
            .unwrap();
        let kept_len = first.writer.len();
        drop(first);

        // Simulate a crash mid-append: garbage bytes after the last record.
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn");
        fs::OpenOptions::new()
            .append(true)
            .open(&segment)
            .unwrap()
            .write_all(b"\xde\xad\xbe\xefpartial")
            .unwrap();

        let report = open(&dir).unwrap().report.unwrap();
        assert_eq!(report.stop, ScanStop::PartialTail);
        assert!(report.rewound);
        assert_eq!(report.records, 1);
        assert_eq!(report.last_seq, Some(1));
        assert_eq!(
            fs::metadata(&segment).unwrap().len(),
            kept_len,
            "torn tail must be truncated to the last valid record"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_reports_interior_corruption_without_rewinding() {
        let dir = test_session_dir("seq_corrupt");
        let mut first = open(&dir).unwrap();
        first
            .writer
            .append_record(RecordKind::Output, 1, 0, b"good")
            .unwrap();
        first
            .writer
            .append_record(RecordKind::Output, 2, 0, b"corrupted")
            .unwrap();
        let full_len = first.writer.len();
        drop(first);

        // Corrupt one payload byte of the second record (not the tail).
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn");
        let mut bytes = fs::read(&segment).unwrap();
        let second_payload = HEADER_LEN + 4 + HEADER_LEN;
        bytes[second_payload] ^= 0xFF;
        fs::write(&segment, &bytes).unwrap();

        let report = open(&dir).unwrap().report.unwrap();
        assert_eq!(report.stop, ScanStop::CrcMismatch);
        assert!(
            !report.rewound,
            "corruption is reported, never silently truncated"
        );
        assert_eq!(report.records, 1);
        assert_eq!(fs::metadata(&segment).unwrap().len(), full_len);

        let _ = fs::remove_dir_all(&dir);
    }

    // -- Appender boundary tests (PLAN.md §4.2) --

    fn event(seq: u64, payload: &'static [u8]) -> OrderedEvent {
        OrderedEvent {
            cursor: JournalCursor {
                incarnation: 1,
                seq,
            },
            elapsed_ms: 0,
            kind: RecordKind::Output,
            payload: bytes::Bytes::from_static(payload),
        }
    }

    fn recv_ack(acks: &std::sync::mpsc::Receiver<JournalAck>) -> JournalAck {
        acks.recv_timeout(std::time::Duration::from_secs(5))
            .expect("appender acknowledgement timed out")
    }

    #[test]
    fn appender_writes_in_order_and_acks_journal_and_durable_cursors() {
        let dir = test_session_dir("appender_ok");
        let (appender, incarnation, report, acks) = JournalAppender::spawn(&dir).unwrap();
        assert_eq!(incarnation, 1);
        assert!(report.is_none());

        for seq in 1..=3u64 {
            appender.try_submit(event(seq, b"chunk")).unwrap();
            assert_eq!(recv_ack(&acks), JournalAck::Journaled(seq));
        }
        appender.request_sync();
        assert_eq!(recv_ack(&acks), JournalAck::Durable(3));
        appender.shutdown();
        drop(appender);

        let outcome =
            scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn")).unwrap();
        assert!(outcome.is_clean());
        assert_eq!(outcome.records.len(), 3);
        assert_eq!(
            outcome.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn appender_rejects_out_of_order_records_without_a_silent_hole() {
        let dir = test_session_dir("appender_ooo");
        let (appender, _, _, acks) = JournalAppender::spawn(&dir).unwrap();

        appender.try_submit(event(1, b"one")).unwrap();
        assert_eq!(recv_ack(&acks), JournalAck::Journaled(1));
        appender.try_submit(event(3, b"three")).unwrap();
        match recv_ack(&acks) {
            JournalAck::Failed(reason) => {
                assert!(
                    reason.contains("contiguity"),
                    "unexpected failure: {reason}"
                );
            }
            other => panic!("expected contiguity failure, got {other:?}"),
        }
        // After a contiguity violation the appender is dead: further
        // records fail fast rather than writing past a hole.
        appender.try_submit(event(2, b"two")).unwrap();
        assert!(matches!(recv_ack(&acks), JournalAck::Failed(_)));
        appender.shutdown();
        drop(appender);

        let outcome =
            scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn")).unwrap();
        assert_eq!(
            outcome.records.len(),
            1,
            "only the valid prefix may be written"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Build an appender whose queue is never drained, so budget and
    /// capacity behaviour is deterministic.
    fn undrained_appender(capacity: usize, budget: usize) -> JournalAppender {
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        // Leak the receiver so the channel never disconnects and never
        // drains; the test process exits with it.
        std::mem::forget(rx);
        JournalAppender {
            tx,
            queued_bytes: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            queue_budget_bytes: budget,
            worker: std::sync::Mutex::new(None),
        }
    }

    #[test]
    fn queue_byte_budget_rejects_submission_without_dropping() {
        let appender = undrained_appender(64, 8);
        // Each record is 4 bytes; exactly two fit the 8-byte budget.
        appender.try_submit(event(1, b"1234")).unwrap();
        appender.try_submit(event(2, b"1234")).unwrap();
        assert_eq!(
            appender.try_submit(event(3, b"1234")),
            Err(JournalSubmitError::QueueBudgetExhausted)
        );
        // The rejected record is not enqueued: budget accounting is exact.
        assert_eq!(
            appender
                .queued_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            8
        );
    }

    #[test]
    fn queue_message_capacity_rejects_submission_without_dropping() {
        let appender = undrained_appender(1, DEFAULT_QUEUE_BUDGET_BYTES);
        appender.try_submit(event(1, b"a")).unwrap();
        assert_eq!(
            appender.try_submit(event(2, b"b")),
            Err(JournalSubmitError::QueueFull)
        );
    }

    #[test]
    fn policy_codec_roundtrips_and_rejects_malformed() {
        let payload = policy_payload("modes", "app_cursor_keys=1,bracketed_paste=0");
        assert_eq!(
            parse_policy(&payload),
            Some(("modes", "app_cursor_keys=1,bracketed_paste=0"))
        );
        assert_eq!(parse_policy(b"no-equals"), None);
        assert_eq!(parse_policy(b"=value"), None);
        assert_eq!(parse_policy(b"key=bad\nvalue"), None);
        assert_eq!(parse_policy(&[0xff, 0xfe]), None);
    }

    #[test]
    fn record_policy_validates_and_journals() {
        let dir = test_session_dir("policy");
        let (mut shadow, _, _) = ShadowJournal::open(&dir).unwrap();
        assert!(matches!(
            shadow.record_policy("", "x"),
            Err(JournalSubmitError::InvalidEvent(_))
        ));
        assert!(matches!(
            shadow.record_policy("a=b", "x"),
            Err(JournalSubmitError::InvalidEvent(_))
        ));
        assert!(matches!(
            shadow.record_policy("k", "x\ny"),
            Err(JournalSubmitError::InvalidEvent(_))
        ));
        assert_eq!(
            shadow.core.head_seq(),
            None,
            "invalid events never sequence"
        );

        shadow
            .record_policy("modes", "app_cursor_keys=1,bracketed_paste=1")
            .unwrap();
        shadow.request_sync();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.durable_seq() < 1 {
            shadow.poll_acks();
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn");
        let outcome = scan_segment(&segment).unwrap();
        assert_eq!(outcome.records.len(), 1);
        assert_eq!(outcome.records[0].kind, RecordKind::Policy);
        assert_eq!(
            parse_policy(&outcome.records[0].payload),
            Some(("modes", "app_cursor_keys=1,bracketed_paste=1"))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resize_payload_roundtrips_and_rejects_malformed() {
        let payload = encode_resize_payload(24, 80);
        assert_eq!(decode_resize_payload(&payload), Some((24, 80)));
        assert_eq!(decode_resize_payload(&payload[..3]), None);
        assert_eq!(decode_resize_payload(&[]), None);
        // Zero-sized geometry is invalid and must not decode.
        assert_eq!(decode_resize_payload(&encode_resize_payload(0, 80)), None);
        assert_eq!(decode_resize_payload(&encode_resize_payload(24, 0)), None);
    }

    #[test]
    fn lifecycle_payload_roundtrips_and_rejects_malformed() {
        let payload = encode_lifecycle_payload(LifecycleCode::Stopped, Some(0), "exit");
        assert_eq!(
            decode_lifecycle_payload(&payload),
            Some((LifecycleCode::Stopped, Some(0), "exit"))
        );
        // A clean exit code 0 stays distinguishable from an absent code.
        let payload = encode_lifecycle_payload(LifecycleCode::Failed, None, "signal");
        assert_eq!(
            decode_lifecycle_payload(&payload),
            Some((LifecycleCode::Failed, None, "signal"))
        );
        let payload = encode_lifecycle_payload(LifecycleCode::Started, None, "");
        assert_eq!(
            decode_lifecycle_payload(&payload),
            Some((LifecycleCode::Started, None, ""))
        );
        assert_eq!(decode_lifecycle_payload(&payload[..4]), None);
        let mut bad = encode_lifecycle_payload(LifecycleCode::Killed, Some(-9), "x");
        bad[0] = 0xEE;
        assert_eq!(decode_lifecycle_payload(&bad), None);
        let mut bad_utf8 = encode_lifecycle_payload(LifecycleCode::Killed, Some(-9), "x");
        *bad_utf8.last_mut().unwrap() = 0xFF;
        assert_eq!(decode_lifecycle_payload(&bad_utf8), None);
    }

    #[test]
    fn shadow_journal_orders_output_resize_and_lifecycle_in_one_stream() {
        let dir = test_session_dir("shadow_mixed_order");
        let (mut shadow, _, _) = ShadowJournal::open(&dir).unwrap();

        shadow.record_resize(24, 80).unwrap();
        shadow
            .record_lifecycle(LifecycleCode::Started, None, "pid=1")
            .unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"before"))
            .unwrap();
        shadow.record_resize(40, 120).unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"after"))
            .unwrap();
        shadow
            .record_lifecycle(LifecycleCode::Stopped, Some(0), "exit")
            .unwrap();
        shadow.request_sync();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.durable_seq() < 6 {
            assert!(
                std::time::Instant::now() < deadline,
                "durable ack timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
            shadow.poll_acks();
        }
        drop(shadow);

        let outcome =
            scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn")).unwrap();
        assert!(outcome.is_clean());
        assert_eq!(
            outcome.records.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![
                RecordKind::Resize,
                RecordKind::Lifecycle,
                RecordKind::Output,
                RecordKind::Resize,
                RecordKind::Output,
                RecordKind::Lifecycle,
            ]
        );
        assert_eq!(
            decode_resize_payload(&outcome.records[0].payload),
            Some((24, 80))
        );
        assert_eq!(
            decode_resize_payload(&outcome.records[3].payload),
            Some((40, 120))
        );
        assert_eq!(
            decode_lifecycle_payload(&outcome.records[5].payload),
            Some((LifecycleCode::Stopped, Some(0), "exit"))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // -- Fixed-range reads (I3) --

    fn write_ten_record_segment(dir: &Path) {
        let mut opened = open(dir).unwrap();
        for seq in 1..=10u64 {
            opened
                .writer
                .append_record(
                    RecordKind::Output,
                    seq,
                    seq,
                    format!("payload-{seq:02}").as_bytes(),
                )
                .unwrap();
        }
    }

    #[test]
    fn read_range_returns_exactly_the_requested_window() {
        let dir = test_session_dir("range_window");
        write_ten_record_segment(&dir);

        let read = read_range(&dir, 1, 3, 5, usize::MAX).unwrap();
        assert!(!read.truncated);
        assert_eq!(read.stop, ScanStop::CleanEof);
        assert_eq!(
            read.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(read.records[0].payload, b"payload-03".to_vec());

        // A window past the end clamps to what exists.
        let read = read_range(&dir, 1, 8, 100, usize::MAX).unwrap();
        assert_eq!(
            read.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![8, 9, 10]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_range_rejects_invalid_arguments_and_missing_incarnations() {
        let dir = test_session_dir("range_args");
        write_ten_record_segment(&dir);

        assert!(read_range(&dir, 1, 0, 5, usize::MAX).is_err());
        assert!(read_range(&dir, 1, 6, 5, usize::MAX).is_err());
        let missing = read_range(&dir, 2, 1, 5, usize::MAX).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_range_byte_budget_truncates_without_a_hole() {
        let dir = test_session_dir("range_budget");
        write_ten_record_segment(&dir);

        // Each payload is 10 bytes; a 25-byte budget fits two records.
        let read = read_range(&dir, 1, 1, 10, 25).unwrap();
        assert!(read.truncated);
        assert_eq!(
            read.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        // Resume where the budget cut the window.
        let rest = read_range(&dir, 1, 3, 10, usize::MAX).unwrap();
        assert!(!rest.truncated);
        assert_eq!(rest.records.len(), 8);

        // The first in-window record is always included, even when it
        // alone exceeds the budget, so callers can always make progress.
        let read = read_range(&dir, 1, 1, 10, 1).unwrap();
        assert!(read.truncated);
        assert_eq!(read.records.len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_range_validates_the_prefix_before_the_window() {
        let dir = test_session_dir("range_prefix");
        write_ten_record_segment(&dir);

        // Corrupt record 2's payload, then read a later window: the
        // corruption is in the consumed prefix and must surface.
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn");
        let mut bytes = fs::read(&segment).unwrap();
        let record_len = HEADER_LEN + 10;
        bytes[record_len + HEADER_LEN] ^= 0xFF;
        fs::write(&segment, &bytes).unwrap();

        let read = read_range(&dir, 1, 5, 7, usize::MAX).unwrap();
        assert_eq!(read.stop, ScanStop::CrcMismatch);
        assert!(read.records.is_empty(), "no window data past corruption");

        let _ = fs::remove_dir_all(&dir);
    }

    // -- Bounded tail, cross-incarnation history, retention (I3/I8) --

    #[test]
    fn read_tail_returns_the_newest_records_within_budget() {
        let dir = test_session_dir("tail_budget");
        write_ten_record_segment(&dir);

        // Payloads are 10 bytes; a 25-byte budget covers the newest two.
        let tail = read_tail(&dir, 1, 25).unwrap();
        assert_eq!(
            tail.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![9, 10]
        );
        let all = read_tail(&dir, 1, usize::MAX).unwrap();
        assert_eq!(all.records.len(), 10);
        // The newest record is always included, even over budget.
        let one = read_tail(&dir, 1, 1).unwrap();
        assert_eq!(one.records.len(), 1);
        assert_eq!(one.records[0].seq, 10);

        let _ = fs::remove_dir_all(&dir);
    }

    /// ADR-0002 acceptance: the journal-backed equivalent of
    /// `persist::tests::repro_truncated_log_reuses_offsets`. Cursors are
    /// `(incarnation, seq)`, so a restart can never alias new bytes onto
    /// an old cursor, and retention expiry fails loudly.
    #[test]
    fn history_cursors_never_alias_across_restart_and_retention() {
        let dir = test_session_dir("history_alias");

        let mut first = open(&dir).unwrap();
        for (seq, payload) in [
            (1, b"one".as_slice()),
            (2, b"two".as_slice()),
            (3, b"three".as_slice()),
        ] {
            first
                .writer
                .append_record(RecordKind::Output, seq, seq, payload)
                .unwrap();
        }
        drop(first);

        // "Crash"/restart: a new incarnation starts; old cursors keep
        // addressing the pre-restart bytes.
        let mut second = open(&dir).unwrap();
        assert_eq!(second.incarnation, 2);
        second
            .writer
            .append_record(RecordKind::Output, 1, 0, b"four")
            .unwrap();
        second
            .writer
            .append_record(RecordKind::Output, 2, 1, b"five")
            .unwrap();
        drop(second);

        let from_old = JournalCursor {
            incarnation: 1,
            seq: 2,
        };
        let read = read_history(&dir, from_old, usize::MAX).unwrap();
        assert!(!read.truncated);
        assert_eq!(
            read.events
                .iter()
                .map(|e| (e.cursor.incarnation, e.cursor.seq, e.payload.clone()))
                .collect::<Vec<_>>(),
            vec![
                (1, 2, b"two".to_vec()),
                (1, 3, b"three".to_vec()),
                (2, 1, b"four".to_vec()),
                (2, 2, b"five".to_vec()),
            ],
            "pre-restart cursors read pre-restart bytes, then continue across incarnations"
        );
        assert_eq!(
            read.next,
            Some(JournalCursor {
                incarnation: 2,
                seq: 3
            })
        );

        // Retention removes the sealed first incarnation.
        let deleted = retain_before_unchecked(&dir, 2).unwrap();
        assert_eq!(deleted, vec![1]);

        // A cursor into the removed incarnation fails loudly — never an
        // empty success, never aliased bytes.
        let expired = read_history(&dir, from_old, usize::MAX).unwrap_err();
        assert_eq!(expired.kind(), io::ErrorKind::NotFound);

        // Cursors into the retained incarnation are unaffected.
        let read = read_history(
            &dir,
            JournalCursor {
                incarnation: 2,
                seq: 1,
            },
            usize::MAX,
        )
        .unwrap();
        assert_eq!(read.events.len(), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_never_deletes_the_active_incarnation() {
        let dir = test_session_dir("retention_active");
        let _first = open(&dir).unwrap();
        let deleted = retain_before_unchecked(&dir, 99).unwrap();
        assert!(deleted.is_empty(), "the only (active) segment stays");
        assert!(
            dir.join(JOURNAL_DIR_NAME)
                .join("seg-00000001-0001.ojrn")
                .exists()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Rollover: sequences continue across bounded parts, and history,
    /// range and tail reads transparently cross part boundaries.
    #[test]
    fn sealed_part_manifest_covers_rotation_and_clean_shutdown() {
        let dir = test_session_dir("manifest");
        let (mut shadow, incarnation, _) =
            ShadowJournal::open_with_options(&dir, std::time::Duration::from_secs(3600), 512)
                .unwrap();
        for _ in 0..20 {
            shadow
                .record_output(bytes::Bytes::from(vec![b'x'; 64]))
                .unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.journal_seq() < 20 {
            shadow.poll_acks();
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Clean shutdown seals the active tail part too.
        shadow.shutdown();
        drop(shadow);

        let journal_dir = dir.join(JOURNAL_DIR_NAME);
        let entries = read_manifest(&journal_dir).unwrap();
        assert_eq!(entries.len(), 4, "3 rotated parts + sealed tail");
        assert!(entries.iter().all(|entry| entry.incarnation == incarnation));
        // Sequences are contiguous across sealed parts.
        assert_eq!(entries[0].first_seq, 1);
        for pair in entries.windows(2) {
            assert_eq!(pair[0].last_seq + 1, pair[1].first_seq);
        }
        assert_eq!(entries.last().unwrap().last_seq, 20);
        // Every entry matches its part exactly.
        assert_eq!(
            verify_manifest(&journal_dir),
            Vec::<String>::new(),
            "clean journal verifies"
        );

        // Tampering with a sealed part is detected by the CRC.
        let part_path = segment_path(&journal_dir, incarnation, 2);
        let mut bytes = std::fs::read(&part_path).unwrap();
        bytes[40] ^= 0xFF;
        std::fs::write(&part_path, &bytes).unwrap();
        let issues = verify_manifest(&journal_dir);
        assert_eq!(issues.len(), 1, "one tampered part, one issue");
        assert!(issues[0].contains("CRC-32 mismatch"), "{}", issues[0]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rollover_keeps_one_sequence_across_parts() {
        let dir = test_session_dir("rollover");
        // 100-byte records, 512-byte parts -> 5 records per part.
        let (mut shadow, incarnation, _) =
            ShadowJournal::open_with_options(&dir, std::time::Duration::from_secs(3600), 512)
                .unwrap();
        assert_eq!(incarnation, 1);
        for _ in 0..20 {
            shadow
                .record_output(bytes::Bytes::from(vec![b'x'; 64]))
                .unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.journal_seq() < 20 {
            shadow.poll_acks();
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let journal_dir = dir.join(JOURNAL_DIR_NAME);
        let parts = list_segments(&journal_dir).unwrap();
        assert_eq!(parts.len(), 4, "20 records roll over into 4 parts");
        assert_eq!(parts[0], (1, 1));
        assert_eq!(parts[3], (1, 4));

        // History reads cross parts seamlessly and contiguously.
        let history = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 1,
            },
            usize::MAX,
        )
        .unwrap();
        let seqs: Vec<u64> = history
            .events
            .iter()
            .map(|event| event.cursor.seq)
            .collect();
        assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
        assert!(!history.truncated);

        // Range reads cross parts too.
        let range = read_range(&dir, 1, 4, 8, usize::MAX).unwrap();
        assert_eq!(
            range.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![4, 5, 6, 7, 8]
        );

        // Tail reads seek within the newest part...
        let tail = read_tail(&dir, 1, 200).unwrap();
        assert_eq!(
            tail.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            vec![18, 19, 20]
        );
        // ...and cross into earlier parts when the budget demands it.
        let tail = read_tail(&dir, 1, 700).unwrap();
        assert_eq!(
            tail.records.iter().map(|r| r.seq).collect::<Vec<_>>(),
            (11..=20).collect::<Vec<_>>()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // -- Checkpoints and checkpoint-gated retention (PLAN §5.3, ADR-0002) --

    fn test_checkpoint(program: &[u8]) -> Checkpoint {
        Checkpoint {
            rows: 24,
            cols: 80,
            cursor: (7, 3),
            alt_screen: false,
            app_cursor_keys: true,
            bracketed_paste: false,
            filtered_offset: 1234,
            program: bytes::Bytes::copy_from_slice(program),
        }
    }

    #[test]
    fn checkpoint_codec_roundtrips_and_rejects_garbage() {
        let checkpoint = test_checkpoint(b"\x1b[2J\x1b[Hpainted");
        let encoded = encode_checkpoint(&checkpoint);
        assert_eq!(decode_checkpoint(&encoded).unwrap(), checkpoint);

        // Bad magic, wrong version, truncation, trailing bytes.
        let mut bad = encoded.to_vec();
        bad[0] = b'X';
        assert!(decode_checkpoint(&bad).is_err());
        let mut bad = encoded.to_vec();
        bad[4] = 0xEE;
        assert!(decode_checkpoint(&bad).is_err());
        assert!(decode_checkpoint(&encoded[..encoded.len() - 1]).is_err());
        let mut bad = encoded.to_vec();
        bad.push(0);
        assert!(decode_checkpoint(&bad).is_err());
    }

    #[test]
    fn checkpoint_v1_records_still_decode_without_a_filtered_offset() {
        // Legacy v1 layout: no filtered-offset field (19-byte fixed part).
        let checkpoint = test_checkpoint(b"\x1b[2Jlegacy");
        let mut v1 = Vec::new();
        v1.extend_from_slice(CHECKPOINT_MAGIC);
        v1.extend_from_slice(&1u16.to_le_bytes());
        v1.extend_from_slice(&checkpoint.rows.to_le_bytes());
        v1.extend_from_slice(&checkpoint.cols.to_le_bytes());
        v1.extend_from_slice(&checkpoint.cursor.0.to_le_bytes());
        v1.extend_from_slice(&checkpoint.cursor.1.to_le_bytes());
        v1.push(0b110); // alt_screen=0, app_cursor_keys=1, bracketed_paste=1
        v1.extend_from_slice(&(checkpoint.program.len() as u32).to_le_bytes());
        v1.extend_from_slice(&checkpoint.program);

        let decoded = decode_checkpoint(&v1).unwrap();
        assert_eq!(decoded.filtered_offset, 0, "v1 carries no anchor offset");
        assert!(decoded.app_cursor_keys);
        assert!(decoded.bracketed_paste);
        assert_eq!(decoded.program, checkpoint.program);
    }

    #[test]
    fn checkpoint_anchors_scan_headers_and_track_filtered_offsets() {
        let dir = test_session_dir("anchors");
        let (mut shadow, incarnation, _) = ShadowJournal::open(&dir).unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"aaa"))
            .unwrap();
        let mut checkpoint = checkpoint_payload();
        checkpoint.filtered_offset = 3;
        shadow.record_checkpoint(&checkpoint).unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"bbb"))
            .unwrap();
        checkpoint.filtered_offset = 6;
        shadow.record_checkpoint(&checkpoint).unwrap();
        shadow.shutdown();

        let anchors = checkpoint_anchors(&dir, incarnation).unwrap();
        assert_eq!(
            anchors
                .iter()
                .map(|anchor| (anchor.filtered_offset, anchor.cursor.seq))
                .collect::<Vec<_>>(),
            vec![(3, 2), (6, 4)],
            "one anchor per checkpoint, in journal order"
        );
        assert_eq!(anchors[0].cursor.incarnation, incarnation);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn retention_is_gated_on_the_newest_checkpoint() {
        let dir = test_session_dir("retention_gate");
        // Incarnation 1: plain output, no checkpoint.
        let (mut shadow, _, _) = ShadowJournal::open(&dir).unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"inc1"))
            .unwrap();
        // Seal incarnation 1 durably: an empty crash-leftover part would
        // be cleaned up by the next `open`, which is not what this test
        // exercises.
        shadow.shutdown();
        // Without any checkpoint on record, retention deletes nothing.
        assert!(retain_before(&dir, u64::MAX).unwrap().is_empty());
        assert_eq!(
            latest_checkpoint_incarnation(&dir).unwrap(),
            None,
            "no checkpoint yet"
        );

        // Incarnation 2: one output record and a checkpoint.
        let (mut shadow2, incarnation2, _) = ShadowJournal::open(&dir).unwrap();
        assert_eq!(incarnation2, 2);
        shadow2
            .record_output(bytes::Bytes::from_static(b"inc2"))
            .unwrap();
        shadow2
            .record_checkpoint(&test_checkpoint(b"restore"))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            shadow2.request_sync();
            shadow2.poll_acks();
            if shadow2.core.durable_seq() >= 2 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(latest_checkpoint_incarnation(&dir).unwrap(), Some(2));

        // Gated retention now deletes incarnation 1 (older than the
        // checkpoint's incarnation) but never the checkpoint's own.
        assert_eq!(retain_before(&dir, u64::MAX).unwrap(), vec![1]);
        assert_eq!(
            list_incarnations(&dir.join(JOURNAL_DIR_NAME)).unwrap(),
            vec![2]
        );
        // The checkpoint survives and stays decodable.
        let outcome = read_history(
            &dir,
            JournalCursor {
                incarnation: 2,
                seq: 1,
            },
            usize::MAX,
        )
        .unwrap();
        assert!(
            outcome
                .events
                .iter()
                .any(|event| event.kind == RecordKind::CheckpointRef)
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// An explicit retention horizon below the gate still wins: the gate
    /// permits, it does not force.
    #[test]
    fn retention_horizon_below_the_gate_is_respected() {
        let dir = test_session_dir("retention_clamp");
        let (mut shadow, _, _) = ShadowJournal::open(&dir).unwrap();
        shadow
            .record_checkpoint(&test_checkpoint(b"restore"))
            .unwrap();
        // Seal incarnation 1 durably (see the gated-retention test above).
        shadow.shutdown();
        let (mut shadow2, _, _) = ShadowJournal::open(&dir).unwrap();
        shadow2
            .record_checkpoint(&test_checkpoint(b"restore2"))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            shadow2.request_sync();
            shadow2.poll_acks();
            if shadow2.core.durable_seq() >= 1 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Gate would allow deleting incarnation 1, but the caller's
        // horizon says keep everything from incarnation 1 on.
        assert!(retain_before(&dir, 1).unwrap().is_empty());
        assert_eq!(
            list_incarnations(&dir.join(JOURNAL_DIR_NAME)).unwrap(),
            vec![1, 2]
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// PLAN2 §P2.5: byte-budget retention computes the highest
    /// `min_incarnation` such that the surviving sealed bytes fit under
    /// the cap. It honours three invariants:
    ///
    /// - never crosses the checkpoint gate (live + checkpoint-bearing
    ///   incarnations stay),
    /// - whole-incarnation granularity (either keep or drop such that
    ///   total ≤ cap),
    /// - recency bias (older incarnations are the ones dropped first).
    #[test]
    fn min_incarnation_for_byte_budget_never_falls_below_the_gate_and_drops_oldest_first() {
        let dir = test_session_dir("retention_byte_budget");
        let journal_dir = dir.join(JOURNAL_DIR_NAME);

        // Three incarnations, each with exactly one sealed part of
        // 1000 bytes — distinguishable incs let us assert ordered
        // deletion. Each incarnation is sealed by closing the runtime
        // and reopening; the second opens with a checkpoint so the gate
        // advances to incarnation 2.
        let (mut producer, _, _) = ShadowJournal::open(&dir).unwrap();
        producer
            .record_output(bytes::Bytes::from(vec![b'a'; 1000]))
            .unwrap();
        producer.shutdown();

        let (mut inc2, _, _) = ShadowJournal::open(&dir).unwrap();
        inc2.record_checkpoint(&test_checkpoint(b"first")).unwrap();
        inc2.record_output(bytes::Bytes::from(vec![b'b'; 1000]))
            .unwrap();
        // Drain the checkpoint payload before shutdown so it's part of
        // the sealed manifest, not just the in-memory queue.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            inc2.request_sync();
            inc2.poll_acks();
            if inc2.core.durable_seq() >= 1 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        inc2.shutdown();

        let (mut inc3, _, _) = ShadowJournal::open(&dir).unwrap();
        inc3.record_checkpoint(&test_checkpoint(b"second")).unwrap();
        inc3.record_output(bytes::Bytes::from(vec![b'c'; 1000]))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            inc3.request_sync();
            inc3.poll_acks();
            if inc3.core.durable_seq() >= 1 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        inc3.shutdown();

        // Sanity-check the manifest shape we just built. Total bytes
        // per incarnation exceed the raw payload because each journal
        // record carries a header (magic, kind, incarnation, seq,
        // elapsed_ms, payload_len) + a CRC-32 trailer — read the actual
        // values out of the manifest and use them in the cap arithmetic
        // below, so the test does not depend on the codec's exact
        // overhead.
        let bytes_per_inc: std::collections::HashMap<u64, u64> = {
            let mut map = std::collections::HashMap::new();
            for entry in read_manifest(&journal_dir).unwrap() {
                *map.entry(entry.incarnation).or_insert(0u64) += entry.bytes;
            }
            map
        };
        let b1 = *bytes_per_inc.get(&1).expect("inc 1 sealed");
        let b2 = *bytes_per_inc.get(&2).expect("inc 2 sealed");
        let b3 = *bytes_per_inc.get(&3).expect("inc 3 sealed");
        assert!(
            b1 > 1000 && b2 > 1000 && b3 > 1000,
            "all three incs carry > 1000 bytes (header + payload): {b1}/{b2}/{b3}"
        );

        // Cap = 0 means disabled: horizon is u64::MAX (delete nothing).
        assert_eq!(
            min_incarnation_for_byte_budget(&journal_dir, 0).unwrap(),
            u64::MAX
        );

        // Cap ≥ total bytes: keep everything, horizon lands at 1.
        // (Once b1 + b2 + b3 fits, every iteration step adds the next
        // pre-gate incarnation to the kept set.)
        let gate = latest_checkpoint_incarnation(&dir).unwrap().unwrap();
        assert_eq!(gate, 3, "checkpoint-bearing incarnation");
        let total = b1 + b2 + b3;
        assert_eq!(
            min_incarnation_for_byte_budget(&journal_dir, total).unwrap(),
            1,
            "with enough budget, every incarnation is kept"
        );
        assert_eq!(
            min_incarnation_for_byte_budget(&journal_dir, total + 1).unwrap(),
            1,
            "extra headroom must not push the horizon past 1"
        );

        // Drop one incarnation: pick a cap between (b3) and (b2 + b3).
        // That keeps [inc 2, inc 3] = b2 + b3 bytes ≤ cap, but adding
        // inc 1 would push us over.
        assert!(
            b3 < b2 + b3,
            "inc 2 must contribute bytes (different payload from gate)"
        );
        assert_eq!(
            min_incarnation_for_byte_budget(&journal_dir, b2 + b3).unwrap(),
            2,
            "drop only inc 1; keep [inc 2, inc 3] at exactly the cap"
        );

        // Drop two incarnations: pick a cap that fits inc 3 alone but
        // not inc 3 plus inc 2. With inc 2 = b2 and the gate at inc 3,
        // cap = b3 → kept + b2 > cap so inc 2 stays dropped, and
        // adding any older inc only grows the rejected set.
        assert!(
            b2 > 0 && b3 > 0,
            "helper needs positive sealed bytes to arithmetic against"
        );
        assert_eq!(
            min_incarnation_for_byte_budget(&journal_dir, b3).unwrap(),
            3,
            "drop inc 1 + inc 2; keep [inc 3] only"
        );

        // Cap = 1: nothing fits; horizon lands at the gate (3) because
        // the gate invariant always wins. This is what users see when
        // they pick an absurdly small cap by mistake.
        assert_eq!(min_incarnation_for_byte_budget(&journal_dir, 1).unwrap(), 3);

        // End-to-end smoke: drive the helper into the retention call
        // and verify the surviving incarnations.
        let horizon = min_incarnation_for_byte_budget(&journal_dir, b3).unwrap();
        let deleted = retain_before(&dir, horizon).unwrap();
        assert_eq!(deleted, vec![1, 2], "inc 1 and inc 2 must be deleted");
        let mut survivors = list_incarnations(&journal_dir).unwrap();
        survivors.sort();
        assert_eq!(survivors, vec![3]);

        let _ = fs::remove_dir_all(&dir);
    }

    /// PLAN2 §P2.5: when no checkpoint has ever been emitted
    /// (`latest_checkpoint_incarnation == None`), the byte-budget
    /// horizon must default to the latest incarnation, matching the
    /// gate semantics used by [`retain_before`].
    #[test]
    fn min_incarnation_for_byte_budget_without_a_checkpoint_uses_latest_as_gate() {
        let dir = test_session_dir("retention_byte_budget_no_gate");
        let journal_dir = dir.join(JOURNAL_DIR_NAME);

        // Two incarnations, no checkpoints anywhere — like a session
        // whose first checkpoint cycle has not fired yet.
        let (mut producer, _, _) = ShadowJournal::open(&dir).unwrap();
        producer
            .record_output(bytes::Bytes::from(vec![b'x'; 500]))
            .unwrap();
        producer.shutdown();

        let (mut inc2, _, _) = ShadowJournal::open(&dir).unwrap();
        inc2.record_output(bytes::Bytes::from(vec![b'y'; 500]))
            .unwrap();
        inc2.shutdown();

        assert!(
            latest_checkpoint_incarnation(&dir).unwrap().is_none(),
            "this fixture deliberately has no checkpoint"
        );

        // With the gate defaulting to the latest incarnation (2), no
        // pre-gate incarnation exists, so the horizon is 2 (the gate).
        assert_eq!(
            min_incarnation_for_byte_budget(&journal_dir, 100).unwrap(),
            2
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// A cursor beyond the recovered valid tail must fail loudly — never
    /// silently return the next incarnation's bytes or empty success
    /// (incomplete capture, PLAN.md §6.2).
    #[test]
    fn cursor_beyond_recovered_tail_fails_loudly() {
        let dir = test_session_dir("cursor_tail");
        write_ten_record_segment(&dir);

        let err = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 12,
            },
            usize::MAX,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("incomplete capture"),
            "unexpected error: {err}"
        );

        // Exactly at the recovered tail: legitimately caught up.
        let read = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 11,
            },
            usize::MAX,
        )
        .unwrap();
        assert!(read.events.is_empty());
        assert!(read.next.is_none());

        // Tear the tail, recover, and confirm cursors into the lost
        // region now fail while the rewound boundary still resumes.
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001-0001.ojrn");
        fs::OpenOptions::new()
            .append(true)
            .open(&segment)
            .unwrap()
            .write_all(b"\xde\xadpartial")
            .unwrap();
        let recovered = open(&dir).unwrap();
        assert_eq!(recovered.incarnation, 2);
        assert!(recovered.report.unwrap().rewound);

        let err = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 12,
            },
            usize::MAX,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // The rewound boundary resumes into the next incarnation.
        let read = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 11,
            },
            usize::MAX,
        )
        .unwrap();
        assert!(read.events.is_empty());

        // No journal at all is loud too.
        let empty = test_session_dir("cursor_empty");
        fs::create_dir_all(&empty).unwrap();
        let err = read_history(
            &empty,
            JournalCursor {
                incarnation: 1,
                seq: 1,
            },
            usize::MAX,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&empty);
    }

    /// Retention racing readers: readers observe either the complete
    /// incarnation or a truthful prefix of it, but never a hole; once
    /// deletion finishes, cursors into it fail loudly (I3).
    #[test]
    fn retention_concurrent_with_readers_never_shows_a_hole() {
        let dir = test_session_dir("retention_race");
        let (mut shadow, _, _) =
            ShadowJournal::open_with_options(&dir, std::time::Duration::from_secs(3600), 512)
                .unwrap();
        for _ in 0..15 {
            shadow
                .record_output(bytes::Bytes::from(vec![b'y'; 64]))
                .unwrap();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.journal_seq() < 15 {
            shadow.poll_acks();
            assert!(std::time::Instant::now() < deadline, "drain timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        drop(shadow);
        // A second incarnation so retention may delete the first.
        let (mut shadow2, incarnation2, _) = ShadowJournal::open(&dir).unwrap();
        assert_eq!(incarnation2, 2);
        shadow2
            .record_output(bytes::Bytes::from_static(b"inc2"))
            .unwrap();

        let reader_dir = dir.clone();
        let reader = std::thread::spawn(move || {
            for _ in 0..200 {
                let result = read_history(
                    &reader_dir,
                    JournalCursor {
                        incarnation: 1,
                        seq: 1,
                    },
                    usize::MAX,
                );
                match result {
                    Ok(read) => {
                        // Any observed incarnation-1 events must be a
                        // contiguous prefix 1..=K (newest-part-first
                        // deletion can only shorten from the tail).
                        let mut expected = 1u64;
                        for event in &read.events {
                            if event.cursor.incarnation == 1 {
                                assert_eq!(event.cursor.seq, expected, "hole in incarnation 1");
                                expected += 1;
                            }
                        }
                    }
                    // Retention may legitimately win the race.
                    Err(err) => assert!(
                        matches!(
                            err.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::InvalidData
                        ),
                        "unexpected read error: {err}"
                    ),
                }
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(2));
        let deleted = retain_before_unchecked(&dir, 2).unwrap();
        assert_eq!(deleted, vec![1]);
        reader.join().unwrap();

        assert!(
            list_segments(&dir.join(JOURNAL_DIR_NAME))
                .unwrap()
                .iter()
                .all(|&(incarnation, _)| incarnation == 2)
        );
        let err = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 1,
            },
            usize::MAX,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_budget_truncates_and_resumes_exactly() {
        let dir = test_session_dir("history_budget");
        write_ten_record_segment(&dir);

        let first = read_history(
            &dir,
            JournalCursor {
                incarnation: 1,
                seq: 1,
            },
            25,
        )
        .unwrap();
        assert!(first.truncated);
        assert_eq!(first.events.len(), 2);
        let resume = first.next.unwrap();
        assert_eq!(
            resume,
            JournalCursor {
                incarnation: 1,
                seq: 3
            }
        );

        let rest = read_history(&dir, resume, usize::MAX).unwrap();
        assert!(!rest.truncated);
        assert_eq!(rest.events.len(), 8);
        assert_eq!(rest.events[0].cursor.seq, 3);

        let _ = fs::remove_dir_all(&dir);
    }

    /// Disk-full degradation (PLAN.md §6.1 M1 exit: "disk stall cannot
    /// grow memory indefinitely" + explicit failure state): an appender
    /// whose writes fail must surface `Failed`, reject further records
    /// without writing past a hole, and leave the segment untouched.
    #[cfg(target_os = "linux")]
    #[test]
    fn disk_full_degrades_the_appender_without_a_hole() {
        let full = Path::new("/dev/full");
        // Some sandboxes lack /dev/full; skip instead of failing.
        let Ok(writer) = SegmentWriter::open_append(full) else {
            return;
        };
        let writer =
            RollingSegmentWriter::new(PathBuf::from("/dev"), 1, writer, DEFAULT_SEGMENT_MAX_BYTES);
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let (ack_tx, acks) = std::sync::mpsc::channel();
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = std::thread::spawn(move || {
            appender_loop(
                writer,
                rx,
                ack_tx,
                queued,
                std::time::Duration::from_millis(10),
            )
        });

        tx.send(AppenderMsg::Record(Box::new(event(1, b"x"))))
            .unwrap();
        match recv_ack(&acks) {
            JournalAck::Failed(reason) => {
                assert!(reason.contains("append failed"), "unexpected: {reason}");
            }
            other => panic!("expected append failure, got {other:?}"),
        }
        // Fail-fast afterwards: no partial recovery, no silent hole.
        tx.send(AppenderMsg::Record(Box::new(event(2, b"y"))))
            .unwrap();
        assert!(matches!(recv_ack(&acks), JournalAck::Failed(_)));
        tx.send(AppenderMsg::Shutdown).unwrap();
        worker.join().unwrap();
    }

    /// Regression for the `recv_timeout` starvation bug: with an absolute
    /// sync deadline, a producer that never lets the queue go idle must
    /// still see `durable_seq` advance within roughly one cadence.
    #[test]
    fn sync_deadline_fires_under_a_continuous_producer() {
        let dir = test_session_dir("sync_deadline");
        let (mut shadow, _, _) =
            ShadowJournal::open_with_sync_interval(&dir, std::time::Duration::from_millis(50))
                .unwrap();
        let start = std::time::Instant::now();
        while shadow.core.durable_seq() < 1 {
            // Gaps between records stay far below the cadence, so a
            // per-message timeout would keep resetting forever.
            shadow
                .record_output(bytes::Bytes::from_static(b"x"))
                .unwrap();
            shadow.poll_acks();
            std::thread::sleep(std::time::Duration::from_millis(2));
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "durable_seq never advanced under a continuous producer"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// A graceful shutdown must sync whatever it already acknowledged as
    /// journaled — otherwise "written" records can be lost without any
    /// failed ack (I8).
    #[test]
    fn shutdown_syncs_unsynced_records() {
        let dir = test_session_dir("shutdown_sync");
        let opened = open(&dir).unwrap();
        let writer = RollingSegmentWriter::new(
            opened.journal_dir.clone(),
            opened.incarnation,
            opened.writer,
            DEFAULT_SEGMENT_MAX_BYTES,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        let (ack_tx, acks) = std::sync::mpsc::channel();
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker = std::thread::spawn(move || {
            appender_loop(
                writer,
                rx,
                ack_tx,
                queued,
                // Long cadence: only the shutdown barrier may sync.
                std::time::Duration::from_secs(3600),
            )
        });
        tx.send(AppenderMsg::Record(Box::new(event(1, b"x"))))
            .unwrap();
        assert_eq!(recv_ack(&acks), JournalAck::Journaled(1));
        tx.send(AppenderMsg::Shutdown).unwrap();
        let ack = acks
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("shutdown must produce a final durability ack");
        assert_eq!(ack, JournalAck::Durable(1));
        worker.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stats_scan_counts_records_without_buffering() {
        let dir = test_session_dir("stats_scan");
        write_ten_record_segment(&dir);
        let path = segment_path(&dir.join(JOURNAL_DIR_NAME), 1, 1);
        let stats = scan_segment_stats(&path).unwrap();
        assert_eq!(stats.records, 10);
        assert_eq!(stats.last_seq, Some(10));
        assert_eq!(stats.valid_len, fs::metadata(&path).unwrap().len());
        assert_eq!(stats.stop, ScanStop::CleanEof);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_rejects_a_first_sequence_other_than_one() {
        let dir = test_session_dir("first_seq");
        let journal_dir = dir.join(JOURNAL_DIR_NAME);
        fs::create_dir_all(&journal_dir).unwrap();
        let path = segment_path(&journal_dir, 1, 1);
        let mut writer = SegmentWriter::create(&path).unwrap();
        writer
            .append_record(RecordKind::Output, 5, 0, b"orphan")
            .unwrap();
        drop(writer);
        let stats = scan_segment_stats(&path).unwrap();
        assert_eq!(stats.stop, ScanStop::SequenceDiscontinuity);
        assert_eq!(stats.records, 0);
        // ...but a continuation scan with the right expectation accepts it.
        let stats = scan_segment_stats_from(&path, Some(5)).unwrap();
        assert_eq!(stats.stop, ScanStop::CleanEof);
        assert_eq!(stats.records, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The M1 exit requirement "disk stall cannot grow memory
    /// indefinitely": once persistence degrades, further records are
    /// refused before they are cached, and the cache size freezes.
    #[test]
    fn degraded_journal_stops_growing() {
        let (_ack_tx, acks) = std::sync::mpsc::channel();
        let mut shadow = ShadowJournal {
            core: SequencerCore::new(1),
            appender: undrained_appender(64, 8),
            acks,
        };
        let payload = || bytes::Bytes::from_static(b"1234");
        shadow.record_output(payload()).unwrap();
        shadow.record_output(payload()).unwrap();
        // Budget exhausted: this event is published, then the submit
        // failure degrades the core.
        assert_eq!(
            shadow.record_output(payload()),
            Err(JournalSubmitError::QueueBudgetExhausted)
        );
        assert!(shadow.core.is_degraded());
        let cache_at_degrade = shadow.core.cache_bytes();
        let head_at_degrade = shadow.core.head_seq();

        for _ in 0..1000 {
            assert_eq!(
                shadow.record_output(payload()),
                Err(JournalSubmitError::PersistenceDegraded)
            );
        }
        assert_eq!(shadow.core.cache_bytes(), cache_at_degrade);
        assert_eq!(shadow.core.head_seq(), head_at_degrade);
    }

    #[test]
    fn sync_cadence_advances_durable_seq_without_explicit_requests() {
        let dir = test_session_dir("sync_cadence");
        let (mut shadow, _, _) =
            ShadowJournal::open_with_sync_interval(&dir, std::time::Duration::from_millis(10))
                .unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"tick"))
            .unwrap();

        // No request_sync: the cadence tick must flush on its own.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.durable_seq() < 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "cadence sync timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
            shadow.poll_acks();
        }
        assert_eq!(shadow.core.durable_seq(), 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shadow_journal_orders_caches_and_journals_output() {
        let dir = test_session_dir("shadow_journal");
        let (mut shadow, incarnation, report) = ShadowJournal::open(&dir).unwrap();
        assert_eq!(incarnation, 1);
        assert!(report.is_none());

        let first = shadow
            .record_output(bytes::Bytes::from_static(b"one"))
            .unwrap();
        let second = shadow
            .record_output(bytes::Bytes::from_static(b"two"))
            .unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);

        shadow.request_sync();
        // Wait for the durable ack to arrive.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shadow.core.durable_seq() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "durable ack timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
            shadow.poll_acks();
        }
        assert_eq!(shadow.core.journal_seq(), 2);
        assert!(!shadow.core.is_degraded());

        let _ = fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------------
    // Manifest / retention / verification coherence (corrective increment)
    // ------------------------------------------------------------------

    /// Write one incarnation of output records and seal it (clean
    /// shutdown), returning the incarnation number.
    fn append_incarnation(session_dir: &Path, chunks: &[&[u8]], max_part_bytes: u64) -> u64 {
        let (mut journal, incarnation, _report) = ShadowJournal::open_with_options(
            session_dir,
            std::time::Duration::from_secs(3600),
            max_part_bytes,
        )
        .unwrap();
        for chunk in chunks {
            journal
                .record_output(bytes::Bytes::copy_from_slice(chunk))
                .unwrap();
        }
        journal.shutdown();
        incarnation
    }

    fn checkpoint_payload() -> Checkpoint {
        Checkpoint {
            rows: 24,
            cols: 80,
            cursor: (1, 1),
            alt_screen: false,
            app_cursor_keys: false,
            bracketed_paste: false,
            filtered_offset: 42,
            program: bytes::Bytes::from_static(b"\x1b[2Jrestored"),
        }
    }

    /// Drop the last `count` lines of `manifest.log`, simulating a crash
    /// that lost the final manifest appends.
    fn truncate_manifest_lines(journal_dir: &Path, count: usize) {
        let path = journal_dir.join(MANIFEST_FILE_NAME);
        let lines: Vec<String> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        let keep = lines.len() - count;
        let mut text = lines[..keep].join("\n");
        if keep > 0 {
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
    }

    /// The cross-feature lifecycle (mandatory corrective-increment gate):
    /// write → checkpoint → seal → retain → reopen → replay/resume →
    /// verify (the journal integrity core). Intentional retention must be
    /// distinguishable from corruption end to end.
    #[test]
    fn retention_checkpoint_manifest_and_verification_stay_coherent() {
        let session_dir = test_session_dir("retain-coherent");
        let journal_dir = session_dir.join(JOURNAL_DIR_NAME);

        assert_eq!(
            append_incarnation(&session_dir, &[b"one", b"two"], 1 << 20),
            1
        );
        // Incarnation 2 carries the checkpoint that gates retention.
        let (mut journal, incarnation, _report) = ShadowJournal::open_with_options(
            &session_dir,
            std::time::Duration::from_secs(3600),
            1 << 20,
        )
        .unwrap();
        assert_eq!(incarnation, 2);
        journal
            .record_output(bytes::Bytes::from_static(b"three"))
            .unwrap();
        journal.record_checkpoint(&checkpoint_payload()).unwrap();
        journal.shutdown();

        assert!(verify_manifest(&journal_dir).is_empty());
        let deleted = retain_before(&session_dir, u64::MAX).unwrap();
        assert_eq!(deleted, vec![1]);
        assert!(!segment_path(&journal_dir, 1, 1).exists());
        // The tombstone makes the deletion intentional and auditable.
        assert!(
            read_manifest_lines(&journal_dir)
                .unwrap()
                .iter()
                .any(|line| matches!(line, ManifestLine::Retired(r) if r.retired == 1))
        );
        assert!(verify_manifest(&journal_dir).is_empty());
        // The checkpoint gate is unaffected by the deletion it permitted.
        assert_eq!(
            latest_checkpoint_incarnation(&session_dir).unwrap(),
            Some(2)
        );

        // Replay/resume reads the surviving (newest) incarnation from its
        // start — before any reopen creates a newer, empty one.
        let resumed =
            crate::session::replay::filtered_stream_window(&session_dir, 0, 1 << 20).unwrap();
        assert_eq!(resumed, b"three");

        // Reopen: a new incarnation above the tombstoned one.
        let opened = open(&session_dir).unwrap();
        assert_eq!(opened.incarnation, 3);
        drop(opened.writer);
        assert!(verify_manifest(&journal_dir).is_empty());

        let _ = fs::remove_dir_all(&session_dir);
    }

    #[test]
    fn open_completes_interrupted_retention_and_never_reuses_incarnations() {
        let session_dir = test_session_dir("retain-crash");
        let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
        append_incarnation(&session_dir, &[b"one", b"two"], 1 << 20);
        append_incarnation(&session_dir, &[b"three"], 1 << 20);

        // Crash after the tombstone landed but before the unlinks.
        RetiredIncarnation { retired: 1 }
            .append_to(&journal_dir)
            .unwrap();
        let issues = verify_manifest(&journal_dir);
        assert_eq!(issues.len(), 1, "unexpected issues: {issues:?}");
        assert!(issues[0].contains("tombstoned incarnation 1"));

        // A tombstone bounds future incarnation numbers even when it names
        // an incarnation with no files on disk.
        RetiredIncarnation { retired: 7 }
            .append_to(&journal_dir)
            .unwrap();
        let opened = open(&session_dir).unwrap();
        assert_eq!(opened.incarnation, 8);
        drop(opened.writer);
        assert!(!segment_path(&journal_dir, 1, 1).exists());
        assert!(verify_manifest(&journal_dir).is_empty());

        let _ = fs::remove_dir_all(&session_dir);
    }

    #[test]
    fn open_seals_validated_parts_orphaned_by_a_crash() {
        let session_dir = test_session_dir("orphan-seal");
        let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
        // 48-byte records with a 64-byte cap rotate every record.
        append_incarnation(
            &session_dir,
            &[b"aaaaaaaaaaaa", b"bbbbbbbbbbbb", b"cccccccccccc"],
            64,
        );
        assert_eq!(list_segments(&journal_dir).unwrap().len(), 3);

        // Crash between part sync and manifest append, twice: parts 2 and
        // 3 lost their entries. Part 3 is the active tail (skipped by
        // verification); part 2 is an unmanifested non-tail (flagged).
        truncate_manifest_lines(&journal_dir, 2);
        let issues = verify_manifest(&journal_dir);
        assert_eq!(issues.len(), 1, "unexpected issues: {issues:?}");
        assert!(issues[0].contains("no manifest entry"));
        // The unmanifested bytes are still readable: replay sees every
        // chunk even before any repair.
        let resumed =
            crate::session::replay::filtered_stream_window(&session_dir, 0, 1 << 20).unwrap();
        assert_eq!(resumed, b"aaaaaaaaaaaabbbbbbbbbbbbcccccccccccc");

        // Reopen validates both and seals them; verification is clean.
        let opened = open(&session_dir).unwrap();
        assert_eq!(opened.incarnation, 2);
        drop(opened.writer);
        assert!(verify_manifest(&journal_dir).is_empty());
        assert_eq!(read_manifest(&journal_dir).unwrap().len(), 3);

        let _ = fs::remove_dir_all(&session_dir);
    }

    #[test]
    fn open_removes_empty_crash_leftover_parts() {
        let session_dir = test_session_dir("empty-leftover");
        let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
        append_incarnation(&session_dir, &[b"data"], 1 << 20);
        // A crash between rollover-create and the first append leaves an
        // empty newest part of the next incarnation.
        SegmentWriter::create(&segment_path(&journal_dir, 2, 1)).unwrap();
        // It is the active tail, so verification does not flag it (a live
        // daemon may legitimately hold one).
        assert!(verify_manifest(&journal_dir).is_empty());

        let opened = open(&session_dir).unwrap();
        assert_eq!(opened.incarnation, 3);
        drop(opened.writer);
        assert!(!segment_path(&journal_dir, 2, 1).exists());
        assert!(verify_manifest(&journal_dir).is_empty());

        let _ = fs::remove_dir_all(&session_dir);
    }
}
