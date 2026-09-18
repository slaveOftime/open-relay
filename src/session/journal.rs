//! Segmented session journal: typed, checksummed records with stable
//! sequence numbers (PLAN.md §6.1, invariants I1/I3/I8).
//!
//! This module is M1 groundwork: it implements the record format, segment
//! writer/reader and torn-tail recovery. Nothing wires it into the live
//! session pipeline yet — `output.log` remains the canonical store until the
//! sequencer lands and the attach path reads exact committed ranges.
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
//! unlike the `output.log` offsets that `truncate_output_log` reuses today.
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

fn segment_path(journal_dir: &Path, incarnation: u64) -> PathBuf {
    journal_dir.join(format!("{SEGMENT_PREFIX}{incarnation:08}{SEGMENT_SUFFIX}"))
}

/// Newest incarnation number present in `journal_dir`, if any.
fn latest_incarnation(journal_dir: &Path) -> io::Result<Option<u64>> {
    let mut latest = None;
    for entry in fs::read_dir(journal_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(number) = name
            .strip_prefix(SEGMENT_PREFIX)
            .and_then(|rest| rest.strip_suffix(SEGMENT_SUFFIX))
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        latest = Some(latest.map_or(number, |current: u64| current.max(number)));
    }
    Ok(latest)
}

/// What recovery found in the previous incarnation's segment.
#[derive(Debug)]
pub struct RecoveryReport {
    /// Incarnation the recovered segment belongs to.
    pub incarnation: u64,
    /// Valid records recovered.
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

    let latest = latest_incarnation(&journal_dir)?;
    let mut report = None;
    if let Some(previous) = latest {
        let path = segment_path(&journal_dir, previous);
        let outcome = scan_segment(&path)?;
        let rewound = outcome.stop.may_rewind();
        if rewound {
            // A torn tail can only mean the previous incarnation died
            // mid-append; truncate so no reader ever sees it again.
            fs::OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_len(outcome.valid_len)?;
        }
        report = Some(RecoveryReport {
            incarnation: previous,
            records: outcome.records.len() as u64,
            last_seq: outcome.records.last().map(|record| record.seq),
            stop: outcome.stop,
            valid_len: outcome.valid_len,
            rewound,
        });
    }

    let incarnation = latest.unwrap_or(0) + 1;
    let writer = SegmentWriter::create(&segment_path(&journal_dir, incarnation))?;
    Ok(OpenedJournal {
        incarnation,
        report,
        writer,
    })
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
        let opened = open(session_dir)?;
        let (tx, rx) = std::sync::mpsc::sync_channel(DEFAULT_QUEUE_CAPACITY);
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        let queued_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_queued = std::sync::Arc::clone(&queued_bytes);
        std::thread::Builder::new()
            .name("journal-appender".to_string())
            .spawn(move || appender_loop(opened.writer, rx, ack_tx, worker_queued))?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalSubmitError {
    /// Message bound reached.
    QueueFull,
    /// Byte bound reached — the appender is stalled or the session is
    /// producing faster than storage can absorb.
    QueueBudgetExhausted,
    /// The appender thread is gone.
    AppenderDead,
}

impl std::fmt::Display for JournalSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "journal queue is full"),
            Self::QueueBudgetExhausted => write!(f, "journal queue byte budget exhausted"),
            Self::AppenderDead => write!(f, "journal appender is dead"),
        }
    }
}

fn appender_loop(
    mut writer: SegmentWriter,
    rx: std::sync::mpsc::Receiver<AppenderMsg>,
    ack_tx: std::sync::mpsc::Sender<JournalAck>,
    queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::atomic::Ordering;
    let mut expected_seq = 1u64;
    let mut last_written = 0u64;
    let mut dead: Option<String> = None;

    let fail = |ack_tx: &std::sync::mpsc::Sender<JournalAck>,
                dead: &mut Option<String>,
                reason: String| {
        *dead = Some(reason.clone());
        let _ = ack_tx.send(JournalAck::Failed(reason));
    };

    while let Ok(msg) = rx.recv() {
        match msg {
            AppenderMsg::Shutdown => break,
            AppenderMsg::Sync => {
                if let Some(reason) = &dead {
                    let _ = ack_tx.send(JournalAck::Failed(reason.clone()));
                } else if let Err(err) = writer.sync() {
                    fail(&ack_tx, &mut dead, format!("journal sync failed: {err}"));
                } else {
                    let _ = ack_tx.send(JournalAck::Durable(last_written));
                }
            }
            AppenderMsg::Record(event) => {
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
}

impl LifecycleCode {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Started),
            2 => Some(Self::Stopped),
            3 => Some(Self::Killed),
            4 => Some(Self::Failed),
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
        let (appender, incarnation, report, acks) = JournalAppender::spawn(session_dir)?;
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

/// Development-only switch for the M1 shadow journal: when set (and not
/// `0`), the PTY reader thread also journals output records. Off by
/// default until M3 makes the journal the canonical stream.
pub fn shadow_enabled() -> bool {
    std::env::var_os("OLY_JOURNAL").is_some_and(|value| !value.is_empty() && value != "0")
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
    let mut file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut records = Vec::new();
    let mut offset = 0u64;
    let mut expected_seq: Option<u64> = None;

    loop {
        let mut header = [0u8; HEADER_LEN];
        match read_exact_or_partial(&mut file, &mut header)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial => return Ok(outcome(records, offset, ScanStop::PartialTail)),
            ReadPiece::Empty => {
                debug_assert_eq!(offset, file_len, "EOF only at the real end");
                return Ok(outcome(records, offset, ScanStop::CleanEof));
            }
        }

        if &header[..4] != RECORD_MAGIC {
            return Ok(outcome(records, offset, ScanStop::InvalidHeader));
        }
        if u16::from_le_bytes(header[4..6].try_into().unwrap()) != RECORD_VERSION {
            return Ok(outcome(records, offset, ScanStop::UnsupportedVersion));
        }
        let kind = match RecordKind::from_u16(u16::from_le_bytes(header[6..8].try_into().unwrap()))
        {
            Some(kind) => kind,
            None => return Ok(outcome(records, offset, ScanStop::UnknownKind)),
        };
        let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap());
        if payload_len > MAX_PAYLOAD_LEN {
            return Ok(outcome(records, offset, ScanStop::OversizeLength));
        }

        let mut payload = vec![0u8; payload_len as usize];
        match read_exact_or_partial(&mut file, &mut payload)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial | ReadPiece::Empty => {
                return Ok(outcome(records, offset, ScanStop::PartialTail));
            }
        }

        let stored_crc = u32::from_le_bytes(header[32..36].try_into().unwrap());
        if crc32_two(&header[..32], &payload) != stored_crc {
            return Ok(outcome(records, offset, ScanStop::CrcMismatch));
        }

        let seq = u64::from_le_bytes(header[12..20].try_into().unwrap());
        if let Some(expected) = expected_seq
            && seq != expected
        {
            // A sequence gap means an earlier record was lost or the tail was
            // aliased: stop here so recovery never presents a silent hole.
            return Ok(outcome(records, offset, ScanStop::SequenceDiscontinuity));
        }
        expected_seq = Some(seq + 1);

        records.push(Record {
            kind,
            seq,
            elapsed_ms: u64::from_le_bytes(header[20..28].try_into().unwrap()),
            payload,
        });
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
        let outcome = scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000002.ojrn")).unwrap();
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
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001.ojrn");
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
        let segment = dir.join(JOURNAL_DIR_NAME).join("seg-00000001.ojrn");
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

        let outcome = scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000001.ojrn")).unwrap();
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

        let outcome = scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000001.ojrn")).unwrap();
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

        let outcome = scan_segment(&dir.join(JOURNAL_DIR_NAME).join("seg-00000001.ojrn")).unwrap();
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
