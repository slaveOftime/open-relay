//! Stream / range / history / tail read APIs (PLAN2 S1.5 step 4).
//!
//! Lifted from `session/journal/mod.rs`. The reader-path APIs
//! (`RangeRead`, `read_range`, `SegmentStream`, `incarnation_parts`,
//! `CollectWindow`, `IndexEntry`, `SegmentIndex`, `scan_segment_index`,
//! `TailRead`, `read_tail`, `HistoryEvent`, `HistoryRead`, `read_history`,
//! `recovered_tail`) is byte-identical to the previous inline definitions.

use std::{
    io,
    path::{Path, PathBuf},
};

use super::{
    CheckpointAnchor, JOURNAL_DIR_NAME, Record, ScanMode, ScanStart, ScanStop, list_segments,
    scan_impl, segment_path,
};
#[cfg(test)]
use super::{
    JournalCursor, MAX_PAYLOAD_LEN, RecordKind, SPARSE_INDEX_STRIDE_BYTES, fs, list_incarnations,
    part_first_seq, scan_segment_stats_from,
};

/// Result of a fixed-range read: the in-window records (contiguous, in
/// order), the integrity status of the consumed prefix, and whether the
/// byte budget cut the window short.
///
/// `stop` reports stream integrity of everything read up to the stopping
/// point. On a **live** segment `ScanStop::PartialTail` is normal — it
/// means "more is being appended right now", not corruption. When
/// `truncated` is set, retry with `from_seq = last_returned_seq + 1`.
#[derive(Debug)]
#[cfg(test)]
pub struct RangeRead {
    pub records: Vec<Record>,
    pub stop: ScanStop,
    pub truncated: bool,
}

/// Read the records of one incarnation whose sequences fall inside
/// `from_seq..=to_seq`, buffering at most `max_bytes` of payload (the
/// first in-window record is always included, even if it alone exceeds
/// the budget). Continuity and integrity of the whole consumed prefix are
/// still validated — a range read never presents a silent hole (I3).
#[cfg(test)]
pub fn read_range(
    session_dir: &Path,
    incarnation: u64,
    from_seq: u64,
    to_seq: u64,
    max_bytes: usize,
) -> io::Result<RangeRead> {
    if from_seq == 0 || from_seq > to_seq {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid journal range {from_seq}..={to_seq}"),
        ));
    }
    let parts = incarnation_parts(&session_dir.join(JOURNAL_DIR_NAME), incarnation)?;
    let mut records = Vec::new();
    let mut buffered_bytes = 0usize;
    let mut truncated = false;
    let mut stop = ScanStop::CleanEof;
    // Cross-part continuity is enforced while reading: each part must
    // begin exactly where the previous part's validated scan ended.
    let mut expected_first: Option<u64> = Some(1);
    for (_, path) in &parts {
        if expected_first.is_some_and(|next| next > to_seq) {
            break; // everything left is beyond the window
        }
        if buffered_bytes >= max_bytes && !records.is_empty() {
            truncated = true;
            break;
        }
        let remaining = max_bytes.saturating_sub(buffered_bytes).max(1);
        let result = scan_impl(
            path,
            ScanMode::Window(CollectWindow {
                from_seq,
                to_seq,
                max_buffered_bytes: remaining,
            }),
            expected_first,
            None,
        )?;
        expected_first = result.last_seq.map(|seq| seq + 1).or(expected_first);
        truncated |= result.truncated;
        buffered_bytes += result
            .outcome
            .records
            .iter()
            .map(|record| record.payload.len())
            .sum::<usize>();
        records.extend(result.outcome.records);
        stop = result.outcome.stop;
        if !matches!(stop, ScanStop::CleanEof) || truncated {
            break; // corrupt or torn part: never scan past it silently
        }
    }
    Ok(RangeRead {
        records,
        stop,
        truncated,
    })
}

/// All parts of one incarnation as `(part, path)`, ascending; `NotFound`
/// when the incarnation is not retained. Part numbering must be
/// contiguous from 1 — a hole means retention deleted the wrong thing or
/// the directory is corrupt, and reads must fail rather than silently
/// skip a range (I3). (Retention deletes newest-part-first, so a
/// concurrent reader can observe a *prefix* of a being-deleted
/// incarnation — cursors stay truthful — but never a hole. The M3
/// manifest will make retention/reader coordination exact.)
pub(crate) fn incarnation_parts(
    journal_dir: &Path,
    incarnation: u64,
) -> io::Result<Vec<(u64, PathBuf)>> {
    let parts: Vec<(u64, PathBuf)> = list_segments(journal_dir)?
        .into_iter()
        .filter(|&(inc, _)| inc == incarnation)
        .map(|(inc, part)| (part, segment_path(journal_dir, inc, part)))
        .collect();
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no journal segment for incarnation {incarnation}"),
        ));
    }
    if parts
        .iter()
        .enumerate()
        .any(|(index, &(part, _))| part != index as u64 + 1)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("journal incarnation {incarnation} has a hole in its segment parts"),
        ));
    }
    Ok(parts)
}

/// Streaming reader over one incarnation, pulling bounded batches from a
/// resume position (PLAN §5.3 anchored replay). The resume position is
/// either the journal start (prefix fully validated) or the record
/// following a [`CheckpointAnchor`] — the skipped prefix is then trusted
/// as of the anchor, the same trust model the sparse tail index uses for
/// sealed parts. Headers, CRCs, and cross-part continuity are validated
/// from the resume position onward; corruption from there is an error,
/// never a silently truncated stream.
pub struct SegmentStream {
    parts: Vec<(u64, PathBuf)>,
    part_index: usize,
    byte_offset: u64,
    next_seq: u64,
    done: bool,
}

impl SegmentStream {
    /// Stream from the journal start, validating the whole prefix.
    pub fn open(session_dir: &Path, incarnation: u64) -> io::Result<Self> {
        let parts = incarnation_parts(&session_dir.join(JOURNAL_DIR_NAME), incarnation)?;
        Ok(Self {
            parts,
            part_index: 0,
            byte_offset: 0,
            next_seq: 1,
            done: false,
        })
    }

    /// Stream starting at the record following a checkpoint anchor,
    /// skipping the anchored prefix entirely.
    pub fn open_at(
        session_dir: &Path,
        incarnation: u64,
        anchor: &CheckpointAnchor,
    ) -> io::Result<Self> {
        debug_assert_eq!(anchor.cursor.incarnation, incarnation);
        let parts = incarnation_parts(&session_dir.join(JOURNAL_DIR_NAME), incarnation)?;
        let part_index = parts
            .iter()
            .position(|(part, _)| *part == anchor.part)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "checkpoint anchor segment part is not retained",
                )
            })?;
        Ok(Self {
            parts,
            part_index,
            byte_offset: anchor.record_end_offset,
            next_seq: anchor.cursor.seq + 1,
            done: false,
        })
    }

    /// Pull the next batch, buffering at most `max_bytes` of payload (the
    /// first record of a batch is always included). An empty batch means
    /// the validated stream end — or a torn live tail — is reached.
    pub fn next_batch(&mut self, max_bytes: usize) -> io::Result<Vec<Record>> {
        let mut records = Vec::new();
        while !self.done && records.is_empty() {
            let Some((_, path)) = self.parts.get(self.part_index) else {
                self.done = true;
                break;
            };
            let start = (self.byte_offset > 0).then_some(ScanStart {
                offset: self.byte_offset,
                seq: self.next_seq,
            });
            let result = scan_impl(
                path,
                ScanMode::Window(CollectWindow {
                    from_seq: self.next_seq,
                    to_seq: u64::MAX,
                    max_buffered_bytes: max_bytes,
                }),
                (self.byte_offset == 0).then_some(self.next_seq),
                start,
            )?;
            if let Some(last) = result.outcome.records.last() {
                self.next_seq = last.seq + 1;
            }
            match result.outcome.stop {
                ScanStop::CleanEof if result.truncated => {
                    // Budget stop mid-part: resume at the validated end.
                    self.byte_offset = result.outcome.valid_len;
                    records = result.outcome.records;
                }
                ScanStop::CleanEof | ScanStop::PartialTail => {
                    // Part exhausted (or torn live tail): advance.
                    self.part_index += 1;
                    self.byte_offset = 0;
                    records = result.outcome.records;
                }
                stop => {
                    self.done = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("journal segment is corrupt from the resume position: {stop:?}"),
                    ));
                }
            }
        }
        Ok(records)
    }
}

/// Window/budget for a range read.
pub(crate) struct CollectWindow {
    pub(crate) from_seq: u64,
    pub(crate) to_seq: u64,
    pub(crate) max_buffered_bytes: usize,
}

/// Where one record lives inside its segment file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // reached from `scan::scan_impl` (cfg(test) build arm) via field type at `scan::ScanResult::index`
pub(crate) struct IndexEntry {
    pub seq: u64,
    /// Byte offset of the record header inside the segment.
    pub offset: u64,
    /// Header plus payload length.
    pub record_len: u64,
}

/// Payload-free sparse segment index: one entry per at most
/// [`SPARSE_INDEX_STRIDE_BYTES`] of data, so memory is bounded by part
/// size / stride regardless of record count. Basis for bounded tail
/// reads (and later persisted O(1) seeks).
#[derive(Debug)]
#[cfg(test)]
pub struct SegmentIndex {
    pub entries: Vec<IndexEntry>,
    pub valid_len: u64,
    pub stop: ScanStop,
}

#[cfg(test)]
impl SegmentIndex {
    /// Where to start reading so that the scanned region covers at least
    /// the newest `max_bytes` bytes of the valid prefix. Returns
    /// `(seq, offset)`; the region length is `valid_len - offset`, at
    /// most `max_bytes + stride + one record`.
    #[cfg(test)]
    fn tail_start(&self, max_bytes: u64) -> Option<(u64, u64)> {
        let first = self.entries.first()?;
        if self.valid_len <= max_bytes {
            return Some((first.seq, first.offset));
        }
        let target = self.valid_len - max_bytes;
        let entry = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.offset <= target)
            .unwrap_or(first);
        Some((entry.seq, entry.offset))
    }
}

/// Scan a segment collecting only the sparse record index (payloads are
/// validated but not retained).
#[cfg(test)]
pub fn scan_segment_index(path: &Path) -> io::Result<SegmentIndex> {
    // Lenient on the first sequence: parts after the first continue the
    // incarnation's sequence, and cross-part continuity is validated by
    // the read assembly, not the per-part index.
    let result = scan_impl(path, ScanMode::Index, None, None)?;
    Ok(SegmentIndex {
        entries: result.index,
        valid_len: result.outcome.valid_len,
        stop: result.outcome.stop,
    })
}

/// Result of a bounded tail read.
#[derive(Debug)]
#[cfg(test)]
pub struct TailRead {
    pub records: Vec<Record>,
}

/// Read the newest records of one incarnation whose payloads fit in
/// `max_bytes` (the newest record is always included). Memory and work
/// stay bounded by `max_bytes + part size limit`, never by the total
/// recording: parts are bounded, the sparse index seeks directly to the
/// tail region, and only that region is re-validated and buffered.
#[cfg(test)]
pub fn read_tail(session_dir: &Path, incarnation: u64, max_bytes: usize) -> io::Result<TailRead> {
    let parts = incarnation_parts(&session_dir.join(JOURNAL_DIR_NAME), incarnation)?;
    // Select parts newest-first: whole parts while they fit the remaining
    // budget, then sparse-seek into the oldest selected part.
    let mut selected: Vec<(PathBuf, Option<ScanStart>)> = Vec::new();
    let mut remaining = max_bytes.max(1) as u64;
    for (index, (_, path)) in parts.iter().enumerate().rev() {
        let len = fs::metadata(path)?.len();
        if len <= remaining {
            selected.push((path.clone(), None));
            remaining -= len;
            if remaining == 0 {
                break;
            }
            continue;
        }
        // Part larger than the remaining budget: sparse-index it and seek
        // straight to the tail region.
        let part_index = scan_segment_index(path)?;
        if index + 1 != parts.len() && !matches!(part_index.stop, ScanStop::CleanEof) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: sealed part is corrupt: {:?}",
                    path.display(),
                    part_index.stop
                ),
            ));
        }
        if let Some((seq, offset)) = part_index.tail_start(remaining) {
            selected.push((path.clone(), Some(ScanStart { offset, seq })));
        }
        break; // budget covered by the seek region
    }

    // Assemble oldest-first, validating sequence continuity across part
    // boundaries. Every scan is bounded: whole parts each fit the budget,
    // the seek region is at most budget + stride + one record.
    let scan_budget = max_bytes
        .saturating_add(SPARSE_INDEX_STRIDE_BYTES as usize)
        .saturating_add(MAX_PAYLOAD_LEN as usize);
    let mut records: Vec<Record> = Vec::new();
    let mut buffered = 0usize;
    let mut expected_first: Option<u64> = None;
    // Selected newest-first above; assemble oldest-first.
    for (path, start) in selected.into_iter().rev() {
        let result = scan_impl(
            &path,
            ScanMode::Window(CollectWindow {
                from_seq: start.map(|start| start.seq).unwrap_or(1),
                to_seq: u64::MAX,
                max_buffered_bytes: scan_budget,
            }),
            if start.is_some() {
                None
            } else {
                expected_first
            },
            start,
        )?;
        if !matches!(result.outcome.stop, ScanStop::CleanEof) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{}: sealed part is corrupt: {:?}",
                    path.display(),
                    result.outcome.stop
                ),
            ));
        }
        expected_first = result.last_seq.map(|seq| seq + 1).or(expected_first);
        buffered += result
            .outcome
            .records
            .iter()
            .map(|record| record.payload.len())
            .sum::<usize>();
        records.extend(result.outcome.records);
    }

    // Trim from the front to fit the budget; the newest record is always
    // kept even if it alone exceeds the budget.
    while records.len() > 1 && buffered > max_bytes {
        buffered -= records[0].payload.len();
        records.remove(0);
    }
    Ok(TailRead { records })
}

/// One event returned by [`read_history`], carrying its durable cursor.
#[derive(Debug)]
#[cfg(test)]
pub struct HistoryEvent {
    pub cursor: JournalCursor,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

/// Result of a cross-incarnation history read.
#[derive(Debug)]
#[cfg(test)]
pub struct HistoryRead {
    pub events: Vec<HistoryEvent>,
    /// Resume cursor (exclusive of the returned events). `None` when no
    /// events were returned — the caller is caught up or `from` is past
    /// the current head.
    pub next: Option<JournalCursor>,
    /// The byte budget cut the read short; resume from `next`.
    pub truncated: bool,
}

/// The one internal read API for live and completed history (PLAN.md §6.1
/// M1 exit): reads events with cursor >= `from` across incarnation
/// segments, oldest first, buffering at most `max_bytes` of payload (the
/// first event is always included).
///
/// Cursors never alias (I3): a restart opens a new incarnation, so a
/// pre-restart cursor keeps addressing the pre-restart bytes. If `from`
/// names an incarnation that retention has removed, the read fails loudly
/// with `NotFound` instead of returning empty or aliased data.
///
/// Corruption inside any consumed incarnation is an `InvalidData` error —
/// history reads never skip past a hole.
#[cfg(test)]
pub fn read_history(
    session_dir: &Path,
    from: JournalCursor,
    max_bytes: usize,
) -> io::Result<HistoryRead> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = list_incarnations(&journal_dir)?;
    if incarnations.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "journal cursor {}:{} names a session with no journal history",
                from.incarnation, from.seq
            ),
        ));
    }
    if !incarnations.contains(&from.incarnation) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "journal cursor {}:{} expired: incarnation no longer retained",
                from.incarnation, from.seq
            ),
        ));
    }
    // A cursor beyond the recovered valid tail of its incarnation must
    // fail loudly (incomplete capture) — never silently slide into the
    // next incarnation's bytes or return empty success.
    if from.seq == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal cursor seq starts at 1",
        ));
    }
    let recovered = recovered_tail(&journal_dir, from.incarnation)?;
    if from.seq > recovered + 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "journal cursor {}:{} is beyond the recovered tail {}:{} — incomplete capture",
                from.incarnation, from.seq, from.incarnation, recovered
            ),
        ));
    }
    // Caught up with the live tail: nothing to read.
    if from.seq == recovered + 1 && from.incarnation == *incarnations.last().expect("nonempty") {
        return Ok(HistoryRead {
            events: Vec::new(),
            next: None,
            truncated: false,
        });
    }

    let mut events = Vec::new();
    let mut buffered = 0usize;
    let mut truncated = false;
    for incarnation in incarnations
        .iter()
        .copied()
        .filter(|incarnation| *incarnation >= from.incarnation)
    {
        if buffered >= max_bytes && !events.is_empty() {
            truncated = true;
            break;
        }
        let from_seq = if incarnation == from.incarnation {
            if from.seq == recovered + 1 {
                continue; // sealed boundary: nothing left in this incarnation
            }
            from.seq
        } else {
            1
        };
        let remaining = max_bytes.saturating_sub(buffered).max(1);
        let read = read_range(session_dir, incarnation, from_seq, u64::MAX, remaining)?;
        if !matches!(read.stop, ScanStop::CleanEof | ScanStop::PartialTail) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "journal incarnation {incarnation} is corrupt: {:?}",
                    read.stop
                ),
            ));
        }
        truncated |= read.truncated;
        buffered += read
            .records
            .iter()
            .map(|record| record.payload.len())
            .sum::<usize>();
        events.extend(read.records.into_iter().map(|record| HistoryEvent {
            cursor: JournalCursor {
                incarnation,
                seq: record.seq,
            },
            kind: record.kind,
            payload: record.payload,
        }));
        if truncated {
            break;
        }
    }

    let next = events.last().map(|event| JournalCursor {
        incarnation: event.cursor.incarnation,
        seq: event.cursor.seq + 1,
    });
    Ok(HistoryRead {
        events,
        next,
        truncated,
    })
}

/// Highest recovered valid sequence of an incarnation (0 = no records).
/// Only the newest part can be torn by a crash, so this scans the newest
/// non-empty part — O(part), never O(history).
#[cfg(test)]
fn recovered_tail(journal_dir: &Path, incarnation: u64) -> io::Result<u64> {
    let parts = incarnation_parts(journal_dir, incarnation)?;
    let last_index = parts.len() - 1;
    for (index, (_, path)) in parts.iter().enumerate().rev() {
        let first = part_first_seq(path)?;
        let stats = scan_segment_stats_from(path, first)?;
        if let Some(last) = stats.last_seq {
            return Ok(last);
        }
        // An empty part is only legal as the newest (crash between
        // rollover and first append); an empty sealed part is corruption.
        if index != last_index {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: empty sealed segment part", path.display()),
            ));
        }
    }
    Ok(0)
}
