//! Journal-served reads: derive the canonical **filtered display
//! stream** from the raw journal.
//!
//! The journal stores pre-filter PTY bytes (ADR-0002: replay and
//! post-mortems must never lose data the scan pipeline dropped). The
//! filtered stream clients attach to is a deterministic function of those
//! raw bytes — the same `PtyScanner` the reader loop runs, which buffers
//! escape sequences split across records so the concatenated result is
//! chunk-boundary independent. `output.log` therefore duplicates state the
//! journal already owns and is retired; this module is the read path that
//! replaces it.
//!
//! Cost note: replay is checkpoint-anchored: deriving from an arbitrary
//! filtered offset starts at the newest anchored checkpoint at or before
//! that offset, so cost is bounded by the checkpoint cadence
//! (~32 MiB), never by total recording size. Sessions without anchors
//! (pre-checkpoint journals, or a scanner that was mid-escape at every
//! cadence boundary) fall back to a full-prefix scan in bounded batches.

use std::io;
use std::path::Path;

use super::journal::{self, CheckpointAnchor, JOURNAL_DIR_NAME, RecordKind, SegmentStream};
use super::scan::{PtyScanner, ScanOut};
use crate::protocol::LogResize;

/// Payload bytes of journal records consumed per derivation batch. Bounds
/// memory while replaying; the scanner's concatenation is boundary
/// independent, so batch size never affects the derived stream.
const REPLAY_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Maximum filtered-stream lookahead included in resize history for a tail.
/// Resizes further ahead cannot affect a bounded render window.
pub(crate) const MAX_RESIZE_EVENTS_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// Resolve the replay anchor for a target filtered offset: the newest
/// anchored (v2, scanner-idle) checkpoint at or before the target, or no
/// anchor (journal start). The anchor's offset counts the filtered bytes
/// journaled *before* its checkpoint record, so replay resumes on the
/// next record.
fn replay_start(
    session_dir: &Path,
    incarnation: u64,
    target_offset: u64,
) -> Option<CheckpointAnchor> {
    journal::checkpoint_anchors(session_dir, incarnation)
        .unwrap_or_default()
        .into_iter()
        .rfind(|anchor| anchor.filtered_offset > 0 && anchor.filtered_offset <= target_offset)
}

/// Open a replay stream for `incarnation`, checkpoint-anchored for
/// `target_offset` when an anchor covers it, and return it with the
/// filtered offset the stream starts at.
fn replay_stream(
    session_dir: &Path,
    incarnation: u64,
    target_offset: u64,
) -> io::Result<(SegmentStream, u64)> {
    match replay_start(session_dir, incarnation, target_offset) {
        Some(anchor) => Ok((
            SegmentStream::open_at(session_dir, incarnation, &anchor)?,
            anchor.filtered_offset,
        )),
        None => Ok((SegmentStream::open(session_dir, incarnation)?, 0)),
    }
}

/// Anchors of the latest incarnation, ascending by filtered offset.
/// `oly logs` reuses these records to avoid rescanning the journal for each
/// candidate tail start.
pub(crate) fn replay_anchors(session_dir: &Path) -> io::Result<Vec<CheckpointAnchor>> {
    let Some(incarnation) = latest_incarnation(session_dir)? else {
        return Ok(Vec::new());
    };
    let mut anchors: Vec<_> = journal::checkpoint_anchors(session_dir, incarnation)?
        .into_iter()
        .filter(|anchor| anchor.filtered_offset > 0)
        .collect();
    // Stable ordering preserves journal order for duplicate offsets, so the
    // latest checkpoint at an equal offset is considered first by callers.
    anchors.sort_by_key(|anchor| anchor.filtered_offset);
    Ok(anchors)
}

/// Latest journal incarnation for a session directory, if any.
fn latest_incarnation(session_dir: &Path) -> io::Result<Option<u64>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    match journal::list_incarnations(&journal_dir) {
        Ok(incarnations) => Ok(incarnations.last().copied()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Lazy `io::Read` over the derived filtered display stream of the latest
/// journal incarnation — the streaming replacement for opening
/// `output.log`. Pulls bounded record batches, filters them through the
/// scanner, and never buffers more than one batch.
pub struct ReplayReader {
    stream: Option<SegmentStream>,
    scanner: PtyScanner,
    pending: std::collections::VecDeque<u8>,
    done: bool,
}

impl ReplayReader {
    pub fn new(session_dir: &Path) -> io::Result<Self> {
        let stream = match latest_incarnation(session_dir)? {
            Some(incarnation) => Some(SegmentStream::open(session_dir, incarnation)?),
            None => None,
        };
        Ok(Self {
            stream,
            scanner: PtyScanner::new(),
            pending: std::collections::VecDeque::new(),
            done: false,
        })
    }

    fn fill(&mut self) -> io::Result<()> {
        if self.done || !self.pending.is_empty() {
            return Ok(());
        }
        let Some(stream) = &mut self.stream else {
            self.done = true;
            return Ok(());
        };
        // `SegmentStream` resumes at a byte position, so successive fills
        // never rescan a validated prefix.
        let records = stream.next_batch(REPLAY_BATCH_BYTES)?;
        if records.is_empty() {
            self.done = true;
            return Ok(());
        }
        let mut out = ScanOut::default();
        for record in &records {
            if record.kind == RecordKind::Output {
                self.scanner.scan(&record.payload, &mut out);
                self.pending.extend(out.filtered.iter().copied());
            }
        }
        Ok(())
    }
}

impl io::Read for ReplayReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            self.fill()?;
        }
        let n = self.pending.len().min(buf.len());
        for slot in &mut buf[..n] {
            *slot = self.pending.pop_front().expect("len checked");
        }
        Ok(n)
    }
}

/// Derive the filtered display stream of the session's latest journal
/// incarnation, starting at filtered-stream offset `from_offset`.
///
/// Returns the filtered bytes from that offset and the filtered-stream end
/// offset (the total filtered length, i.e. what `current_output_offset`
/// reported from `output.log`). A session directory without a journal
/// yields an empty stream.
///
/// Never returns a silent hole: the underlying [`SegmentStream`] validates
/// continuity and integrity from the resume position onward, and a torn
/// tail ends the stream exactly where the validated prefix ends.
pub fn filtered_stream_from(session_dir: &Path, from_offset: u64) -> io::Result<(Vec<u8>, u64)> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = match journal::list_incarnations(&journal_dir) {
        Ok(incarnations) => incarnations,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(err) => return Err(err),
    };
    let Some(&incarnation) = incarnations.last() else {
        return Ok((Vec::new(), 0));
    };

    let (mut stream, mut filtered_pos) = replay_stream(session_dir, incarnation, from_offset)?;
    let mut scanner = PtyScanner::new();
    let mut out = ScanOut::default();
    let mut collected: Vec<u8> = Vec::new();

    loop {
        let records = stream.next_batch(REPLAY_BATCH_BYTES)?;
        if records.is_empty() {
            break;
        }
        for record in &records {
            if record.kind == RecordKind::Output {
                scanner.scan(&record.payload, &mut out);
                let batch = &out.filtered;
                let batch_start = filtered_pos;
                filtered_pos = filtered_pos.saturating_add(batch.len() as u64);
                if filtered_pos > from_offset {
                    let skip = from_offset.saturating_sub(batch_start) as usize;
                    collected.extend_from_slice(&batch[skip.min(batch.len())..]);
                }
            }
        }
    }

    Ok((collected, filtered_pos))
}

/// Derive the filtered display stream **and** the resize history of the
/// latest incarnation in a single journal pass (`oly logs` reads both;
/// deriving them together halves the replay cost versus
/// [`filtered_stream_from`] + [`resize_events_from`]).
///
/// Resize offsets are absolute filtered-stream offsets, exactly like
/// [`resize_events_from`]'s. The caller can trim the returned history to
/// the bounded resize window relevant to its tail, matching
/// [`resize_events_from`]'s 64 MiB forward-scan policy.
#[cfg(test)]
pub fn filtered_stream_and_resizes_from(
    session_dir: &Path,
    from_offset: u64,
) -> io::Result<(Vec<u8>, u64, Vec<LogResize>)> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = match journal::list_incarnations(&journal_dir) {
        Ok(incarnations) => incarnations,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), 0, Vec::new()));
        }
        Err(err) => return Err(err),
    };
    let Some(&incarnation) = incarnations.last() else {
        return Ok((Vec::new(), 0, Vec::new()));
    };

    let (stream, filtered_pos) = replay_stream(session_dir, incarnation, from_offset)?;
    derive_stream_and_resizes(stream, filtered_pos, from_offset)
}

/// Derive a stream from an already-discovered checkpoint anchor. The anchor
/// is reused only while it still belongs to the latest incarnation; if the
/// session restarted after discovery, replay safely falls back to its start.
pub(crate) fn filtered_stream_and_resizes_from_anchor(
    session_dir: &Path,
    from_offset: u64,
    anchor: Option<&CheckpointAnchor>,
) -> io::Result<(Vec<u8>, u64, Vec<LogResize>, u64)> {
    let Some(incarnation) = latest_incarnation(session_dir)? else {
        return Ok((Vec::new(), 0, Vec::new(), 0));
    };
    let stale_anchor = anchor.is_some_and(|anchor| anchor.cursor.incarnation != incarnation);
    let from_offset = if stale_anchor { 0 } else { from_offset };
    let anchor = anchor.filter(|anchor| anchor.cursor.incarnation == incarnation);
    let (stream, filtered_pos) = match anchor {
        Some(anchor) => (
            SegmentStream::open_at(session_dir, incarnation, anchor)?,
            anchor.filtered_offset,
        ),
        None => (SegmentStream::open(session_dir, incarnation)?, 0),
    };
    let (bytes, end, resizes) = derive_stream_and_resizes(stream, filtered_pos, from_offset)?;
    Ok((bytes, end, resizes, from_offset.max(filtered_pos)))
}

fn derive_stream_and_resizes(
    mut stream: SegmentStream,
    mut filtered_pos: u64,
    from_offset: u64,
) -> io::Result<(Vec<u8>, u64, Vec<LogResize>)> {
    let mut scanner = PtyScanner::new();
    let mut out = ScanOut::default();
    let mut collected: Vec<u8> = Vec::new();
    let mut resizes = Vec::new();

    loop {
        let records = stream.next_batch(REPLAY_BATCH_BYTES)?;
        if records.is_empty() {
            break;
        }
        for record in &records {
            match record.kind {
                RecordKind::Output => {
                    scanner.scan(&record.payload, &mut out);
                    let batch = &out.filtered;
                    let batch_start = filtered_pos;
                    filtered_pos = filtered_pos.saturating_add(batch.len() as u64);
                    if filtered_pos > from_offset {
                        let skip = from_offset.saturating_sub(batch_start) as usize;
                        collected.extend_from_slice(&batch[skip.min(batch.len())..]);
                    }
                }
                RecordKind::Resize => {
                    if let Some((rows, cols)) = journal::decode_resize_payload(&record.payload) {
                        resizes.push(LogResize {
                            offset: filtered_pos,
                            rows,
                            cols,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    Ok((collected, filtered_pos, resizes))
}

/// Derive the resize history of the latest incarnation by walking the
/// journal: one [`LogResize`] per resize record, where `offset` is the
/// filtered-stream length at the moment the resize was recorded. This is
/// the canonical resize source since `events.log` was retired; the
/// journal is append-ordered with the output stream, so the offsets are
/// derived, never stored, and cannot disagree with the stream.
pub fn resize_events(session_dir: &Path) -> io::Result<Vec<LogResize>> {
    resize_events_from(session_dir, 0)
}

/// Bounded variant of [`resize_events`]: only resizes at or after
/// `from_offset`, anchored at a checkpoint. Offsets in the result are
/// absolute filtered-stream offsets.
pub fn resize_events_from(session_dir: &Path, from_offset: u64) -> io::Result<Vec<LogResize>> {
    let Some(incarnation) = latest_incarnation(session_dir)? else {
        return Ok(Vec::new());
    };

    let (mut stream, mut filtered_pos) = replay_stream(session_dir, incarnation, from_offset)?;
    let mut scanner = PtyScanner::new();
    let mut out = ScanOut::default();
    let mut events = Vec::new();

    loop {
        let records = stream.next_batch(REPLAY_BATCH_BYTES)?;
        if records.is_empty() {
            break;
        }
        for record in &records {
            match record.kind {
                RecordKind::Output => {
                    scanner.scan(&record.payload, &mut out);
                    filtered_pos = filtered_pos.saturating_add(out.filtered.len() as u64);
                }
                RecordKind::Resize => {
                    if let Some((rows, cols)) = journal::decode_resize_payload(&record.payload) {
                        events.push(LogResize {
                            offset: filtered_pos,
                            rows,
                            cols,
                        });
                    }
                }
                _ => {}
            }
        }
        if filtered_pos > from_offset + MAX_RESIZE_EVENTS_SCAN_BYTES {
            // No useful resize history this far ahead of the window.
            break;
        }
    }

    Ok(events)
}

/// Bounded variant of [`filtered_stream_from`]: returns at most
/// `max_bytes` of the filtered display stream starting at `from_offset`.
/// Attach pumps use this to resync a lagged client in bounded windows
/// instead of materializing the whole lag in memory at once; a short
/// result means the persisted stream is exhausted at the moment of the
/// read.
pub fn filtered_stream_window(
    session_dir: &Path,
    from_offset: u64,
    max_bytes: usize,
) -> io::Result<Vec<u8>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = match journal::list_incarnations(&journal_dir) {
        Ok(incarnations) => incarnations,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let Some(&incarnation) = incarnations.last() else {
        return Ok(Vec::new());
    };

    let (mut stream, mut filtered_pos) = replay_stream(session_dir, incarnation, from_offset)?;
    let mut scanner = PtyScanner::new();
    let mut out = ScanOut::default();
    let mut collected: Vec<u8> = Vec::new();

    loop {
        let records = stream.next_batch(REPLAY_BATCH_BYTES)?;
        if records.is_empty() {
            break;
        }
        for record in &records {
            if record.kind == RecordKind::Output {
                scanner.scan(&record.payload, &mut out);
                let batch = &out.filtered;
                let batch_start = filtered_pos;
                filtered_pos = filtered_pos.saturating_add(batch.len() as u64);
                if filtered_pos > from_offset && collected.len() < max_bytes {
                    let skip = from_offset.saturating_sub(batch_start) as usize;
                    let take = batch.len().saturating_sub(skip.min(batch.len()));
                    let take = take.min(max_bytes - collected.len());
                    collected.extend_from_slice(&batch[skip.min(batch.len())..][..take]);
                }
            }
        }
        if collected.len() >= max_bytes {
            break;
        }
    }

    Ok(collected)
}

/// Filtered-stream end offset of the latest incarnation (the offset space
/// attach cursors and resume tokens refer to).
pub fn filtered_stream_len(session_dir: &Path) -> io::Result<u64> {
    Ok(filtered_stream_from(session_dir, u64::MAX)?.1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal::ShadowJournal;

    /// Flush all queued records to disk (bounded wait, deterministic).
    fn flush(journal: &mut ShadowJournal, expected_seq: u64) {
        journal.request_sync();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while journal.core.durable_seq() < expected_seq {
            journal.poll_acks();
            assert!(
                std::time::Instant::now() < deadline,
                "journal did not reach durable seq {expected_seq}"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn journal_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oly-replay-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn thousands_of_lines_page_exactly_across_utf8_and_controls() {
        let dir = journal_dir("many-pages");
        let canonical = (1..=2100)
            .map(|n| format!("\x1b[31m{n}:你好\x1b[0m\r\n"))
            .collect::<String>()
            .into_bytes();
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        for part in canonical.chunks(2048) {
            journal
                .record_output(bytes::Bytes::copy_from_slice(part))
                .unwrap();
        }
        journal.shutdown();
        let mut delivered = Vec::new();
        while delivered.len() < canonical.len() {
            let page = filtered_stream_window(&dir, delivered.len() as u64, 257).unwrap();
            assert!(!page.is_empty());
            assert!(page.len() <= 257);
            delivered.extend_from_slice(&page);
        }
        assert_eq!(delivered, canonical);
        assert!(
            filtered_stream_window(&dir, canonical.len() as u64, 257)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn filtered_stream_strips_queries_like_the_reader_did() {
        let dir = journal_dir("strip");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            // Raw bytes interleave display text with a DSR probe; the
            // filtered stream must match what the reader loop would have
            // persisted to output.log.
            journal
                .record_output(bytes::Bytes::from_static(b"hello \x1b[6nworld"))
                .unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"!\x1b[?25l"))
                .unwrap();
            flush(&mut journal, 2);
        }

        let (bytes, end) = filtered_stream_from(&dir, 0).unwrap();
        assert_eq!(end as usize, bytes.len());
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("hello "), "{text:?}");
        assert!(text.contains("world!"), "{text:?}");
        assert!(
            !text.contains("\x1b[6n"),
            "the DSR query is stripped exactly like the reader did: {text:?}"
        );
        // Display-relevant sequences (cursor visibility) pass through, as
        // they did into output.log.
        assert!(text.contains("\x1b[?25l"), "{text:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filtered_stream_window_bounds_the_returned_bytes() {
        let dir = journal_dir("window");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            for chunk in [b"alpha".as_slice(), b"beta".as_slice(), b"gamma".as_slice()] {
                journal
                    .record_output(bytes::Bytes::copy_from_slice(chunk))
                    .unwrap();
            }
            flush(&mut journal, 3);
        }
        // Full stream: "alphabetagamma" (14 bytes).
        let window = filtered_stream_window(&dir, 0, 8).unwrap();
        assert_eq!(window, b"alphabet", "bounded at max_bytes");
        let window = filtered_stream_window(&dir, 8, 8).unwrap();
        assert_eq!(
            window, b"agamma",
            "continues exactly where the last window ended"
        );
        let window = filtered_stream_window(&dir, 14, 8).unwrap();
        assert!(
            window.is_empty(),
            "short/empty window = persisted end reached"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filtered_stream_resumes_from_a_mid_stream_offset() {
        let dir = journal_dir("offset");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            for chunk in [&b"aaaa"[..], b"bbbb", b"cccc", b"dddd"] {
                journal
                    .record_output(bytes::Bytes::from_static(chunk))
                    .unwrap();
            }
            flush(&mut journal, 4);
        }

        let (all, end) = filtered_stream_from(&dir, 0).unwrap();
        assert_eq!(end, 16);
        assert_eq!(all, b"aaaabbbbccccdddd");
        let (tail, end2) = filtered_stream_from(&dir, 6).unwrap();
        assert_eq!(end2, 16);
        assert_eq!(tail, b"bbccccdddd");
        // Beyond the end yields an empty stream at the same end offset.
        let (empty, end3) = filtered_stream_from(&dir, 99).unwrap();
        assert_eq!(end3, 16);
        assert!(empty.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn combined_stream_and_resize_replay_matches_separate_derivations() {
        let dir = journal_dir("combined");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"abc"))
                .unwrap();
            journal.record_resize(24, 80).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"defgh"))
                .unwrap();
            journal.record_resize(30, 100).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"ij"))
                .unwrap();
            flush(&mut journal, 5);
        }

        let (bytes, end, resizes) = filtered_stream_and_resizes_from(&dir, 0).unwrap();
        assert_eq!(bytes, b"abcdefghij");
        assert_eq!(end, 10);
        assert_eq!(
            resizes,
            vec![
                LogResize {
                    offset: 3,
                    rows: 24,
                    cols: 80,
                },
                LogResize {
                    offset: 8,
                    rows: 30,
                    cols: 100,
                },
            ]
        );

        let (tail, end, tail_resizes) = filtered_stream_and_resizes_from(&dir, 4).unwrap();
        assert_eq!(tail, b"efghij");
        assert_eq!(end, 10);
        assert_eq!(tail_resizes, resize_events_from(&dir, 4).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn combined_replay_preserves_resize_history_across_multiple_anchors() {
        let dir = journal_dir("combined_anchors");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"aa"))
                .unwrap();
            journal.record_resize(24, 80).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"bb"))
                .unwrap();
            journal
                .record_checkpoint(&journal::Checkpoint {
                    rows: 24,
                    cols: 80,
                    cursor: (1, 1),
                    alt_screen: false,
                    app_cursor_keys: false,
                    bracketed_paste: false,
                    filtered_offset: 4,
                    program: bytes::Bytes::from_static(b"restore"),
                })
                .unwrap();
            journal.record_resize(30, 100).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"cc"))
                .unwrap();
            journal.record_resize(40, 120).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"dd"))
                .unwrap();
            journal
                .record_checkpoint(&journal::Checkpoint {
                    rows: 40,
                    cols: 120,
                    cursor: (1, 1),
                    alt_screen: false,
                    app_cursor_keys: false,
                    bracketed_paste: false,
                    filtered_offset: 8,
                    program: bytes::Bytes::from_static(b"restore"),
                })
                .unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"ee"))
                .unwrap();
            journal.record_resize(50, 140).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"ff"))
                .unwrap();
            flush(&mut journal, 12);
        }

        let anchors = replay_anchors(&dir).unwrap();
        assert_eq!(
            anchors
                .iter()
                .map(|anchor| anchor.filtered_offset)
                .collect::<Vec<_>>(),
            vec![4, 8]
        );
        for anchor in &anchors {
            let offset = anchor.filtered_offset;
            let (bytes, end, resizes, replay_start) =
                filtered_stream_and_resizes_from_anchor(&dir, offset, Some(anchor)).unwrap();
            assert_eq!(replay_start, offset);
            assert_eq!(end, 12);
            assert_eq!(bytes, b"aabbccddeeff"[offset as usize..]);
            assert_eq!(resizes, resize_events_from(&dir, offset).unwrap());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn replay_anchors_keep_journal_order_for_duplicate_offsets() {
        let dir = journal_dir("duplicate_anchor_offsets");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"prefix"))
                .unwrap();
            for _ in 0..2 {
                journal
                    .record_checkpoint(&journal::Checkpoint {
                        rows: 24,
                        cols: 80,
                        cursor: (1, 1),
                        alt_screen: false,
                        app_cursor_keys: false,
                        bracketed_paste: false,
                        filtered_offset: 6,
                        program: bytes::Bytes::from_static(b"restore"),
                    })
                    .unwrap();
            }
            journal
                .record_output(bytes::Bytes::from_static(b"tail"))
                .unwrap();
            flush(&mut journal, 4);
        }

        let anchors = replay_anchors(&dir).unwrap();
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0].filtered_offset, 6);
        assert_eq!(anchors[1].filtered_offset, 6);
        assert!(anchors[0].cursor.seq < anchors[1].cursor.seq);

        let (bytes, end, _, start) =
            filtered_stream_and_resizes_from_anchor(&dir, 6, Some(&anchors[1])).unwrap();
        assert_eq!(bytes, b"tail");
        assert_eq!(end, 10);
        assert_eq!(start, 6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_replay_anchor_matches_discovered_anchor_and_ignores_stale_incarnations() {
        let dir = journaled_stream_with_checkpoints(
            "explicit_anchor",
            &[b"aaaa", b"bbbb", b"cccc"],
            &[(2, 8)],
        );
        let anchors = replay_anchors(&dir).unwrap();
        assert_eq!(anchors.len(), 1);
        let (expected, expected_end, expected_resizes) =
            filtered_stream_and_resizes_from(&dir, 8).unwrap();
        let (actual, actual_end, actual_resizes, actual_start) =
            filtered_stream_and_resizes_from_anchor(&dir, 8, Some(&anchors[0])).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual_end, expected_end);
        assert_eq!(actual_resizes, expected_resizes);
        assert_eq!(actual_start, 8);

        // Reopening creates a new incarnation; a stale anchor must never be
        // used to skip the new incarnation's prefix.
        {
            let (mut journal, incarnation, _) = ShadowJournal::open(&dir).unwrap();
            assert_eq!(incarnation, 2);
            journal
                .record_output(bytes::Bytes::from_static(b"fresh"))
                .unwrap();
            flush(&mut journal, 1);
        }
        let (fresh, end, _, fresh_start) =
            filtered_stream_and_resizes_from_anchor(&dir, 8, Some(&anchors[0])).unwrap();
        assert_eq!(fresh, b"fresh");
        assert_eq!(end, 5);
        assert_eq!(fresh_start, 0, "stale offset must be reset on restart");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filtered_stream_uses_only_the_latest_incarnation() {
        let dir = journal_dir("restart");
        {
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            journal
                .record_output(bytes::Bytes::from_static(b"first-run"))
                .unwrap();
            flush(&mut journal, 1);
        }
        {
            // A restart opens a new incarnation; its output is a fresh
            // stream (output.log was truncated on restart too).
            let (mut journal, incarnation, _) = ShadowJournal::open(&dir).unwrap();
            assert_eq!(incarnation, 2);
            journal
                .record_output(bytes::Bytes::from_static(b"second-run"))
                .unwrap();
            flush(&mut journal, 1);
        }

        let (bytes, end) = filtered_stream_from(&dir, 0).unwrap();
        assert_eq!(bytes, b"second-run");
        assert_eq!(end, 10);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Write `outputs`, placing an anchored checkpoint after each
    /// `checkpoint_after` record count, then flush.
    fn journaled_stream_with_checkpoints(
        tag: &str,
        outputs: &[&[u8]],
        checkpoint_after: &[(usize, u64)],
    ) -> std::path::PathBuf {
        let dir = journal_dir(tag);
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        let mut checkpoints = checkpoint_after.iter().peekable();
        for (index, chunk) in outputs.iter().enumerate() {
            journal
                .record_output(bytes::Bytes::copy_from_slice(chunk))
                .unwrap();
            if let Some(&&(after, offset)) = checkpoints.peek()
                && after == index + 1
            {
                journal
                    .record_checkpoint(&journal::Checkpoint {
                        rows: 24,
                        cols: 80,
                        cursor: (1, 1),
                        alt_screen: false,
                        app_cursor_keys: false,
                        bracketed_paste: false,
                        filtered_offset: offset,
                        program: bytes::Bytes::from_static(b"\x1b[2Jrepaint"),
                    })
                    .unwrap();
                checkpoints.next();
            }
        }
        flush(
            &mut journal,
            outputs.len() as u64 + checkpoint_after.len() as u64,
        );
        dir
    }

    #[test]
    fn anchored_replay_matches_full_replay_for_any_offset() {
        let dir = journaled_stream_with_checkpoints(
            "anchored",
            &[b"aaaa", b"bbbb", b"cccc", b"dddd", b"eeee"],
            &[(2, 8), (4, 16)],
        );

        let (full, end) = filtered_stream_from(&dir, 0).unwrap();
        assert_eq!(end, 20);
        for offset in [0, 1, 7, 8, 9, 15, 16, 17, 19, 20, 25] {
            let (anchored, anchored_end) = filtered_stream_from(&dir, offset).unwrap();
            assert_eq!(anchored_end, end, "end offset at {offset}");
            let expected = &full[(offset.min(end)) as usize..];
            assert_eq!(anchored, expected, "anchored replay diverges at {offset}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn anchored_windows_match_full_stream_slices() {
        let dir = journaled_stream_with_checkpoints(
            "anchored_window",
            &[b"aaaa", b"bbbb", b"cccc", b"dddd", b"eeee"],
            &[(2, 8), (4, 16)],
        );

        let (full, _) = filtered_stream_from(&dir, 0).unwrap();
        for offset in [0u64, 8, 9, 16, 17] {
            let window = filtered_stream_window(&dir, offset, 3).unwrap();
            let expected = &full[offset as usize..(offset as usize + 3).min(full.len())];
            assert_eq!(window, expected, "anchored window diverges at {offset}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn anchored_replay_skips_records_before_the_anchor() {
        let dir = journaled_stream_with_checkpoints(
            "anchored_skip",
            &[b"aaaa", b"bbbb", b"cccc", b"dddd"],
            &[(2, 8)],
        );

        // Corrupt a pre-anchor record's payload in place: anchored replay
        // from a covered offset never reads it (anchored bound), while a
        // full-prefix replay must fail integrity validation.
        let journal_dir = dir.join(journal::JOURNAL_DIR_NAME);
        let segment = std::fs::read_dir(&journal_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "ojrn"))
            .expect("segment file");
        let mut bytes = std::fs::read(&segment).unwrap();
        // Header is 36 bytes; first payload byte sits at offset 36.
        bytes[36] ^= 0xFF;
        std::fs::write(&segment, bytes).unwrap();

        let (tail, end) = filtered_stream_from(&dir, 12).unwrap();
        assert_eq!(end, 16);
        assert_eq!(tail, b"dddd");
        assert!(
            filtered_stream_from(&dir, 0).is_err(),
            "a corrupted prefix must fail a full replay — partial replay
             is not honest reporting"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_journal_yields_an_empty_stream() {
        let dir = journal_dir("missing");
        let (bytes, end) = filtered_stream_from(&dir, 0).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(end, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
