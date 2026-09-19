//! Journal-served reads (M3-1b): derive the canonical **filtered display
//! stream** from the raw journal.
//!
//! The journal stores pre-filter PTY bytes (ADR-0002: replay and
//! post-mortems must never lose data the scan pipeline dropped). The
//! filtered stream clients attach to is a deterministic function of those
//! raw bytes — the same `PtyScanner` the reader loop runs, which buffers
//! escape sequences split across records so the concatenated result is
//! chunk-boundary independent. `output.log` therefore duplicates state the
//! journal already owns and is retired in M3-1c; this module is the read
//! path that replaces it.
//!
//! Cost note: replay is checkpoint-anchored (PLAN §5.3): deriving from an
//! arbitrary filtered offset starts at the newest anchored checkpoint at
//! or before that offset, so cost is bounded by the checkpoint cadence
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

/// How far past `from_offset` a resize-history derivation scans. Resizes
/// further ahead cannot affect a bounded render window, so the scan stays
/// bounded even for arbitrarily long-lived sessions (PLAN §5.3).
const MAX_RESIZE_EVENTS_SCAN_BYTES: u64 = 64 * 1024 * 1024;

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

/// Anchored checkpoint offsets of the latest incarnation, ascending.
/// `oly logs` uses these to bound tail replays (PLAN §5.3).
pub fn replay_anchors(session_dir: &Path) -> io::Result<Vec<u64>> {
    let Some(incarnation) = latest_incarnation(session_dir)? else {
        return Ok(Vec::new());
    };
    let mut offsets: Vec<u64> = journal::checkpoint_anchors(session_dir, incarnation)?
        .into_iter()
        .filter(|anchor| anchor.filtered_offset > 0)
        .map(|anchor| anchor.filtered_offset)
        .collect();
    offsets.sort_unstable();
    Ok(offsets)
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

/// Derive the resize history of the latest incarnation by walking the
/// journal: one [`LogResize`] per resize record, where `offset` is the
/// filtered-stream length at the moment the resize was recorded. This is
/// the canonical resize source since M6-2 retired `events.log`; the
/// journal is append-ordered with the output stream, so the offsets are
/// derived, never stored, and cannot disagree with the stream.
pub fn resize_events(session_dir: &Path) -> io::Result<Vec<LogResize>> {
    resize_events_from(session_dir, 0)
}

/// Bounded variant of [`resize_events`]: only resizes at or after
/// `from_offset`, anchored at a checkpoint (PLAN §5.3). Offsets in the
/// result are absolute filtered-stream offsets.
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

/// Bounded variant of [`filtered_stream_from`] (M3-5, I7): returns at most
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
        // from a covered offset never reads it (PLAN §5.3 bound), while a
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
            "a corrupted prefix must fail a full replay (I3)"
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
