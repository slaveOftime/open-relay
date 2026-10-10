//! Derived screen observations, sampled after complete journal Output records.
//! One terminal preserves parser/main/alternate-buffer state. Cold replay is
//! linear; warm reads resume the validated journal record position. The cache
//! is rebuildable, incarnation-keyed, and bounded; never a source of truth.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::protocol::StreamPosition;
use crate::session::journal::{
    self, RecordKind, SegmentStream, decode_resize_payload, list_incarnations,
};
use crate::session::scan::{PtyScanner, ScanOut};
use crate::terminal::Terminal;

const MAX_CAPTURES: usize = 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const HISTORY_BATCH_BYTES: usize = 256 * 1024;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

#[derive(Debug, Clone)]
pub struct ScreenSnapshot {
    /// Exclusive filtered-byte observation boundary (not a delivery claim).
    pub position: StreamPosition,
    pub rows: u16,
    pub cols: u16,
    pub rows_bytes: Vec<Vec<u8>>,
}

pub struct ScreenHistory {
    pub snapshots: Vec<ScreenSnapshot>,
    pub end_position: StreamPosition,
    pub text_width: u16,
}

struct Replay {
    stream: SegmentStream,
    scanner: PtyScanner,
    engine: Terminal,
    position: StreamPosition,
    captures: VecDeque<ScreenSnapshot>,
    last_hash: Option<[u8; 32]>,
    retained_bytes: usize,
    capacity: usize,
    tail: bool,
    // Lifetime input is a conservative cache-admission charge, NOT a
    // retained-memory measurement or a read limit. Ordinary Unicode and
    // repeated titles can make a small terminal exceed it; serve uncached.
    input_bytes: usize,
    peak_cols: usize,
    peak_rows: usize,
}

impl Replay {
    fn new(dir: &Path, incarnation: u64, capacity: usize, tail: bool) -> io::Result<Self> {
        Ok(Self {
            stream: SegmentStream::open(dir, incarnation)?,
            scanner: PtyScanner::new(),
            engine: Terminal::new(DEFAULT_ROWS, DEFAULT_COLS, if tail { capacity } else { 0 }),
            position: StreamPosition {
                incarnation,
                offset: 0,
            },
            captures: VecDeque::new(),
            last_hash: None,
            retained_bytes: 0,
            capacity,
            tail,
            input_bytes: 0,
            peak_cols: DEFAULT_COLS as usize,
            peak_rows: DEFAULT_ROWS as usize,
        })
    }

    fn update(&mut self, dir: &Path) -> io::Result<()> {
        self.stream.refresh(dir, self.position.incarnation)?;
        let mut out = ScanOut::default();
        loop {
            let records = self.stream.next_batch(HISTORY_BATCH_BYTES)?;
            if records.is_empty() {
                break;
            }
            for record in records {
                match record.kind {
                    RecordKind::Resize => {
                        if let Some((rows, cols)) = decode_resize_payload(&record.payload) {
                            if self.tail
                                && (rows as usize * 2 + self.capacity) * cols as usize > 8_000_000
                            {
                                return Err(io::Error::other(
                                    "tail viewport/history exceeds eight million cells; request fewer lines",
                                ));
                            }
                            if rows as usize * cols as usize > 1_000_000 {
                                return Err(io::Error::other(
                                    "recorded viewport exceeds one million cells",
                                ));
                            }
                            self.peak_rows = self.peak_rows.max(rows.max(1) as usize);
                            self.peak_cols = self.peak_cols.max(cols.max(1) as usize);
                            self.engine.resize(rows.max(1), cols.max(1));
                            let _ = self.engine.drain_events();
                        }
                    }
                    RecordKind::Output => {
                        self.scanner.scan(&record.payload, &mut out);
                        if out.filtered.is_empty() {
                            continue;
                        }
                        // Conservatively charge non-ASCII/OSC input for cache
                        // admission only. Never reject a recording based on
                        // lifetime throughput: most of these bytes are not retained.
                        let extra = if record.payload.windows(2).any(|b| b == b"\x1b]") {
                            record.payload.len()
                        } else {
                            record.payload.iter().filter(|b| **b >= 128).count()
                        };
                        self.input_bytes = self.input_bytes.saturating_add(extra);
                        self.engine.feed(&out.filtered);
                        self.position.offset += out.filtered.len() as u64;
                        out = ScanOut::default();
                        let _ = self.engine.drain_events();
                        if !self.tail {
                            self.capture();
                        }
                        if self.retained_bytes > CACHE_BYTES {
                            return Err(io::Error::other(
                                "frame history exceeds 64 MiB; request fewer frames",
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn capture(&mut self) {
        let (rows, cols) = self.engine.size();
        let hash = self.engine.screen_digest();
        if self.last_hash == Some(hash) {
            return;
        }
        self.last_hash = Some(hash);
        let frame = ScreenSnapshot {
            position: self.position,
            rows,
            cols,
            rows_bytes: self.engine.styled_screen_rows(),
        };
        self.retained_bytes += frame_bytes(&frame);
        self.captures.push_back(frame);
        // Explicit failure instead of silently dropping requested history.
        // Grid allocation itself is bounded by validated recorded geometry.
        while self.captures.len() > self.capacity {
            self.retained_bytes -= frame_bytes(&self.captures.pop_front().unwrap());
        }
    }

    fn memory_bytes(&self) -> usize {
        // Charge historical maximum geometry, both buffers, maximum retained
        // scrollback, and allocator slack. Shrinking may retain capacity.
        self.retained_bytes
            .saturating_add(
                (self.peak_rows * 2 + if self.tail { self.capacity } else { 0 })
                    * self.peak_cols
                    * 256,
            )
            .saturating_add(self.scanner.memory_bytes())
            .saturating_add(self.input_bytes.saturating_mul(32))
    }

    fn result(&self, from: u64, count: usize, keep_color: bool) -> ScreenHistory {
        let mut snapshots: Vec<_> = self
            .captures
            .iter()
            .rev()
            .filter(|s| s.position.offset > from)
            .take(count)
            .cloned()
            .collect();
        snapshots.reverse();
        if !keep_color {
            for frame in &mut snapshots {
                for row in &mut frame.rows_bytes {
                    *row = strip_sgr(row);
                }
            }
        }
        ScreenHistory {
            snapshots,
            end_position: self.position,
            text_width: self.engine.size().1,
        }
    }
}

fn frame_bytes(frame: &ScreenSnapshot) -> usize {
    std::mem::size_of::<ScreenSnapshot>()
        + frame.rows_bytes.capacity() * std::mem::size_of::<Vec<u8>>()
        + frame.rows_bytes.iter().map(|r| r.capacity()).sum::<usize>()
}

/// Styled engine rows contain SGR only, not arbitrary terminal controls.
fn strip_sgr(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"\x1b[") {
            i += 2;
            while i < bytes.len() && bytes[i] != b'm' {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
        } else {
            result.push(bytes[i]);
            i += 1;
        }
    }
    result
}

struct Entry {
    dir: PathBuf,
    incarnation: u64,
    fingerprint: Vec<(PathBuf, u64, u64)>,
    replay: Replay,
}
#[derive(Default)]
struct Cache {
    entries: VecDeque<Entry>,
    // Weak gates coalesce a key even when its replay is too large to retain.
    // Neither metadata nor replay work runs under this registry lock.
    gates: Vec<(PathBuf, bool, Weak<Mutex<()>>)>,
}
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn key_gate(dir: &Path, tail: bool) -> io::Result<Arc<Mutex<()>>> {
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(Cache::default()))
        .lock()
        .map_err(|_| io::Error::other("log cache poisoned"))?;
    cache.gates.retain(|(_, _, gate)| gate.strong_count() > 0);
    if let Some(gate) = cache.gates.iter().find_map(|(path, mode, gate)| {
        (path == dir && *mode == tail)
            .then(|| gate.upgrade())
            .flatten()
    }) {
        return Ok(gate);
    }
    let gate = Arc::new(Mutex::new(()));
    cache.gates.push((dir.into(), tail, Arc::downgrade(&gate)));
    Ok(gate)
}

fn fingerprint(dir: &Path, incarnation: u64) -> io::Result<Vec<(PathBuf, u64, u64)>> {
    let parts = journal::incarnation_parts(&dir.join(journal::JOURNAL_DIR_NAME), incarnation)?;
    parts
        .into_iter()
        .map(|(_, path)| {
            let metadata = std::fs::metadata(&path)?;
            #[cfg(unix)]
            let identity = {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            };
            #[cfg(not(unix))]
            let identity = metadata
                .created()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            Ok((path, metadata.len(), identity))
        })
        .collect()
}

/// Up to 1024 consecutive distinct observations, oldest first. Byte boundaries
/// are exclusive. Zero is an empty, non-advancing read, without journal replay.
/// Call only on a blocking worker; per-key gates coalesce concurrent rebuilds.
pub fn collect(dir: &Path, from: u64, count: usize, keep_color: bool) -> io::Result<ScreenHistory> {
    collect_impl(dir, from, count, keep_color, false, false)
}

/// Current grid includes resize-only changes, unlike output-event samples.
pub fn current(dir: &Path, keep_color: bool) -> io::Result<ScreenHistory> {
    collect_impl(dir, 0, 1, keep_color, true, false)
}

/// Render scrollback plus the visible main/alternate grid using continuous
/// recorded terminal state. Retain N history rows, plus the visible viewport,
/// so a final empty cursor row never steals the Nth requested line.
/// Cold reads validate the origin; warm reads feed only appended records.
pub fn tail(dir: &Path, count: usize, keep_color: bool) -> io::Result<(Vec<u8>, u64)> {
    let history = collect_impl(dir, 0, count, keep_color, false, true)?;
    let output = history
        .snapshots
        .into_iter()
        .next()
        .map(|s| crate::session::logs::finish_render(s.rows_bytes, count, keep_color))
        .unwrap_or_default();
    Ok((output, history.end_position.offset))
}

fn collect_impl(
    dir: &Path,
    from: u64,
    count: usize,
    keep_color: bool,
    current_screen: bool,
    tail: bool,
) -> io::Result<ScreenHistory> {
    if count > if tail { 65535 } else { MAX_CAPTURES } {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log count exceeds supported range",
        ));
    }
    let gate = key_gate(dir, tail)?;
    let _guard = gate
        .lock()
        .map_err(|_| io::Error::other("log replay poisoned"))?;
    let incarnation = list_incarnations(&dir.join(journal::JOURNAL_DIR_NAME))?
        .last()
        .copied()
        .unwrap_or(0);
    if count == 0 || incarnation == 0 {
        return Ok(ScreenHistory {
            snapshots: vec![],
            end_position: StreamPosition {
                incarnation,
                offset: from,
            },
            text_width: DEFAULT_COLS,
        });
    }
    let current = fingerprint(dir, incarnation)?;
    let existing = {
        let mut cache = CACHE
            .get()
            .unwrap()
            .lock()
            .map_err(|_| io::Error::other("log cache poisoned"))?;
        let index = cache
            .entries
            .iter()
            .position(|e| e.dir == dir && e.replay.tail == tail);
        index.and_then(|i| cache.entries.remove(i))
    };
    let valid = existing.as_ref().is_some_and(|e| {
        e.incarnation == incarnation
            && e.replay.capacity >= count
            && e.fingerprint.iter().all(|(path, len, identity)| {
                current
                    .iter()
                    .any(|(p, n, i)| p == path && n >= len && i == identity)
            })
    });
    let mut replay = if valid {
        existing.unwrap().replay
    } else {
        Replay::new(dir, incarnation, count, tail)?
    };
    replay.update(dir)?;
    let after = fingerprint(dir, incarnation)?;
    if !current.iter().all(|(path, len, identity)| {
        after
            .iter()
            .any(|(p, n, i)| p == path && n >= len && i == identity)
    }) || list_incarnations(&dir.join(journal::JOURNAL_DIR_NAME))?
        .last()
        .copied()
        != Some(incarnation)
    {
        return Err(io::Error::other(
            "journal replaced or truncated during replay",
        ));
    }
    let mut result = replay.result(from, count, keep_color);
    if current_screen || tail {
        let (rows, cols) = replay.engine.size();
        let mut rows_bytes = if tail {
            replay.engine.styled_history_rows(count)
        } else {
            Vec::new()
        };
        rows_bytes.extend(replay.engine.styled_screen_rows());
        if !keep_color {
            for row in &mut rows_bytes {
                *row = strip_sgr(row);
            }
        }
        result.snapshots = vec![ScreenSnapshot {
            position: replay.position,
            rows,
            cols,
            rows_bytes,
        }];
    }
    let size = replay.memory_bytes();
    if size <= CACHE_BYTES {
        let mut cache = CACHE
            .get()
            .unwrap()
            .lock()
            .map_err(|_| io::Error::other("log cache poisoned"))?;
        while cache
            .entries
            .iter()
            .map(|e| e.replay.memory_bytes())
            .sum::<usize>()
            + size
            > CACHE_BYTES
        {
            cache.entries.pop_front();
        }
        cache.entries.push_back(Entry {
            dir: dir.into(),
            incarnation,
            fingerprint: after,
            replay,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::journal::ShadowJournal;
    use bytes::Bytes;

    fn fixture(name: &str, records: &[&[u8]]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oly-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        for record in records {
            journal
                .record_output(Bytes::copy_from_slice(record))
                .unwrap();
        }
        journal.shutdown();
        dir
    }

    #[test]
    fn tail_preserves_prefix_styles_split_sequences_overwrite_and_alt_state() {
        let prefix = (0..150).map(|n| format!("{n}\r\n")).collect::<String>();
        let dir = fixture(
            "tail-context",
            &[
                b"\x1b[31m",
                prefix.as_bytes(),
                b"\x1b[?104",
                b"9h\x1b[HALT",
                b"\x1b[?1049l",
                b"FINAL\rDONE",
            ],
        );
        let (styled, end) = tail(&dir, 40, true).unwrap();
        assert!(styled.windows(7).any(|s| s == b"\x1b[0;31m"));
        let (plain, scanned) = crate::session::journal::stream::measure_replay_bytes(|| {
            tail(&dir, 40, false).unwrap()
        });
        assert_eq!(scanned, 0, "warm tail must not rescan the prefix");
        assert_eq!(plain.1, end);
        let text = String::from_utf8(plain.0).unwrap();
        assert!(text.ends_with("DONEL\n"), "{text:?}");
        assert!(!text.contains("ALT"));
        assert_eq!(text.lines().count(), 40);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn warm_tail_replays_only_appended_payload_with_independent_prefix_sizes() {
        for lines in [1000, 10000] {
            let dir = std::env::temp_dir().join(format!("oly-tail-cost-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            let prefix = (0..lines).map(|n| format!("{n}\r\n")).collect::<String>();
            journal.record_output(Bytes::from(prefix)).unwrap();
            let flush = |journal: &mut ShadowJournal| {
                journal.request_sync();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    journal.poll_acks();
                    if journal.core.is_fully_durable() {
                        break;
                    }
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            };
            flush(&mut journal);
            tail(&dir, 40, false).unwrap();
            let suffix = b"APPENDED\r\n";
            journal.record_output(Bytes::from_static(suffix)).unwrap();
            flush(&mut journal);
            let (result, scanned) = crate::session::journal::stream::measure_replay_bytes(|| {
                tail(&dir, 40, false).unwrap()
            });
            assert_eq!(scanned, suffix.len());
            assert!(result.0.ends_with(b"APPENDED\n"));
            assert_eq!(String::from_utf8(result.0).unwrap().lines().count(), 40);
            journal.shutdown();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn maximum_tail_keeps_65535_lines_not_the_empty_cursor_row() {
        let text = (0..65540).map(|n| format!("{n}\r\n")).collect::<String>();
        let dir = fixture("max-tail", &[text.as_bytes()]);
        let (output, _) = tail(&dir, 65535, false).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.lines().count(), 65535);
        assert!(text.starts_with("5\n"));
        assert!(text.ends_with("65539\n"));
        let cache = CACHE.get().unwrap().lock().unwrap();
        assert!(
            !cache.entries.iter().any(|e| e.dir == dir && e.replay.tail),
            "oversized tail must be served uncached"
        );
        drop(cache);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_retries_torn_records_and_invalidates_truncation_replacement_and_incarnation() {
        let dir = fixture("invalidate", &[b"FIRST", b"\rSECOND"]);
        let incarnation = list_incarnations(&dir.join(journal::JOURNAL_DIR_NAME)).unwrap()[0];
        let path = journal::incarnation_parts(&dir.join(journal::JOURNAL_DIR_NAME), incarnation)
            .unwrap()[0]
            .1
            .clone();
        let original = std::fs::read(&path).unwrap();
        // Truncate within a complete Output record (before shutdown metadata).
        let second = original.windows(6).position(|s| s == b"SECOND").unwrap();
        std::fs::write(&path, &original[..second + 2]).unwrap();
        let first = collect(&dir, 0, 10, false).unwrap();
        assert_eq!(first.snapshots.len(), 1);
        std::fs::write(&path, &original).unwrap();
        assert_eq!(collect(&dir, 0, 10, false).unwrap().snapshots.len(), 2);
        std::fs::write(&path, &original[..second + 2]).unwrap();
        assert_eq!(collect(&dir, 0, 10, false).unwrap().snapshots.len(), 1);
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, &original).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let (_, scanned) = crate::session::journal::stream::measure_replay_bytes(|| {
            collect(&dir, 0, 10, false).unwrap()
        });
        assert!(scanned > 0);
        let (mut journal, next, _) = ShadowJournal::open(&dir).unwrap();
        assert_ne!(next, incarnation);
        journal.record_output(Bytes::from_static(b"NEW")).unwrap();
        journal.shutdown();
        let latest = collect(&dir, 0, 10, false).unwrap();
        assert_eq!(latest.end_position.incarnation, next);
        assert_eq!(latest.snapshots.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocked_cold_replay_does_not_block_async_status_work() {
        let dir = fixture("async-responsive", &[b"STATUS"]);
        let gate = key_gate(&dir, false).unwrap();
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _lock = gate.lock().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        held_rx.recv().unwrap();
        let worker_dir = dir.clone();
        let replay =
            tokio::task::spawn_blocking(move || collect(&worker_dir, 0, 1, false).unwrap());
        let status = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            "healthy"
        })
        .await;
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert_eq!(status.unwrap(), "healthy");
        assert_eq!(replay.await.unwrap().snapshots.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn long_wrapped_lines_and_partial_cursor_moves_use_recorded_geometry() {
        let dir = std::env::temp_dir().join(format!("oly-wrap-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        journal.record_resize(3, 5).unwrap();
        journal
            .record_output(Bytes::from_static(b"abcdefghijK"))
            .unwrap();
        journal
            .record_output(Bytes::from_static(b"\x1b[1;"))
            .unwrap();
        journal.record_output(Bytes::from_static(b"2HZ")).unwrap();
        journal.shutdown();
        assert_eq!(tail(&dir, 3, false).unwrap().0, b"aZcde\nfghij\nK\n");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_same_key_reads_coalesce_cold_replay() {
        let dir = fixture("coalesce", &[b"ONE"]);
        let start = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let dir = dir.clone();
            let start = start.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                crate::session::journal::stream::measure_replay_bytes(|| {
                    collect(&dir, 0, 1, false).unwrap()
                })
                .1
            }));
        }
        start.wait();
        let bytes: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(bytes.iter().filter(|n| **n == 0).count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn large_unicode_and_title_streams_remain_readable_with_small_retained_views() {
        // More than 2 MiB of ordinary UTF-8/OSC-bearing input used to hit
        // the lifetime input * 32 "metadata" read limit, despite a tiny grid.
        for (name, record) in [
            ("unicode", "日志输出\r\n".repeat(8192).into_bytes()),
            (
                "titles",
                [
                    b"\x1b]0;progress\x07".as_slice(),
                    &b"normal output\r\n".repeat(8192),
                ]
                .concat(),
            ),
        ] {
            let dir =
                std::env::temp_dir().join(format!("oly-large-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
            for _ in 0..32 {
                journal
                    .record_output(Bytes::copy_from_slice(&record))
                    .unwrap();
            }
            journal
                .record_output(Bytes::from_static(b"FINAL-MARKER\r\n"))
                .unwrap();
            journal.shutdown();
            let (output, end) = tail(&dir, 40, false).unwrap();
            assert!(output.ends_with(b"FINAL-MARKER\n"));
            assert_eq!(String::from_utf8(output).unwrap().lines().count(), 40);
            for history in [
                current(&dir, false).unwrap(),
                collect(&dir, 0, 1, false).unwrap(),
            ] {
                assert_eq!(history.end_position.offset, end);
                assert_eq!(history.snapshots.len(), 1);
                assert!(
                    history.snapshots[0]
                        .rows_bytes
                        .iter()
                        .any(|row| row == b"FINAL-MARKER")
                );
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn resource_limits_fail_explicitly_and_oversized_entries_are_uncached() {
        let dir = fixture("resource", &[b"A"]);
        let incarnation = list_incarnations(&dir.join(journal::JOURNAL_DIR_NAME)).unwrap()[0];
        let mut replay = Replay::new(&dir, incarnation, 1, false).unwrap();
        replay.input_bytes = CACHE_BYTES;
        assert!(replay.memory_bytes() > CACHE_BYTES);
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        journal.record_resize(2000, 2000).unwrap();
        journal.shutdown();
        assert!(
            collect(&dir, 0, 1, false)
                .err()
                .unwrap()
                .to_string()
                .contains("million cells")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unrelated_cache_keys_progress_while_one_key_is_busy() {
        let a = fixture("busy", &[b"A"]);
        let b = fixture("independent", &[b"B"]);
        let gate = key_gate(&a, false).unwrap();
        let held = gate.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let other = b.clone();
        let worker = std::thread::spawn(move || {
            tx.send(collect(&other, 0, 1, false).unwrap().snapshots.len())
                .unwrap()
        });
        let progress = rx.recv_timeout(std::time::Duration::from_secs(2));
        drop(held);
        worker.join().unwrap();
        assert_eq!(progress.unwrap(), 1);
        std::fs::remove_dir_all(a).unwrap();
        std::fs::remove_dir_all(b).unwrap();
    }

    #[test]
    fn main_alt_main_blank_dedup_and_zero() {
        let dir = std::env::temp_dir().join(format!("oly-frames-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        for bytes in [
            b"MAIN".as_slice(),
            b"\x1b[?1049h\x1b[HALT",
            b"\x1b[?1049l",
            b"\x1b[2J\x1b[H",
            b"\x1b[2J\x1b[H",
        ] {
            journal
                .record_output(Bytes::copy_from_slice(bytes))
                .unwrap();
        }
        journal.shutdown();
        let history = collect(&dir, 0, 10, false).unwrap();
        let contents: Vec<_> = history
            .snapshots
            .iter()
            .map(|s| crate::session::logs::finish_render(s.rows_bytes.clone(), usize::MAX, false))
            .collect();
        assert_eq!(
            contents,
            [
                b"MAIN\n".to_vec(),
                b"ALT\n".to_vec(),
                b"MAIN\n".to_vec(),
                vec![]
            ]
        );
        let end = history.end_position.offset;
        assert!(collect(&dir, end, 10, false).unwrap().snapshots.is_empty());
        let zero = collect(&dir, 0, 0, false).unwrap();
        assert!(zero.snapshots.is_empty());
        assert_eq!(zero.end_position.offset, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dedup_includes_styled_blank_cells_independent_of_color_projection() {
        let dir = std::env::temp_dir().join(format!("oly-style-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        for bytes in [
            b"\x1b[31m ".as_slice(),
            b"\x1b[H\x1b[32m ",
            b"\x1b[H\x1b[32m ",
        ] {
            journal
                .record_output(Bytes::copy_from_slice(bytes))
                .unwrap();
        }
        journal.shutdown();
        let plain = collect(&dir, 0, 10, false).unwrap();
        let styled = collect(&dir, 0, 10, true).unwrap();
        assert_eq!(plain.snapshots.len(), 2);
        assert_eq!(styled.snapshots.len(), 2);
        assert_eq!(plain.end_position, styled.end_position);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn current_screen_applies_resize_without_emitting_a_frame() {
        let dir = std::env::temp_dir().join(format!("oly-resize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut journal, _, _) = ShadowJournal::open(&dir).unwrap();
        journal.record_output(Bytes::from_static(b"A")).unwrap();
        journal.record_resize(10, 50).unwrap();
        journal.shutdown();
        let frames = collect(&dir, 0, 10, false).unwrap();
        assert_eq!(frames.snapshots.len(), 1);
        assert_eq!(frames.snapshots[0].rows, DEFAULT_ROWS);
        let screen = current(&dir, false).unwrap();
        assert_eq!(screen.snapshots[0].rows, 10);
        assert_eq!(screen.snapshots[0].cols, 50);
        assert_eq!(screen.end_position, frames.end_position);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn warm_replay_accepts_appended_records_and_split_sequences() {
        let dir = std::env::temp_dir().join(format!("oly-frames-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut journal, incarnation, _) = ShadowJournal::open(&dir).unwrap();
        journal
            .record_output(Bytes::from_static(b"MAIN\x1b[?104"))
            .unwrap();
        let flush = |journal: &mut ShadowJournal| {
            journal.request_sync();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                journal.poll_acks();
                if journal.core.is_fully_durable() {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        flush(&mut journal);
        let first = collect(&dir, 0, 10, false).unwrap();
        assert_eq!(first.snapshots.len(), 1);
        journal
            .record_output(Bytes::from_static(b"9h\x1b[HALT"))
            .unwrap();
        flush(&mut journal);
        let second = collect(&dir, first.end_position.offset, 10, false).unwrap();
        assert_eq!(second.end_position.incarnation, incarnation);
        assert_eq!(second.snapshots.len(), 1);
        assert_eq!(
            crate::session::logs::finish_render(
                second.snapshots[0].rows_bytes.clone(),
                usize::MAX,
                false
            ),
            b"ALT\n"
        );
        journal
            .record_output(Bytes::from_static(b"\x1b[?1049l"))
            .unwrap();
        flush(&mut journal);
        let third = collect(&dir, second.end_position.offset, 10, false).unwrap();
        assert_eq!(
            crate::session::logs::finish_render(
                third.snapshots[0].rows_bytes.clone(),
                usize::MAX,
                false
            ),
            b"MAIN\n"
        );
        let (warm, replayed) = crate::session::journal::stream::measure_replay_bytes(|| {
            collect(&dir, 0, 10, true).unwrap()
        });
        assert_eq!(
            replayed, 0,
            "unchanged warm history must not feed old records"
        );
        assert_eq!(warm.end_position, third.end_position);
        assert_eq!(warm.snapshots.len(), 3);
        journal.shutdown();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
