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
//! Cost note: deriving from an arbitrary filtered offset replays the raw
//! prefix once (bounded batches, no whole-history buffering). Attach-init
//! calls this once per attach; live clients then follow the broadcast.
//! Cached checkpoints make deep resumes cheap in M3's stream protocol.

use std::io;
use std::path::Path;

use super::journal::{self, JOURNAL_DIR_NAME, RecordKind};
use super::scan::{PtyScanner, ScanOut};

/// Payload bytes of journal records consumed per derivation batch. Bounds
/// memory while replaying; the scanner's concatenation is boundary
/// independent, so batch size never affects the derived stream.
const REPLAY_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// Derive the filtered display stream of the session's latest journal
/// incarnation, starting at filtered-stream offset `from_offset`.
///
/// Returns the filtered bytes from that offset and the filtered-stream end
/// offset (the total filtered length, i.e. what `current_output_offset`
/// reported from `output.log`). A session directory without a journal
/// yields an empty stream.
///
/// Never returns a silent hole: `read_range` validates continuity and
/// integrity of the whole consumed prefix and a torn tail ends the stream
/// exactly where the validated prefix ends (`ScanStop` is exposed for
/// diagnostics, not patched over).
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

    let mut scanner = PtyScanner::new();
    let mut out = ScanOut::default();
    let mut filtered_pos = 0u64;
    let mut collected: Vec<u8> = Vec::new();
    let mut next_seq = 1u64;

    loop {
        let range = journal::read_range(
            session_dir,
            incarnation,
            next_seq,
            u64::MAX,
            REPLAY_BATCH_BYTES,
        )?;
        if range.records.is_empty() {
            break;
        }
        for record in &range.records {
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
            next_seq = record.seq + 1;
        }
        if !range.truncated {
            break;
        }
    }

    Ok((collected, filtered_pos))
}

/// Filtered-stream end offset of the latest incarnation (what
/// `current_output_offset` reported from `output.log`).
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

    #[test]
    fn missing_journal_yields_an_empty_stream() {
        let dir = journal_dir("missing");
        let (bytes, end) = filtered_stream_from(&dir, 0).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(end, 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
