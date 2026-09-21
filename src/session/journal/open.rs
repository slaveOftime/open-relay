//! Open-with-recovery: validates the journal dir, rewinds torn tails,
//! and hands out the [`OpenedJournal`] / [`RecoveryReport`] handle
//! (PLAN2 S1.5 step 6).
//!
//! Lifted from `session/journal/mod.rs`. The open-path (`RecoveryReport`,
//! `OpenedJournal`, `open`, `complete_retired_retention`,
//! `seal_validated_part`, `sync_dir`, `JournalCursor`, `OrderedEvent`)
//! is byte-identical to the previous inline definitions. `JournalCursor`
//! and `OrderedEvent` are only used inside `crate::session::journal` and
//! its tests today, but stay crate-public so callers in `runtime` /
//! `replay` can grow into them without another refactor.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use super::{
    Crc32, JOURNAL_DIR_NAME, RecordKind, ScanStop, SegmentManifestEntry, SegmentWriter,
    list_segments, part_first_seq, read_manifest, retired_incarnations, scan_segment_stats_from,
    segment_path,
};

/// What recovery found in the previous incarnation's newest segment part.
#[derive(Debug)]
pub struct RecoveryReport {
    /// Incarnation the recovered segment belongs to.
    pub incarnation: u64,
    /// Valid records recovered **in the newest part** (sequences continue
    /// across parts, so this is a suffix count, not the incarnation's
    /// total).
    pub records: u64,
    /// Highest valid sequence in that incarnation, if any.
    pub last_seq: Option<u64>,
    /// Why the scan stopped.
    pub stop: ScanStop,
    /// Whether a torn tail was rewound (file truncated to the valid
    /// prefix).
    pub rewound: bool,
}

/// A journal that has been opened for appending: the new incarnation's
/// segment writer plus the recovery report for the previous incarnation.
pub struct OpenedJournal {
    pub incarnation: u64,
    pub report: Option<RecoveryReport>,
    pub writer: SegmentWriter,
    /// `sessions/<id>/journal/` — where rollover creates the next parts.
    pub journal_dir: PathBuf,
}

/// Open (creating if needed) the journal for `session_dir`, recover the
/// newest existing incarnation, and create a new one. A daemon restart
/// opens a **new** incarnation (a new `seg-NNNNNNNN.ojrn` file) rather
/// than appending past possibly-published sequences; the previous
/// incarnation's torn tail is rewound, corruption is reported and left
/// untouched.
pub fn open(session_dir: &Path) -> io::Result<OpenedJournal> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    fs::create_dir_all(&journal_dir)?;

    // Complete any retention interrupted by a crash: the tombstone lands
    // in the manifest before deletion starts, so leftover parts of a
    // retired incarnation are intentional deletions, finished here. The
    // retired set is also a lower bound for the next incarnation number:
    // a tombstoned incarnation must never be reused, even when retention
    // removed its every file.
    let retired = retired_incarnations(&journal_dir)?;
    complete_retired_retention(&journal_dir, &retired)?;

    let segments = list_segments(&journal_dir)?;
    // The manifest is read at open for reconciliation (post-review
    // corrective increment): a malformed manifest is genuine corruption
    // and fails the open loudly instead of risking duplicate entries.
    let manifested: std::collections::HashSet<(u64, u64)> = read_manifest(&journal_dir)?
        .into_iter()
        .map(|entry| (entry.incarnation, entry.part))
        .collect();
    let mut report = None;
    if let Some(&(previous, part)) = segments.last() {
        let path = segment_path(&journal_dir, previous, part);
        // Only the newest part of the newest incarnation can be torn by a
        // crash; earlier parts were sealed by the appender. Recovery must
        // not buffer the recording: the stats scan keeps memory O(1) while
        // still validating every record of the part.
        let expected_first = match part_first_seq(&path) {
            Ok(first) => first,
            // Torn/corrupt first header: let the scanner classify it
            // (a torn tail rewinds; anything else is reported).
            Err(err) if err.kind() == io::ErrorKind::InvalidData => None,
            Err(err) => return Err(err),
        };
        if part == 1 && expected_first.is_some_and(|first| first != 1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: incarnation prefix must start at seq 1", path.display()),
            ));
        }
        let stats = scan_segment_stats_from(&path, expected_first)?;
        let rewound = stats.stop.may_rewind();
        if rewound {
            // A torn tail can only mean the previous incarnation died
            // mid-append; truncate so no reader ever sees it again.
            fs::OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_len(stats.valid_len)?;
            sync_dir(&journal_dir)?;
        }
        // Recovery just validated (and possibly rewound) this part. If a
        // crash killed the appender between part sync and manifest
        // append, seal it now so every non-active part is manifested and
        // `verify_manifest` stays strictly truthful.
        if !manifested.contains(&(previous, part)) {
            match (expected_first, stats.last_seq, stats.stop) {
                (Some(first), Some(last), ScanStop::CleanEof | ScanStop::PartialTail) => {
                    seal_validated_part(&journal_dir, previous, part, first, last)?;
                }
                // An empty crash-leftover tail (rollover created the file
                // but no record ever landed): remove it; nothing is lost.
                (None, None, ScanStop::CleanEof) if stats.valid_len == 0 => {
                    fs::remove_file(&path)?;
                    sync_dir(&journal_dir)?;
                }
                // Anything else is corruption: reported above, left
                // untouched for `verify_manifest` to flag.
                _ => {}
            }
        }
        report = Some(RecoveryReport {
            incarnation: previous,
            records: stats.records,
            last_seq: stats.last_seq,
            stop: stats.stop,
            rewound,
        });
    }
    // Reconcile any other unmanifested part: the crash window between a
    // rotated part's sync and its manifest append can also leave older
    // parts unsealed. Validated parts are sealed; empty leftovers are
    // removed; anything invalid is left for verification to report.
    let previous_tail = segments.last().copied();
    for &(incarnation, part) in &segments {
        if Some((incarnation, part)) == previous_tail || manifested.contains(&(incarnation, part)) {
            continue;
        }
        let path = segment_path(&journal_dir, incarnation, part);
        let expected_first = match part_first_seq(&path) {
            Ok(first) => first,
            Err(err) if err.kind() == io::ErrorKind::InvalidData => continue,
            Err(err) => return Err(err),
        };
        let stats = scan_segment_stats_from(&path, expected_first)?;
        match (expected_first, stats.last_seq, stats.stop) {
            (Some(first), Some(last), ScanStop::CleanEof) => {
                seal_validated_part(&journal_dir, incarnation, part, first, last)?;
            }
            (None, None, ScanStop::CleanEof) if stats.valid_len == 0 => {
                fs::remove_file(&path)?;
                sync_dir(&journal_dir)?;
            }
            _ => {}
        }
    }

    let highest_retired = retired.iter().copied().max().unwrap_or(0);
    let incarnation = segments
        .last()
        .map(|&(incarnation, _)| incarnation)
        .unwrap_or(0)
        .max(highest_retired)
        + 1;
    let writer = SegmentWriter::create(&segment_path(&journal_dir, incarnation, 1))?;
    // Make the new segment's directory entry durable alongside the file;
    // record durability is meaningless if the name can vanish on crash.
    sync_dir(&journal_dir)?;
    Ok(OpenedJournal {
        incarnation,
        report,
        writer,
        journal_dir,
    })
}

/// Finish deleting any leftover parts of tombstoned (retired)
/// incarnations. Called at open so a crash mid-retention is self-healing
/// rather than a permanent verification failure.
fn complete_retired_retention(
    journal_dir: &Path,
    retired: &std::collections::HashSet<u64>,
) -> io::Result<()> {
    if retired.is_empty() {
        return Ok(());
    }
    let mut removed_any = false;
    for (incarnation, part) in list_segments(journal_dir)? {
        if retired.contains(&incarnation) {
            fs::remove_file(segment_path(journal_dir, incarnation, part))?;
            removed_any = true;
        }
    }
    if removed_any {
        sync_dir(journal_dir)?;
    }
    Ok(())
}

/// Seal a part that recovery/reconciliation has just fully validated but
/// the appender never committed to the manifest (crash between part sync
/// and manifest append). Reads the part in full; only called at open for
/// the rare unmanifested case, never on the hot path.
fn seal_validated_part(
    journal_dir: &Path,
    incarnation: u64,
    part: u64,
    first_seq: u64,
    last_seq: u64,
) -> io::Result<()> {
    let path = segment_path(journal_dir, incarnation, part);
    let bytes = fs::read(&path)?;
    let entry = SegmentManifestEntry {
        incarnation,
        part,
        first_seq,
        last_seq,
        bytes: bytes.len() as u64,
        crc32: Crc32::of(&bytes),
    };
    entry.append_to(journal_dir)?;
    sync_dir(journal_dir)
}

/// Best-effort directory sync so segment creation/truncation survives a
/// crash. Unsupported on non-Unix targets, where this is a no-op.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Ordered events and the in-memory sequencing core (M1)
// ---------------------------------------------------------------------------

/// Durable cursor for one journal record: `{session_id, incarnation, seq}`
/// (the session id is implicit in the journal's location).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct JournalCursor {
    pub incarnation: u64,
    pub seq: u64,
}

/// One immutable, already-sequenced session event. Sequence and timing are
/// assigned **before** publication (PLAN.md §4.1 item 3); the payload is
/// reference-counted so live delivery, the recent replay cache and the
/// journal queue share one allocation.
#[derive(Debug, Clone)]
pub struct OrderedEvent {
    pub cursor: JournalCursor,
    pub elapsed_ms: u64,
    pub kind: RecordKind,
    pub payload: bytes::Bytes,
}
