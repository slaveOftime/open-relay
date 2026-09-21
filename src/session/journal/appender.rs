//! Daemon-side record pipeline: sequencer core, appender loop and
//! rolling-segment writer (PLAN2 S1.5 step 7).
//!
//! Lifted from `session/journal/mod.rs`. The appender path
//! (`DEFAULT_CACHE_BUDGET_BYTES`, `DEFAULT_QUEUE_CAPACITY`,
//! `DEFAULT_QUEUE_BUDGET_BYTES`, `DEFAULT_SYNC_INTERVAL`, `SequencerCore`,
//! `JournalAck`, `AppenderMsg`, `JournalAppender`, `JournalSubmitError`,
//! `RollingSegmentWriter`, `appender_loop`) is byte-identical to the
//! previous inline definitions.

use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(test)]
use super::DEFAULT_SEGMENT_MAX_BYTES;
use super::{
    Crc32, HEADER_LEN, JournalCursor, OrderedEvent, RecordKind, RecoveryReport,
    SegmentManifestEntry, SegmentWriter, encode_record_header, open, segment_path, sync_dir,
};

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

    #[cfg(test)]
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

    #[cfg(test)]
    /// Highest sequence published so far, if any.
    pub fn head_seq(&self) -> Option<u64> {
        (self.next_seq > 1).then_some(self.next_seq - 1)
    }

    #[cfg(test)]
    /// Highest sequence contiguously readable from the journal.
    pub fn journal_seq(&self) -> u64 {
        self.journal_seq
    }

    #[cfg(test)]
    /// Highest sequence covered by a completed sync.
    pub fn durable_seq(&self) -> u64 {
        self.durable_seq
    }
    /// The incarnation this sequencer writes under. Journal cursors from
    /// other incarnations must be rejected on resume (ADR-0004).
    pub fn incarnation(&self) -> u64 {
        self.incarnation
    }

    #[cfg(test)]
    /// Number of events currently retained in the recent cache.
    pub fn cached_events(&self) -> usize {
        self.cache.len()
    }

    #[cfg(test)]
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

pub(crate) enum AppenderMsg {
    Record(Box<OrderedEvent>),
    Sync,
    #[cfg(test)]
    Shutdown,
}

/// The journal appender: sole owner of the active [`SegmentWriter`],
/// running on its own thread so sequencing and live publication never
/// perform disk I/O (PLAN.md §4.2). It validates contiguity, appends in
/// assigned order, group-syncs on request, and reports progress through
/// [`JournalAck`]s. The queue is bounded in both messages and bytes.
pub struct JournalAppender {
    pub tx: std::sync::mpsc::SyncSender<AppenderMsg>,
    pub queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub queue_budget_bytes: usize,
    #[cfg(test)]
    pub worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl JournalAppender {
    #[cfg(test)]
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

    #[cfg(test)]
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
        let worker = std::thread::Builder::new()
            .name("journal-appender".to_string())
            .spawn(move || appender_loop(writer, rx, ack_tx, worker_queued, sync_interval))?;
        #[cfg(not(test))]
        drop(worker); // only tests join the worker (via shutdown)
        Ok((
            Self {
                tx,
                queued_bytes,
                queue_budget_bytes: DEFAULT_QUEUE_BUDGET_BYTES,
                #[cfg(test)]
                worker: std::sync::Mutex::new(Some(worker)),
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

    #[cfg(test)]
    /// Stop the appender thread after draining queued records and wait for
    /// the final barrier (last group-sync + tail-part seal) to complete.
    pub fn shutdown(&self) {
        let _ = self.tx.send(AppenderMsg::Shutdown);
        if let Some(worker) = self.worker.lock().unwrap().take() {
            let _ = worker.join();
        }
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
pub(crate) struct RollingSegmentWriter {
    journal_dir: PathBuf,
    incarnation: u64,
    part: u64,
    writer: SegmentWriter,
    part_bytes: u64,
    max_part_bytes: u64,
    /// First sequence in the active part; `None` while the part is empty.
    part_first_seq: Option<u64>,
    /// Last sequence appended to the active part.
    part_last_seq: u64,
    /// Running CRC-32 of the active part's exact bytes (M3-6 manifest).
    part_crc: Crc32,
}

impl RollingSegmentWriter {
    pub(crate) fn new(
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
            part_first_seq: None,
            part_last_seq: 0,
            part_crc: Crc32::new(),
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
            self.seal_part()?;
            self.part += 1;
            self.writer = SegmentWriter::create(&segment_path(
                &self.journal_dir,
                self.incarnation,
                self.part,
            ))?;
            sync_dir(&self.journal_dir)?;
            self.part_bytes = 0;
        }
        let header = encode_record_header(kind, seq, elapsed_ms, payload)?;
        self.part_crc.update(&header);
        self.part_crc.update(payload);
        self.writer.write_encoded(&header, payload)?;
        if self.part_first_seq.is_none() {
            self.part_first_seq = Some(seq);
        }
        self.part_last_seq = seq;
        self.part_bytes += record_bytes;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.writer.sync()
    }

    /// Seal the active part: sync it durable, then append its entry to the
    /// incarnation manifest (M3-6). The manifest entry lands after the part
    /// itself is durable, so a manifest line always names a complete part;
    /// a crash between part sync and manifest append leaves a sealed part
    /// without an entry, which verification reports instead of guessing.
    /// Empty parts (no records) are never sealed.
    fn seal_part(&mut self) -> io::Result<()> {
        let Some(first_seq) = self.part_first_seq.take() else {
            return Ok(());
        };
        self.writer.sync()?;
        let entry = SegmentManifestEntry {
            incarnation: self.incarnation,
            part: self.part,
            first_seq,
            last_seq: self.part_last_seq,
            bytes: self.part_bytes,
            crc32: self.part_crc.finish(),
        };
        entry.append_to(&self.journal_dir)?;
        sync_dir(&self.journal_dir)?;
        self.part_crc = Crc32::new();
        Ok(())
    }
}

pub(crate) fn appender_loop(
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
        // fsync latency is the prime suspect when *everything* gets slow
        // at once (a stalling disk shows up here before anywhere else);
        // see PERFORMANCE.md.
        let sync_start = Instant::now();
        match writer.sync() {
            Ok(()) => {
                crate::metrics::observe("journal_sync", None, sync_start.elapsed());
                *last_durable = last_written;
                *sync_deadline = None;
                let _ = ack_tx.send(JournalAck::Durable(last_written));
            }
            Err(err) => {
                crate::metrics::count("journal_failures", Some("sync"));
                crate::metrics::observe("journal_sync", None, sync_start.elapsed());
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
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
            #[cfg(test)]
            Ok(AppenderMsg::Shutdown) => {
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
                let append_start = Instant::now();
                let append = writer.append_record(
                    event.kind,
                    event.cursor.seq,
                    event.elapsed_ms,
                    &event.payload,
                );
                crate::metrics::observe("journal_append", None, append_start.elapsed());
                match append {
                    Ok(()) => {
                        crate::metrics::add("journal_bytes", None, event.payload.len() as u64);
                        if last_written <= last_durable {
                            sync_deadline = Some(Instant::now() + sync_interval);
                        }
                        last_written = event.cursor.seq;
                        expected_seq += 1;
                        let _ = ack_tx.send(JournalAck::Journaled(event.cursor.seq));
                    }
                    Err(err) => {
                        crate::metrics::count("journal_failures", Some("append"));
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
    // Seal the tail part into the manifest (M3-6): a clean shutdown leaves
    // every part checksummed; only a crash leaves an unsealed tail.
    if dead.is_none()
        && let Err(err) = writer.seal_part()
    {
        fail(&ack_tx, &mut dead, format!("journal seal failed: {err}"));
    }
}
