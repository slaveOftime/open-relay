//! Segmented session journal: typed, checksummed records with stable
//! sequence numbers (PLAN.md §6.1, invariants I1/I3/I8/I10).
//!
//! M1 wires this as a **shadow journal** (dev-gated by `OLY_JOURNAL`):
//! every raw PTY chunk, resize, mode revision (Policy) and lifecycle fact
//! is sequenced under the session write lock and appended by a dedicated
//! thread with a group-sync cadence. `output.log` remains the canonical
//! store until M3.
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

use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const RECORD_MAGIC: &[u8; 4] = b"OJRN";
const RECORD_VERSION: u16 = 1;
/// magic + version + kind + flags + seq + elapsed_ms + payload_len + crc32.
const HEADER_LEN: usize = 4 + 2 + 2 + 4 + 8 + 8 + 4 + 4;
/// Refuse absurd length fields before allocating (PLAN.md §7.4: limits are
/// checked before allocation).
const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024;

/// Typed journal record kinds. Unknown kinds are unrecoverable corruption for
/// this format version: a torn or aliased tail must stop the scan, never be
/// silently skipped (I3 — missing history is explicit, not empty success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RecordKind {
    /// Original PTY output bytes.
    Output = 1,
    /// A successful PTY resize barrier: `rows(u16) cols(u16)` payload.
    Resize = 2,
    /// Lifecycle transition (spawned/completed/capture state), UTF-8 payload.
    Lifecycle = 3,
    /// Points at the checkpoint that reconstructs state at `seq`.
    CheckpointRef = 4,
    /// Terminal profile/policy facts needed to interpret the stream.
    Policy = 5,
}

impl RecordKind {
    fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Output),
            2 => Some(Self::Resize),
            3 => Some(Self::Lifecycle),
            4 => Some(Self::CheckpointRef),
            5 => Some(Self::Policy),
            _ => None,
        }
    }
}

/// One decoded journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub seq: u64,
    /// Milliseconds since the writer's start (monotonic, per incarnation).
    pub elapsed_ms: u64,
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3, reflected) — hand-rolled to avoid a new dependency;
// verified against the standard check vector in the tests below.
// ---------------------------------------------------------------------------

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Append handle for one journal segment. The writer is a pure serializer:
/// the **sequencer** assigns `seq` and `elapsed_ms` before the event is
/// published, and the writer writes what it is given. It never allocates
/// sequences and never invents timestamps, so reopening an active segment
/// can never move event time backwards and publication never waits on disk.
pub struct SegmentWriter {
    file: fs::File,
    /// Bytes written so far; the authoritative segment length.
    written: u64,
}

impl SegmentWriter {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)?,
            written: 0,
        })
    }

    pub fn open_append(path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new().append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self { file, written })
    }

    /// Append one already-sequenced record.
    pub fn append_record(
        &mut self,
        kind: RecordKind,
        seq: u64,
        elapsed_ms: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        if payload.len() > MAX_PAYLOAD_LEN as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "journal payload of {} bytes exceeds the {} byte limit",
                    payload.len(),
                    MAX_PAYLOAD_LEN
                ),
            ));
        }

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(RECORD_MAGIC);
        header[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
        // flags [8..12] stay zero until a feature needs them.
        header[12..20].copy_from_slice(&seq.to_le_bytes());
        header[20..28].copy_from_slice(&elapsed_ms.to_le_bytes());
        header[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let crc = crc32_two(&header[..32], payload);
        header[32..36].copy_from_slice(&crc.to_le_bytes());

        self.file.write_all(&header)?;
        self.file.write_all(payload)?;
        self.written += (HEADER_LEN + payload.len()) as u64;
        Ok(())
    }

    /// Push appended records to the storage device. Callers decide the
    /// group-sync cadence (PLAN.md §4.2): live publication does not wait for
    /// this, but `durable_seq` may not advance past the last synced record.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Bytes written so far — the segment's authoritative length.
    pub fn len(&self) -> u64 {
        self.written
    }

    pub fn is_empty(&self) -> bool {
        self.written == 0
    }
}

// ---------------------------------------------------------------------------
// Per-session sequencer (M1)
// ---------------------------------------------------------------------------

/// Directory inside `sessions/<id>/` holding the journal segments.
pub const JOURNAL_DIR_NAME: &str = "journal";
const SEGMENT_PREFIX: &str = "seg-";
const SEGMENT_SUFFIX: &str = ".ojrn";

/// Maximum size of one segment part before the appender rolls over to the
/// next part of the same incarnation. Bounding part size bounds recovery
/// work, per-read validation work and sparse-index memory (ADR-0002).
pub const DEFAULT_SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Segments are split into bounded parts within one incarnation:
/// `seg-{incarnation:08}-{part:04}.ojrn`. Sequences continue across
/// parts, so cursors stay `{incarnation, seq}` and never name parts.
fn segment_path(journal_dir: &Path, incarnation: u64, part: u64) -> PathBuf {
    journal_dir.join(format!(
        "{SEGMENT_PREFIX}{incarnation:08}-{part:04}{SEGMENT_SUFFIX}"
    ))
}

fn parse_segment_name(name: &str) -> Option<(u64, u64)> {
    let rest = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    let (incarnation, part) = rest.split_once('-')?;
    Some((incarnation.parse().ok()?, part.parse().ok()?))
}

/// All `(incarnation, part)` pairs in a journal directory, ascending.
pub fn list_segments(journal_dir: &Path) -> io::Result<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    if !journal_dir.exists() {
        return Ok(segments);
    }
    for entry in fs::read_dir(journal_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pair) = parse_segment_name(name) else {
            continue;
        };
        segments.push(pair);
    }
    segments.sort_unstable();
    Ok(segments)
}

/// All incarnation numbers present in a journal directory, ascending.
pub fn list_incarnations(journal_dir: &Path) -> io::Result<Vec<u64>> {
    let segments = list_segments(journal_dir)?;
    let mut incarnations: Vec<u64> = segments
        .iter()
        .map(|&(incarnation, _)| incarnation)
        .collect();
    incarnations.dedup();
    Ok(incarnations)
}

/// Sequence number of a segment part's first record, read from its header
/// only (O(1)); `None` for an empty part (a crash between rollover and
/// the first append can leave one as the newest part).
fn part_first_seq(path: &Path) -> io::Result<Option<u64>> {
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; HEADER_LEN];
    match read_exact_or_partial(&mut file, &mut header)? {
        ReadPiece::Empty => Ok(None),
        ReadPiece::Partial => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: torn first record header", path.display()),
        )),
        ReadPiece::Complete => {
            if header[0..4] != *RECORD_MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: bad journal magic", path.display()),
                ));
            }
            Ok(Some(u64::from_le_bytes(
                header[12..20].try_into().expect("seq field"),
            )))
        }
    }
}

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
    /// Offset of the first byte that is not a valid record.
    pub valid_len: u64,
    /// Whether a torn tail was rewound (file truncated to `valid_len`).
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

    let segments = list_segments(&journal_dir)?;
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
        report = Some(RecoveryReport {
            incarnation: previous,
            records: stats.records,
            last_seq: stats.last_seq,
            stop: stats.stop,
            valid_len: stats.valid_len,
            rewound,
        });
    }

    let incarnation = segments
        .last()
        .map(|&(incarnation, _)| incarnation)
        .unwrap_or(0)
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

/// Best-effort directory sync so segment creation/truncation survives a
/// crash. Unsupported on non-Unix targets, where this is a no-op.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
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

/// Default byte budget for a session's recent replay cache.
pub const DEFAULT_CACHE_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Default message bound for one session's journal queue.
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
/// Default byte bound for one session's journal queue. A stalled appender
/// can therefore hold at most this many bytes of session output before
/// submission is rejected and the session degrades explicitly — memory
/// cannot grow indefinitely (PLAN.md §4.2).
pub const DEFAULT_QUEUE_BUDGET_BYTES: usize = 32 * 1024 * 1024;
/// Default group-sync cadence: the appender syncs at most this long after
/// the last unsynced append, bounding the crash-loss window without
/// per-record `fsync` latency on the ingest path (ADR-0002).
pub const DEFAULT_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// The in-memory sequencing authority (PLAN.md §4.1/§4.2, invariants
/// I1/I8). Allocates monotonic `seq` and monotonic `elapsed_ms` **before**
/// an event is published, retains recent events in a byte-bounded cache
/// until the journal makes them replayable, and tracks the
/// `head_seq`/`journal_seq`/`durable_seq` cursors separately. It performs
/// no I/O; persistence is the [`JournalAppender`]'s job.
pub struct SequencerCore {
    incarnation: u64,
    next_seq: u64,
    started: std::time::Instant,
    journal_seq: u64,
    durable_seq: u64,
    degraded: Option<String>,
    cache: std::collections::VecDeque<OrderedEvent>,
    cache_bytes: usize,
    cache_budget_bytes: usize,
}

impl SequencerCore {
    pub fn new(incarnation: u64) -> Self {
        Self::with_budget(incarnation, DEFAULT_CACHE_BUDGET_BYTES)
    }

    pub fn with_budget(incarnation: u64, cache_budget_bytes: usize) -> Self {
        Self {
            incarnation,
            next_seq: 1,
            started: std::time::Instant::now(),
            journal_seq: 0,
            durable_seq: 0,
            degraded: None,
            cache: std::collections::VecDeque::new(),
            cache_bytes: 0,
            cache_budget_bytes,
        }
    }

    /// Assign the next sequence number and monotonic elapsed time, retain
    /// the event in the recent cache, and return it for publication.
    ///
    /// If the cache is over budget, journaled events are evicted first. If
    /// it is *still* over budget, the event is retained anyway — an event
    /// is never silently dropped — and [`Self::over_budget`] reports the
    /// backpressure condition so the caller can slow ingestion.
    pub fn publish(&mut self, kind: RecordKind, payload: bytes::Bytes) -> OrderedEvent {
        let event = OrderedEvent {
            cursor: JournalCursor {
                incarnation: self.incarnation,
                seq: self.next_seq,
            },
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            kind,
            payload,
        };
        self.next_seq += 1;
        self.cache_bytes += event.payload.len();
        if self.cache_bytes > self.cache_budget_bytes {
            self.evict_journaled();
        }
        self.cache.push_back(event.clone());
        event
    }

    /// Drop cached events the journal already serves (`seq <= journal_seq`).
    fn evict_journaled(&mut self) {
        while let Some(front) = self.cache.front() {
            if front.cursor.seq > self.journal_seq {
                break;
            }
            self.cache_bytes -= front.payload.len();
            self.cache.pop_front();
        }
    }

    /// The cache holds events beyond its byte budget because the journal
    /// has not caught up. Callers should backpressure ingestion.
    pub fn over_budget(&self) -> bool {
        self.cache_bytes > self.cache_budget_bytes
    }

    /// The appender confirmed records through `seq` are readable from
    /// storage. Advances `journal_seq`; never moves it backwards.
    pub fn note_journaled(&mut self, seq: u64) {
        self.journal_seq = self.journal_seq.max(seq);
    }

    /// A completed sync covers records through `seq`.
    pub fn note_durable(&mut self, seq: u64) {
        self.durable_seq = self.durable_seq.max(seq);
    }

    /// Persistence failed; the session is explicitly degraded (PLAN.md
    /// §4.2). Subsequent `publish` calls still sequence — publication must
    /// not silently continue pretending durability is on track.
    pub fn degrade(&mut self, reason: impl Into<String>) {
        if self.degraded.is_none() {
            self.degraded = Some(reason.into());
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.is_some()
    }

    pub fn degraded_reason(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    /// Highest sequence published so far, if any.
    pub fn head_seq(&self) -> Option<u64> {
        (self.next_seq > 1).then_some(self.next_seq - 1)
    }

    /// Highest sequence contiguously readable from the journal.
    pub fn journal_seq(&self) -> u64 {
        self.journal_seq
    }

    /// Highest sequence covered by a completed sync.
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq
    }

    /// Number of events currently retained in the recent cache.
    pub fn cached_events(&self) -> usize {
        self.cache.len()
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }
}

/// Acknowledgement stream from the journal appender back to the sequencer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalAck {
    /// Records through this sequence are appended and readable.
    Journaled(u64),
    /// A completed sync covers records through this sequence.
    Durable(u64),
    /// Appending failed (I/O error or a contiguity violation); the
    /// appender is dead and rejects further records with the same error.
    Failed(String),
}

enum AppenderMsg {
    Record(Box<OrderedEvent>),
    Sync,
    Shutdown,
}

/// The journal appender: sole owner of the active [`SegmentWriter`],
/// running on its own thread so sequencing and live publication never
/// perform disk I/O (PLAN.md §4.2). It validates contiguity, appends in
/// assigned order, group-syncs on request, and reports progress through
/// [`JournalAck`]s. The queue is bounded in both messages and bytes.
pub struct JournalAppender {
    tx: std::sync::mpsc::SyncSender<AppenderMsg>,
    queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    queue_budget_bytes: usize,
}

impl JournalAppender {
    /// Open the journal under `session_dir` (recovering the previous
    /// incarnation) and spawn the appender thread for the new one.
    pub fn spawn(
        session_dir: &Path,
    ) -> io::Result<(
        Self,
        u64,
        Option<RecoveryReport>,
        std::sync::mpsc::Receiver<JournalAck>,
    )> {
        Self::spawn_with_sync_interval(session_dir, DEFAULT_SYNC_INTERVAL)
    }

    /// Like [`Self::spawn`], with an explicit group-sync cadence (tests,
    /// and the cadence probe that feeds the ADR-0002 decision).
    pub fn spawn_with_sync_interval(
        session_dir: &Path,
        sync_interval: std::time::Duration,
    ) -> io::Result<(
        Self,
        u64,
        Option<RecoveryReport>,
        std::sync::mpsc::Receiver<JournalAck>,
    )> {
        Self::spawn_with_options(session_dir, sync_interval, DEFAULT_SEGMENT_MAX_BYTES)
    }

    /// Spawn with an explicit segment-part size limit (tests use tiny
    /// limits to exercise rollover).
    pub fn spawn_with_options(
        session_dir: &Path,
        sync_interval: std::time::Duration,
        max_part_bytes: u64,
    ) -> io::Result<(
        Self,
        u64,
        Option<RecoveryReport>,
        std::sync::mpsc::Receiver<JournalAck>,
    )> {
        let opened = open(session_dir)?;
        let writer = RollingSegmentWriter::new(
            opened.journal_dir.clone(),
            opened.incarnation,
            opened.writer,
            max_part_bytes,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(DEFAULT_QUEUE_CAPACITY);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let queued_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_queued = std::sync::Arc::clone(&queued_bytes);
        std::thread::Builder::new()
            .name("journal-appender".to_string())
            .spawn(move || appender_loop(writer, rx, ack_tx, worker_queued, sync_interval))?;
        Ok((
            Self {
                tx,
                queued_bytes,
                queue_budget_bytes: DEFAULT_QUEUE_BUDGET_BYTES,
            },
            opened.incarnation,
            opened.report,
            ack_rx,
        ))
    }

    /// Queue an already-sequenced event for append. Fails without
    /// enqueueing when the queue is full or the byte budget is exhausted;
    /// the caller must degrade/backpressure, never drop silently.
    pub fn try_submit(&self, event: OrderedEvent) -> Result<(), JournalSubmitError> {
        use std::sync::atomic::Ordering;
        let len = event.payload.len();
        let queued = self.queued_bytes.fetch_add(len, Ordering::Relaxed);
        if queued + len > self.queue_budget_bytes {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
            return Err(JournalSubmitError::QueueBudgetExhausted);
        }
        match self.tx.try_send(AppenderMsg::Record(Box::new(event))) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                Err(JournalSubmitError::QueueFull)
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                Err(JournalSubmitError::AppenderDead)
            }
        }
    }

    /// Ask the appender to group-sync everything written so far; the
    /// resulting [`JournalAck::Durable`] advances `durable_seq`.
    pub fn request_sync(&self) {
        // A full queue delays the sync rather than dropping it silently:
        // block briefly is wrong on a hot path, so skip and let the next
        // cadence tick retry.
        let _ = self.tx.try_send(AppenderMsg::Sync);
    }

    /// Stop the appender thread after draining queued records.
    pub fn shutdown(&self) {
        let _ = self.tx.send(AppenderMsg::Shutdown);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalSubmitError {
    /// Message bound reached.
    QueueFull,
    /// Byte bound reached — the appender is stalled or the session is
    /// producing faster than storage can absorb.
    QueueBudgetExhausted,
    /// The appender thread is gone.
    AppenderDead,
    /// The event itself is malformed (e.g. a Policy key/value that does
    /// not fit the codec). Refused before sequencing.
    InvalidEvent(String),
    /// Persistence already failed for this incarnation. New events are
    /// refused (not cached) so a dead or stalled journal cannot grow
    /// memory indefinitely; the incomplete-capture boundary is the
    /// degrade point recorded in the core (PLAN.md §6.1 exit).
    PersistenceDegraded,
}

impl std::fmt::Display for JournalSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "journal queue is full"),
            Self::QueueBudgetExhausted => write!(f, "journal queue byte budget exhausted"),
            Self::AppenderDead => write!(f, "journal appender is dead"),
            Self::PersistenceDegraded => write!(f, "journal persistence is degraded"),
            Self::InvalidEvent(reason) => write!(f, "invalid journal event: {reason}"),
        }
    }
}

/// The appender's segment writer with rollover: when the active part
/// reaches `max_part_bytes` it is sealed (synced) and the next part of
/// the same incarnation begins. Sequences continue across parts, so
/// cursors never name parts (ADR-0002).
struct RollingSegmentWriter {
    journal_dir: PathBuf,
    incarnation: u64,
    part: u64,
    writer: SegmentWriter,
    part_bytes: u64,
    max_part_bytes: u64,
}

impl RollingSegmentWriter {
    fn new(
        journal_dir: PathBuf,
        incarnation: u64,
        writer: SegmentWriter,
        max_part_bytes: u64,
    ) -> Self {
        Self {
            journal_dir,
            incarnation,
            part: 1,
            writer,
            part_bytes: 0,
            max_part_bytes,
        }
    }

    fn append_record(
        &mut self,
        kind: RecordKind,
        seq: u64,
        elapsed_ms: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        let record_bytes = HEADER_LEN as u64 + payload.len() as u64;
        if self.part_bytes > 0 && self.part_bytes + record_bytes > self.max_part_bytes {
            // Seal the current part durable before moving on: after a
            // crash only the newest part may need recovery.
            self.writer.sync()?;
            self.part += 1;
            self.writer = SegmentWriter::create(&segment_path(
                &self.journal_dir,
                self.incarnation,
                self.part,
            ))?;
            sync_dir(&self.journal_dir)?;
            self.part_bytes = 0;
        }
        self.writer.append_record(kind, seq, elapsed_ms, payload)?;
        self.part_bytes += record_bytes;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.writer.sync()
    }
}

fn appender_loop(
    mut writer: RollingSegmentWriter,
    rx: std::sync::mpsc::Receiver<AppenderMsg>,
    ack_tx: std::sync::mpsc::Sender<JournalAck>,
    queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    sync_interval: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    let mut expected_seq = 1u64;
    let mut last_written = 0u64;
    let mut last_durable = 0u64;
    let mut dead: Option<String> = None;
    // Absolute deadline for the next group sync. Set when the first
    // unsynced record lands; a `recv_timeout` that restarts on every
    // message would let a continuous producer starve durability forever.
    let mut sync_deadline: Option<Instant> = None;

    let fail = |ack_tx: &std::sync::mpsc::Sender<JournalAck>,
                dead: &mut Option<String>,
                reason: String| {
        *dead = Some(reason.clone());
        let _ = ack_tx.send(JournalAck::Failed(reason));
    };

    let do_sync = |writer: &mut RollingSegmentWriter,
                   ack_tx: &std::sync::mpsc::Sender<JournalAck>,
                   dead: &mut Option<String>,
                   last_written: u64,
                   last_durable: &mut u64,
                   sync_deadline: &mut Option<Instant>| {
        match writer.sync() {
            Ok(()) => {
                *last_durable = last_written;
                *sync_deadline = None;
                let _ = ack_tx.send(JournalAck::Durable(last_written));
            }
            Err(err) => {
                fail(ack_tx, dead, format!("journal sync failed: {err}"));
            }
        }
    };

    loop {
        let dirty = dead.is_none() && last_written > last_durable;
        let msg = if dirty {
            // Sync at the absolute deadline no matter how busy the queue
            // stays; never wait longer than the cadence.
            let wait = sync_deadline
                .unwrap_or_else(|| Instant::now() + sync_interval)
                .saturating_duration_since(Instant::now());
            rx.recv_timeout(wait)
        } else {
            match rx.recv() {
                Ok(msg) => Ok(msg),
                Err(std::sync::mpsc::RecvError) => {
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                }
            }
        };
        match msg {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) | Ok(AppenderMsg::Shutdown) => {
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if dirty {
                    do_sync(
                        &mut writer,
                        &ack_tx,
                        &mut dead,
                        last_written,
                        &mut last_durable,
                        &mut sync_deadline,
                    );
                }
            }
            Ok(AppenderMsg::Sync) => {
                if let Some(reason) = &dead {
                    let _ = ack_tx.send(JournalAck::Failed(reason.clone()));
                } else {
                    do_sync(
                        &mut writer,
                        &ack_tx,
                        &mut dead,
                        last_written,
                        &mut last_durable,
                        &mut sync_deadline,
                    );
                }
            }
            Ok(AppenderMsg::Record(event)) => {
                queued_bytes.fetch_sub(event.payload.len(), Ordering::Relaxed);
                if let Some(reason) = &dead {
                    let _ = ack_tx.send(JournalAck::Failed(reason.clone()));
                    continue;
                }
                if event.cursor.seq != expected_seq {
                    fail(
                        &ack_tx,
                        &mut dead,
                        format!(
                            "journal contiguity violation: expected seq {expected_seq}, got {}",
                            event.cursor.seq
                        ),
                    );
                    continue;
                }
                match writer.append_record(
                    event.kind,
                    event.cursor.seq,
                    event.elapsed_ms,
                    &event.payload,
                ) {
                    Ok(()) => {
                        if last_written <= last_durable {
                            sync_deadline = Some(Instant::now() + sync_interval);
                        }
                        last_written = event.cursor.seq;
                        expected_seq += 1;
                        let _ = ack_tx.send(JournalAck::Journaled(event.cursor.seq));
                    }
                    Err(err) => {
                        fail(&ack_tx, &mut dead, format!("journal append failed: {err}"));
                    }
                }
            }
        }
    }

    // Final barrier: a graceful shutdown (or the last sender going away)
    // must not strand acknowledged-but-unsynced records (PLAN I8/I10).
    if dead.is_none() && last_written > last_durable {
        do_sync(
            &mut writer,
            &ack_tx,
            &mut dead,
            last_written,
            &mut last_durable,
            &mut sync_deadline,
        );
    }
}

// ---------------------------------------------------------------------------
// Typed payload codecs for non-output records
// ---------------------------------------------------------------------------

/// Resize payload: `rows u16 LE | cols u16 LE`. Geometry is part of the
/// ordered record (ADR-0003): replay applies resizes at their stream
/// position instead of reconstructing them from side channels.
pub fn encode_resize_payload(rows: u16, cols: u16) -> [u8; 4] {
    let mut payload = [0u8; 4];
    payload[0..2].copy_from_slice(&rows.to_le_bytes());
    payload[2..4].copy_from_slice(&cols.to_le_bytes());
    payload
}

/// Policy record codec: one `key=value` line. Keys must be nonempty and
/// free of `=` and newlines; values must be newline-free. That keeps
/// policy payloads line-oriented and grep-able in a hexdump.
pub fn policy_payload(key: &str, value: &str) -> Vec<u8> {
    format!("{key}={value}").into_bytes()
}

/// Inverse of [`policy_payload`]; `None` for malformed payloads.
pub fn parse_policy(payload: &[u8]) -> Option<(&str, &str)> {
    let text = std::str::from_utf8(payload).ok()?;
    let (key, value) = text.split_once('=')?;
    if key.is_empty() || value.contains('\n') {
        return None;
    }
    Some((key, value))
}

pub fn decode_resize_payload(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() != 4 {
        return None;
    }
    let rows = u16::from_le_bytes([payload[0], payload[1]]);
    let cols = u16::from_le_bytes([payload[2], payload[3]]);
    (rows > 0 && cols > 0).then_some((rows, cols))
}

/// Lifecycle facts worth ordering against the output stream. Only terminal
/// transitions and the start fact are journaled; transient states
/// (`running`, `stopping`) are observable from metadata, not stream facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LifecycleCode {
    Started = 1,
    Stopped = 2,
    Killed = 3,
    Failed = 4,
    /// The PTY output stream reached its end (EOF, read error or writer
    /// teardown). Process exit and PTY EOF are separate facts (PLAN.md
    /// I10): completion is only journaled after this record.
    OutputClosed = 5,
}

impl LifecycleCode {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Started),
            2 => Some(Self::Stopped),
            3 => Some(Self::Killed),
            4 => Some(Self::Failed),
            5 => Some(Self::OutputClosed),
            _ => None,
        }
    }
}

/// Lifecycle payload: `code u8 | exit_code i32 LE | detail UTF-8`.
/// `i32::MIN` is the sentinel for "no exit code" so `Some(0)` (a clean
/// exit) stays distinguishable from an absent code.
pub fn encode_lifecycle_payload(
    code: LifecycleCode,
    exit_code: Option<i32>,
    detail: &str,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + detail.len());
    payload.push(code as u8);
    payload.extend_from_slice(&exit_code.unwrap_or(i32::MIN).to_le_bytes());
    payload.extend_from_slice(detail.as_bytes());
    payload
}

pub fn decode_lifecycle_payload(payload: &[u8]) -> Option<(LifecycleCode, Option<i32>, &str)> {
    if payload.len() < 5 {
        return None;
    }
    let code = LifecycleCode::from_u8(payload[0])?;
    let raw_exit = i32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let exit_code = (raw_exit != i32::MIN).then_some(raw_exit);
    let detail = std::str::from_utf8(&payload[5..]).ok()?;
    Some((code, exit_code, detail))
}

/// M1 shadow journal: bundles the sequencing core with the appender for
/// the shadow wiring behind [`shadow_enabled`].
pub struct ShadowJournal {
    pub core: SequencerCore,
    appender: JournalAppender,
    acks: std::sync::mpsc::Receiver<JournalAck>,
}

impl ShadowJournal {
    pub fn open(session_dir: &Path) -> io::Result<(Self, u64, Option<RecoveryReport>)> {
        Self::open_with_sync_interval(session_dir, DEFAULT_SYNC_INTERVAL)
    }

    pub fn open_with_sync_interval(
        session_dir: &Path,
        sync_interval: std::time::Duration,
    ) -> io::Result<(Self, u64, Option<RecoveryReport>)> {
        Self::open_with_options(session_dir, sync_interval, DEFAULT_SEGMENT_MAX_BYTES)
    }

    /// Open with explicit sync cadence and segment-part size (tests use
    /// tiny part sizes to exercise rollover).
    pub fn open_with_options(
        session_dir: &Path,
        sync_interval: std::time::Duration,
        max_part_bytes: u64,
    ) -> io::Result<(Self, u64, Option<RecoveryReport>)> {
        let (appender, incarnation, report, acks) =
            JournalAppender::spawn_with_options(session_dir, sync_interval, max_part_bytes)?;
        Ok((
            Self {
                core: SequencerCore::new(incarnation),
                appender,
                acks,
            },
            incarnation,
            report,
        ))
    }

    /// Sequence an event, retain it in the recent cache and submit it to
    /// the appender. Returns the assigned cursor; a submission failure
    /// degrades the core and returns the error for the caller to log.
    pub fn record(
        &mut self,
        kind: RecordKind,
        payload: bytes::Bytes,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.poll_acks();
        // Once persistence has failed, stop publishing: caching further
        // events that can never be journaled would let a disk stall grow
        // memory indefinitely. The degrade point is the explicit
        // incomplete-capture boundary (I8/I10).
        if self.core.is_degraded() {
            return Err(JournalSubmitError::PersistenceDegraded);
        }
        let event = self.core.publish(kind, payload);
        let cursor = event.cursor;
        if let Err(err) = self.appender.try_submit(event) {
            self.core.degrade(err.to_string());
            return Err(err);
        }
        Ok(cursor)
    }

    pub fn record_output(
        &mut self,
        payload: bytes::Bytes,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(RecordKind::Output, payload)
    }

    pub fn record_resize(
        &mut self,
        rows: u16,
        cols: u16,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(
            RecordKind::Resize,
            bytes::Bytes::copy_from_slice(&encode_resize_payload(rows, cols)),
        )
    }

    /// Record a checkpoint anchoring this stream position: restore and
    /// retention may both start from it (PLAN §5.3).
    pub fn record_checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(RecordKind::CheckpointRef, encode_checkpoint(checkpoint))
    }

    /// Record a terminal-relevant revision (e.g. a mode flip) at its
    /// ordered stream position, right after the output that caused it.
    pub fn record_policy(
        &mut self,
        key: &str,
        value: &str,
    ) -> Result<JournalCursor, JournalSubmitError> {
        if key.is_empty() || key.contains(['=', '\n']) || value.contains('\n') {
            return Err(JournalSubmitError::InvalidEvent(format!(
                "invalid policy key/value: {key:?}"
            )));
        }
        self.record(
            RecordKind::Policy,
            bytes::Bytes::from(policy_payload(key, value)),
        )
    }

    pub fn record_lifecycle(
        &mut self,
        code: LifecycleCode,
        exit_code: Option<i32>,
        detail: &str,
    ) -> Result<JournalCursor, JournalSubmitError> {
        self.record(
            RecordKind::Lifecycle,
            bytes::Bytes::from(encode_lifecycle_payload(code, exit_code, detail)),
        )
    }

    /// Drain pending acknowledgements into the cursors.
    pub fn poll_acks(&mut self) {
        while let Ok(ack) = self.acks.try_recv() {
            match ack {
                JournalAck::Journaled(seq) => self.core.note_journaled(seq),
                JournalAck::Durable(seq) => self.core.note_durable(seq),
                JournalAck::Failed(reason) => self.core.degrade(reason),
            }
        }
    }

    /// Request a group sync (durability cadence; see PLAN.md §4.2).
    pub fn request_sync(&self) {
        self.appender.request_sync();
    }
}

/// M1/M2 kept the journal behind the `OLY_JOURNAL` dev switch; since
/// M3-1 the journal is always on — it is becoming the canonical stream
/// (ADR-0002). The switch is gone; the function remains only so call
/// sites read intentionally.
pub fn shadow_enabled() -> bool {
    true
}

fn crc32_two(first: &[u8], second: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in first.iter().chain(second) {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Reader / torn-tail recovery
// ---------------------------------------------------------------------------

/// Why a segment scan stopped. Recovery distinguishes a torn **active tail**
/// (rewind to `valid_len`) from **corruption** (quarantine and report — never
/// silently truncate). Segment state (active versus sealed) is owned by the
/// caller; a non-tail stop in a sealed segment is always corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStop {
    /// The file ends exactly at a record boundary.
    CleanEof,
    /// The last record is incomplete (crash mid-write). Rewind to
    /// `valid_len` on an active segment.
    PartialTail,
    /// Magic bytes do not match the journal format.
    InvalidHeader,
    /// The record version is not supported by this reader.
    UnsupportedVersion,
    /// The record kind is unknown to this format version.
    UnknownKind,
    /// The declared payload length exceeds the allocation bound.
    OversizeLength,
    /// The stored CRC does not match header plus payload.
    CrcMismatch,
    /// A record's sequence number is not the previous one plus one — an
    /// earlier record was lost or the tail was aliased.
    SequenceDiscontinuity,
}

impl ScanStop {
    /// Only a partial tail may be rewound by truncation (on an active
    /// segment); everything else must be quarantined and reported.
    pub fn may_rewind(self) -> bool {
        matches!(self, ScanStop::PartialTail)
    }
}

/// Result of scanning a segment: every fully valid record in order, the byte
/// length of the valid prefix, and why the scan stopped.
#[derive(Debug)]
pub struct ScanOutcome {
    pub records: Vec<Record>,
    /// Byte offset one past the last valid record. On recovery the caller
    /// truncates an active segment to this length (torn tail) or quarantines
    /// the file (any other stop reason).
    pub valid_len: u64,
    pub stop: ScanStop,
}

impl ScanOutcome {
    pub fn is_clean(&self) -> bool {
        self.stop == ScanStop::CleanEof
    }
}

/// Scan a segment from the start, stopping at the first invalid byte.
///
/// Recovery contract (PLAN.md §6.2): a crash can tear the *tail* of the
/// active segment; recovery rewinds to `valid_len`
/// (`ScanStop::PartialTail`). Anything else is corruption, not a tear — the
/// caller quarantines and reports it instead of silently continuing.
pub fn scan_segment(path: &Path) -> io::Result<ScanOutcome> {
    // Lenient on the first sequence: this is the raw segment inspector
    // (recovery tooling, probes, tests). Stream-level reads enforce the
    // expected first sequence themselves via `scan_segment_stats*` and
    // the read APIs below.
    Ok(scan_impl(path, ScanMode::All, None, None)?.outcome)
}

/// Payload-free segment statistics: record count, last sequence, valid
/// prefix length and stop reason. Recovery and cursor validation use this
/// so their memory stays O(1) regardless of segment size.
#[derive(Debug)]
pub struct SegmentStats {
    pub records: u64,
    pub last_seq: Option<u64>,
    pub valid_len: u64,
    pub stop: ScanStop,
}

/// Stats scan requiring the segment to start at sequence 1 (a complete
/// incarnation prefix).
pub fn scan_segment_stats(path: &Path) -> io::Result<SegmentStats> {
    scan_segment_stats_from(path, Some(1))
}

/// Stats scan with an explicit expected first sequence (`None` accepts any
/// first record; used for continuation segments and partial inspection).
pub fn scan_segment_stats_from(
    path: &Path,
    expected_first_seq: Option<u64>,
) -> io::Result<SegmentStats> {
    let result = scan_impl(path, ScanMode::Stats, expected_first_seq, None)?;
    Ok(SegmentStats {
        records: result.record_count,
        last_seq: result.last_seq,
        valid_len: result.outcome.valid_len,
        stop: result.outcome.stop,
    })
}

/// Result of a fixed-range read: the in-window records (contiguous, in
/// order), the integrity status of the consumed prefix, and whether the
/// byte budget cut the window short.
///
/// `stop` reports stream integrity of everything read up to the stopping
/// point. On a **live** segment `ScanStop::PartialTail` is normal — it
/// means "more is being appended right now", not corruption. When
/// `truncated` is set, retry with `from_seq = last_returned_seq + 1`.
#[derive(Debug)]
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
fn incarnation_parts(journal_dir: &Path, incarnation: u64) -> io::Result<Vec<(u64, PathBuf)>> {
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

/// Window/budget for a range read.
struct CollectWindow {
    from_seq: u64,
    to_seq: u64,
    max_buffered_bytes: usize,
}

/// Where one record lives inside its segment file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
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
pub struct SegmentIndex {
    pub entries: Vec<IndexEntry>,
    pub valid_len: u64,
    pub stop: ScanStop,
}

impl SegmentIndex {
    /// Where to start reading so that the scanned region covers at least
    /// the newest `max_bytes` bytes of the valid prefix. Returns
    /// `(seq, offset)`; the region length is `valid_len - offset`, at
    /// most `max_bytes + stride + one record`.
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
pub struct TailRead {
    pub records: Vec<Record>,
    pub stop: ScanStop,
}

/// Read the newest records of one incarnation whose payloads fit in
/// `max_bytes` (the newest record is always included). Memory and work
/// stay bounded by `max_bytes + part size limit`, never by the total
/// recording: parts are bounded, the sparse index seeks directly to the
/// tail region, and only that region is re-validated and buffered.
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
    let mut stop = ScanStop::CleanEof;
    let mut expected_first: Option<u64> = None;
    let selected_len = selected.len();
    // Selected newest-first above; assemble oldest-first.
    for (n, (path, start)) in selected.into_iter().rev().enumerate() {
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
        let is_newest_selected = n + 1 == selected_len;
        if is_newest_selected {
            stop = result.outcome.stop;
        } else if !matches!(result.outcome.stop, ScanStop::CleanEof) {
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
    Ok(TailRead { records, stop })
}

/// One event returned by [`read_history`], carrying its durable cursor.
#[derive(Debug)]
pub struct HistoryEvent {
    pub cursor: JournalCursor,
    pub elapsed_ms: u64,
    pub kind: RecordKind,
    pub payload: Vec<u8>,
}

/// Result of a cross-incarnation history read.
#[derive(Debug)]
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
            elapsed_ms: record.elapsed_ms,
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

// ---------------------------------------------------------------------------
// Checkpoints (RecordKind::CheckpointRef)
//
// A checkpoint payload is a versioned, self-describing restore anchor
// (PLAN §5.3): it carries the terminal geometry, cursor, modes, and a
// side-effect-free *restore program* (styled scrollback + clear + styled
// screen + cursor report) that repaints equivalent state into a fresh
// engine. Retention is gated on checkpoints: everything older than the
// newest checkpoint's incarnation may be deleted, because replay can
// start at the checkpoint instead.
// ---------------------------------------------------------------------------

pub const CHECKPOINT_VERSION: u16 = 1;
const CHECKPOINT_MAGIC: &[u8; 4] = b"OJCK";

/// One checkpoint payload, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub rows: u16,
    pub cols: u16,
    /// 1-based (row, col) cursor position.
    pub cursor: (u16, u16),
    pub alt_screen: bool,
    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    /// Side-effect-free restore program (repaint escape stream).
    pub program: bytes::Bytes,
}

pub fn encode_checkpoint(checkpoint: &Checkpoint) -> bytes::Bytes {
    let mut out = Vec::with_capacity(19 + checkpoint.program.len());
    out.extend_from_slice(CHECKPOINT_MAGIC);
    out.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    out.extend_from_slice(&checkpoint.rows.to_le_bytes());
    out.extend_from_slice(&checkpoint.cols.to_le_bytes());
    out.extend_from_slice(&checkpoint.cursor.0.to_le_bytes());
    out.extend_from_slice(&checkpoint.cursor.1.to_le_bytes());
    let flags = u8::from(checkpoint.alt_screen)
        | u8::from(checkpoint.app_cursor_keys) << 1
        | u8::from(checkpoint.bracketed_paste) << 2;
    out.push(flags);
    out.extend_from_slice(&(checkpoint.program.len() as u32).to_le_bytes());
    out.extend_from_slice(&checkpoint.program);
    bytes::Bytes::from(out)
}

pub fn decode_checkpoint(payload: &[u8]) -> io::Result<Checkpoint> {
    fn take<'a>(payload: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
        if payload.len() < n {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated checkpoint payload",
            ));
        }
        let (head, tail) = payload.split_at(n);
        *payload = tail;
        Ok(head)
    }
    let mut rest = payload;
    if take(&mut rest, 4)? != CHECKPOINT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad checkpoint magic",
        ));
    }
    let version = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    if version != CHECKPOINT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported checkpoint version {version}"),
        ));
    }
    let rows = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let cols = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let cursor_row = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let cursor_col = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let flags = take(&mut rest, 1)?[0];
    let program_len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap()) as usize;
    let program = take(&mut rest, program_len)?;
    if !rest.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes after checkpoint program",
        ));
    }
    Ok(Checkpoint {
        rows,
        cols,
        cursor: (cursor_row, cursor_col),
        alt_screen: flags & 1 != 0,
        app_cursor_keys: flags & 2 != 0,
        bracketed_paste: flags & 4 != 0,
        program: bytes::Bytes::copy_from_slice(program),
    })
}

/// The incarnation holding the newest checkpoint record, if any. Scans
/// incarnations newest-first, header-only, stopping at torn tails — this
/// runs at retention time, never on the hot path.
pub fn latest_checkpoint_incarnation(session_dir: &Path) -> io::Result<Option<u64>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    for incarnation in list_incarnations(&journal_dir)?.into_iter().rev() {
        if incarnation_has_checkpoint(&journal_dir, incarnation)? {
            return Ok(Some(incarnation));
        }
    }
    Ok(None)
}

fn incarnation_has_checkpoint(journal_dir: &Path, incarnation: u64) -> io::Result<bool> {
    let mut parts: Vec<u64> = list_segments(journal_dir)?
        .into_iter()
        .filter(|&(inc, _)| inc == incarnation)
        .map(|(_, part)| part)
        .collect();
    parts.sort_unstable();
    for part in parts {
        let mut file = fs::File::open(segment_path(journal_dir, incarnation, part))?;
        let mut header = [0u8; HEADER_LEN];
        loop {
            match file.read_exact(&mut header) {
                Ok(()) => {}
                // Torn tail / EOF: nothing usable further in this part.
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err),
            }
            if &header[0..4] != RECORD_MAGIC {
                break;
            }
            let kind = u16::from_le_bytes(header[6..8].try_into().unwrap());
            let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap()) as u64;
            if kind == RecordKind::CheckpointRef as u16 {
                return Ok(true);
            }
            if file.seek_relative(payload_len as i64).is_err() {
                break;
            }
        }
    }
    Ok(false)
}

/// Checkpoint-gated retention (ADR-0002): delete sealed incarnations
/// below `min_incarnation`, but never past the newest checkpoint — the
/// checkpoint's restore program reconstructs the first exposed boundary,
/// and it lives in its own incarnation, so strictly older incarnations
/// are the only ones ever removed. With no checkpoint on record nothing
/// is deleted. Returns the deleted incarnation numbers.
pub fn retain_before(session_dir: &Path, min_incarnation: u64) -> io::Result<Vec<u64>> {
    let Some(gate) = latest_checkpoint_incarnation(session_dir)? else {
        return Ok(Vec::new());
    };
    retain_before_unchecked(session_dir, min_incarnation.min(gate))
}

/// Unchecked retention primitive: delete **all parts** of the sealed
/// incarnations below `min_incarnation`. The latest incarnation is never
/// deleted (it may be active). Returns the deleted incarnation numbers.
/// Cursors into removed incarnations fail loudly on read (see
/// [`read_history`]); they never alias newer bytes.
///
/// **Test/dev plumbing only.** Production retention goes through
/// [`retain_before`], which clamps the deletion horizon to the newest
/// checkpoint (ADR-0002: a sealed incarnation is deleted only once a
/// retained checkpoint can reconstruct the first exposed boundary).
pub fn retain_before_unchecked(session_dir: &Path, min_incarnation: u64) -> io::Result<Vec<u64>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = list_incarnations(&journal_dir)?;
    let latest = incarnations.last().copied();
    let mut deleted = Vec::new();
    for incarnation in incarnations {
        if incarnation >= min_incarnation || Some(incarnation) == latest {
            continue;
        }
        // Delete newest-part-first: a concurrent reader can then only
        // ever observe a prefix of the incarnation (cursors stay
        // truthful), never a hole in the middle.
        for (_, part) in list_segments(&journal_dir)?
            .into_iter()
            .filter(|&(inc, _)| inc == incarnation)
            .rev()
        {
            fs::remove_file(segment_path(&journal_dir, incarnation, part))?;
        }
        deleted.push(incarnation);
    }
    sync_dir(&journal_dir)?;
    Ok(deleted)
}

/// What a scan collects. All modes validate the consumed prefix fully
/// (headers, CRCs, continuity) — they differ only in what they retain.
enum ScanMode {
    /// Keep every record (small segments and tests only — recovery uses
    /// [`ScanMode::Stats`] so open-time memory does not scale with the
    /// recording).
    All,
    /// Keep only records inside the seq window, under a byte budget.
    Window(CollectWindow),
    /// Keep only per-record index entries (bounded tail/seek support).
    Index,
    /// Keep nothing but counters: O(1) memory regardless of segment size.
    Stats,
}

/// Everything one pass over a segment learned. `outcome.records` is only
/// populated in `All`/`Window` modes and `index` only in `Index` mode;
/// `record_count`/`last_seq` are tracked in every mode.
struct ScanResult {
    outcome: ScanOutcome,
    truncated: bool,
    index: Vec<IndexEntry>,
    record_count: u64,
    last_seq: Option<u64>,
}

/// Byte offset and expected sequence to resume a scan from (sparse-index
/// seek). The skipped prefix was validated when the segment part was
/// sealed — or is being written by the appender for the live part — and
/// is re-validated whenever a read needs it; CRC and continuity checks
/// apply from `offset` onward.
#[derive(Debug, Clone, Copy)]
struct ScanStart {
    offset: u64,
    seq: u64,
}

/// Sparse index granularity: one entry per at most this many bytes of
/// segment data. Keeps index memory O(part_size / stride) — 64 entries
/// for a default 64 MiB part — while bounding a tail read's seek region
/// to `max_bytes + stride`.
const SPARSE_INDEX_STRIDE_BYTES: u64 = 1024 * 1024;

fn scan_impl(
    path: &Path,
    mode: ScanMode,
    expected_first_seq: Option<u64>,
    start: Option<ScanStart>,
) -> io::Result<ScanResult> {
    let mut file = fs::File::open(path)?;
    let mut records = Vec::new();
    let mut index_entries = Vec::new();
    let mut buffered_bytes = 0usize;
    let mut truncated = false;
    let mut offset = 0u64;
    let mut expected_seq: Option<u64> = expected_first_seq;
    if let Some(start) = start {
        file.seek(SeekFrom::Start(start.offset))?;
        offset = start.offset;
        expected_seq = Some(start.seq);
    }
    let mut record_count = 0u64;
    let mut last_seq: Option<u64> = None;

    macro_rules! stop {
        ($reason:expr) => {
            return Ok(ScanResult {
                outcome: outcome(records, offset, $reason),
                truncated,
                index: index_entries,
                record_count,
                last_seq,
            })
        };
    }

    loop {
        let mut header = [0u8; HEADER_LEN];
        match read_exact_or_partial(&mut file, &mut header)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial => stop!(ScanStop::PartialTail),
            ReadPiece::Empty => {
                // On a live segment the file may have grown since the scan
                // started; `offset` remains the end of what was validated.
                stop!(ScanStop::CleanEof);
            }
        }

        if &header[..4] != RECORD_MAGIC {
            stop!(ScanStop::InvalidHeader);
        }
        if u16::from_le_bytes(header[4..6].try_into().unwrap()) != RECORD_VERSION {
            stop!(ScanStop::UnsupportedVersion);
        }
        let kind = match RecordKind::from_u16(u16::from_le_bytes(header[6..8].try_into().unwrap()))
        {
            Some(kind) => kind,
            None => stop!(ScanStop::UnknownKind),
        };
        let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap());
        if payload_len > MAX_PAYLOAD_LEN {
            stop!(ScanStop::OversizeLength);
        }

        let seq = u64::from_le_bytes(header[12..20].try_into().unwrap());
        if let ScanMode::Window(window) = &mode
            && seq > window.to_seq
        {
            // Past the requested window: the whole window was present and
            // contiguous. Nothing beyond it needs validation here.
            stop!(ScanStop::CleanEof);
        }

        let mut payload = vec![0u8; payload_len as usize];
        match read_exact_or_partial(&mut file, &mut payload)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial | ReadPiece::Empty => stop!(ScanStop::PartialTail),
        }

        let stored_crc = u32::from_le_bytes(header[32..36].try_into().unwrap());
        if crc32_two(&header[..32], &payload) != stored_crc {
            stop!(ScanStop::CrcMismatch);
        }

        if let Some(expected) = expected_seq
            && seq != expected
        {
            // A sequence gap (or a first record that is not the expected
            // first sequence of this stream) means an earlier record was
            // lost or the tail was aliased: stop here so recovery never
            // presents a silent hole.
            stop!(ScanStop::SequenceDiscontinuity);
        }
        expected_seq = Some(seq + 1);
        record_count += 1;
        last_seq = Some(seq);

        match &mode {
            ScanMode::All => {
                records.push(Record {
                    kind,
                    seq,
                    elapsed_ms: u64::from_le_bytes(header[20..28].try_into().unwrap()),
                    payload,
                });
            }
            ScanMode::Window(window) => {
                if seq >= window.from_seq {
                    let over_budget = !records.is_empty()
                        && buffered_bytes + payload.len() > window.max_buffered_bytes;
                    if over_budget {
                        truncated = true;
                        stop!(ScanStop::CleanEof);
                    }
                    buffered_bytes += payload.len();
                    records.push(Record {
                        kind,
                        seq,
                        elapsed_ms: u64::from_le_bytes(header[20..28].try_into().unwrap()),
                        payload,
                    });
                }
            }
            ScanMode::Index => {
                let due = index_entries
                    .last()
                    .is_none_or(|entry| offset >= entry.offset + SPARSE_INDEX_STRIDE_BYTES);
                if due {
                    index_entries.push(IndexEntry {
                        seq,
                        offset,
                        record_len: HEADER_LEN as u64 + payload_len as u64,
                    });
                }
            }
            ScanMode::Stats => {}
        }
        offset += (HEADER_LEN + payload_len as usize) as u64;
        file.seek(SeekFrom::Start(offset))?;
    }
}

fn outcome(records: Vec<Record>, valid_len: u64, stop: ScanStop) -> ScanOutcome {
    ScanOutcome {
        records,
        valid_len,
        stop,
    }
}

enum ReadPiece {
    Complete,
    Partial,
    Empty,
}

/// Distinguish a clean EOF at a record boundary (`Empty`) from a torn read
/// (`Partial`); both end the scan at the last valid record.
fn read_exact_or_partial(file: &mut fs::File, buf: &mut [u8]) -> io::Result<ReadPiece> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => {
                return Ok(if filled == 0 {
                    ReadPiece::Empty
                } else {
                    ReadPiece::Partial
                });
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(ReadPiece::Complete)
}

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
        let crc = crc32_two(&bytes[first_len as usize..header_end], &payload);
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
    fn retention_is_gated_on_the_newest_checkpoint() {
        let dir = test_session_dir("retention_gate");
        // Incarnation 1: plain output, no checkpoint.
        let (mut shadow, _, _) = ShadowJournal::open(&dir).unwrap();
        shadow
            .record_output(bytes::Bytes::from_static(b"inc1"))
            .unwrap();
        drop(shadow);
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
        drop(shadow);
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
}
