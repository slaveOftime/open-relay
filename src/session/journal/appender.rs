//! Daemon-side record pipeline: sequencer core, appender loop and
//! rolling-segment writer.
//!
//! Lifted from `session/journal/mod.rs`; the appender path
//! (`DEFAULT_CACHE_BUDGET_BYTES`, `DEFAULT_QUEUE_CAPACITY`,
//! `DEFAULT_QUEUE_BUDGET_BYTES`, `DEFAULT_QUEUE_STALL_TIMEOUT`,
//! `DEFAULT_SYNC_INTERVAL`, `SequencerCore`, `JournalAck`, `AppenderMsg`,
//! `JournalAppender`, `JournalSubmitError`, `RollingSegmentWriter`,
//! `appender_loop`) keeps the previous inline definitions' behaviour.

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
/// Default message bound for one session's journal queue. The queue is a
/// staging buffer, not a failure boundary: a producer that finds it full
/// waits for room instead of degrading a healthy session. See
/// [`JournalAppender::try_submit`].
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
/// Default byte bound for one session's journal queue. Unlike the message
/// bound this one *is* a hard boundary: it caps how much session output a
/// stalled appender may hold before submission is rejected and the
/// session degrades explicitly, so memory cannot grow indefinitely.
pub const DEFAULT_QUEUE_BUDGET_BYTES: usize = 32 * 1024 * 1024;
/// How long a producer waits for a free queue slot before treating the
/// appender as wedged rather than merely busy.
///
/// The ingest path submits roughly one record per PTY read, which is
/// thousands per second for chatty output, so a fixed queue depth buys
/// only tens of milliseconds of slack — less than a single `fsync`
/// against a busy disk. Waiting that out is backpressure; turning it
/// into a failed session is what this deadline exists to prevent.
pub const DEFAULT_QUEUE_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Default group-sync cadence: the appender syncs at most this long after
/// the last unsynced append, bounding the crash-loss window without
/// per-record `fsync` latency on the ingest path (ADR-0002).
pub const DEFAULT_SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// The in-memory sequencing authority. Allocates monotonic `seq` and
/// monotonic `elapsed_ms` **before** an event is published, retains
/// recent events in a byte-bounded cache
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

    /// Persistence failed; the session is explicitly degraded. Subsequent
    /// `publish` calls still sequence — publication must not silently
    /// continue pretending durability is on track.
    pub fn degrade(&mut self, reason: impl Into<String>) {
        if self.degraded.is_none() {
            self.degraded = Some(reason.into());
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.is_some()
    }

    /// True only when a sync ack covers every published record, including
    /// the final PTY output and completion fact. Readers must not infer
    /// this from EOF alone: the appender writes asynchronously.
    pub fn is_fully_durable(&self) -> bool {
        !self.is_degraded() && self.durable_seq >= self.next_seq - 1
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

/// Room-available signal between a producer that found the queue full and
/// the appender thread that frees slots.
///
/// A waiting producer blocks on this rather than polling: timer
/// granularity (about 15 ms on Windows) is slower than the disk being
/// waited for, so polling would throttle ingest on the very path that
/// exists to absorb a slow disk.
#[derive(Default)]
pub struct QueueSpace {
    mutex: parking_lot::Mutex<()>,
    condvar: parking_lot::Condvar,
}

impl QueueSpace {
    /// Wake producers waiting for room, after a message is dequeued.
    pub(crate) fn released(&self) {
        let _guard = self.mutex.lock();
        self.condvar.notify_all();
    }

    /// Wait for a release while holding the lock, so a release that
    /// already happened cannot slip between the failed enqueue and the
    /// wait and cost the waiter a full timeout.
    fn wait(&self, guard: &mut parking_lot::MutexGuard<'_, ()>, timeout: std::time::Duration) {
        self.condvar.wait_for(guard, timeout);
    }
}

/// The journal appender: sole owner of the active [`SegmentWriter`],
/// running on its own thread so sequencing and live publication never
/// perform disk I/O. It validates contiguity, appends in assigned order,
/// group-syncs on request, and reports progress through
/// [`JournalAck`]s. The queue is bounded in both messages and bytes.
pub struct JournalAppender {
    pub tx: std::sync::mpsc::SyncSender<AppenderMsg>,
    pub queued_bytes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub queue_budget_bytes: usize,
    /// Wakes a producer that found the queue full.
    pub(crate) space: std::sync::Arc<QueueSpace>,
    /// How long [`Self::try_submit`] waits for room before degrading.
    pub(crate) stall_timeout: std::time::Duration,
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
        let space = std::sync::Arc::new(QueueSpace::default());
        let worker_space = std::sync::Arc::clone(&space);
        let worker = std::thread::Builder::new()
            .name("journal-appender".to_string())
            .spawn(move || {
                appender_loop(
                    writer,
                    rx,
                    ack_tx,
                    worker_queued,
                    worker_space,
                    sync_interval,
                )
            })?;
        #[cfg(not(test))]
        drop(worker); // only tests join the worker (via shutdown)
        Ok((
            Self {
                tx,
                queued_bytes,
                queue_budget_bytes: DEFAULT_QUEUE_BUDGET_BYTES,
                space,
                stall_timeout: DEFAULT_QUEUE_STALL_TIMEOUT,
                #[cfg(test)]
                worker: std::sync::Mutex::new(Some(worker)),
            },
            opened.incarnation,
            opened.report,
            ack_rx,
        ))
    }

    /// Queue an already-sequenced event for append.
    ///
    /// Fails without enqueueing when the byte budget is exhausted (the
    /// hard memory boundary) or the appender is gone. A full message
    /// queue is *not* a failure: the producer waits up to
    /// [`DEFAULT_QUEUE_STALL_TIMEOUT`] for a slot and only then reports
    /// [`JournalSubmitError::QueueFull`], so a momentary stall (an
    /// `fsync`, or a competing session's I/O) slows the session instead
    /// of failing it. Callers hold the session's sequencing lock, so the
    /// wait is what backpressures the PTY reader.
    pub fn try_submit(&self, event: OrderedEvent) -> Result<(), JournalSubmitError> {
        use std::sync::atomic::Ordering;
        let len = event.payload.len();
        let queued = self.queued_bytes.fetch_add(len, Ordering::Relaxed);
        if queued + len > self.queue_budget_bytes {
            self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
            return Err(JournalSubmitError::QueueBudgetExhausted);
        }
        let mut msg = AppenderMsg::Record(Box::new(event));
        // Fast path: room available, so no signalling is involved.
        match self.tx.try_send(msg) {
            Ok(()) => return Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                return Err(JournalSubmitError::AppenderDead);
            }
            Err(std::sync::mpsc::TrySendError::Full(returned)) => msg = returned,
        }
        // Slow path: hold the lock across the remaining attempts so a
        // release cannot be missed between an attempt and a wait.
        let mut guard = self.space.mutex.lock();
        let started = std::time::Instant::now();
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                    return Err(JournalSubmitError::AppenderDead);
                }
                Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                    let elapsed = started.elapsed();
                    if elapsed >= self.stall_timeout {
                        self.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                        return Err(JournalSubmitError::QueueFull);
                    }
                    self.space.wait(&mut guard, self.stall_timeout - elapsed);
                    msg = returned;
                }
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
    /// No queue slot came free within the stall deadline: the appender is
    /// wedged, not merely behind (see [`JournalAppender::try_submit`]).
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
    /// degrade point recorded in the core.
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
    /// Running CRC-32 of the active part's exact bytes (manifest).
    part_crc: Crc32,
    /// Test-only fault injection (constrained test storage): fail the first
    /// append after this many successful ones, so append-failure paths are
    /// exercised without filling the operator's disk.
    #[cfg(test)]
    pub(crate) fail_append_after_records: Option<u64>,
    /// Test-only fault injection: fail the next `sync`.
    #[cfg(test)]
    pub(crate) fail_next_sync: bool,
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
            #[cfg(test)]
            fail_append_after_records: None,
            #[cfg(test)]
            fail_next_sync: false,
        }
    }

    fn append_record(
        &mut self,
        kind: RecordKind,
        seq: u64,
        elapsed_ms: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        #[cfg(test)]
        if let Some(remaining) = self.fail_append_after_records.as_mut() {
            if *remaining == 0 {
                return Err(io::Error::other("injected append failure"));
            }
            *remaining -= 1;
        }
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
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_sync) {
            return Err(io::Error::other("injected sync failure"));
        }
        self.writer.sync()
    }

    /// Seal the active part: sync it durable, then append its entry to the
    /// incarnation manifest. The manifest entry lands after the part
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
    space: std::sync::Arc<QueueSpace>,
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
        if msg.is_ok() {
            // A dequeued message frees a slot: wake a producer waiting for
            // room before the append below can stall on the disk.
            space.released();
        }
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

    // Final barrier on graceful shutdown: an ack means the record is
    // durable on disk; stopping short of that would re-emit records
    // on the next replay.
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
    // Seal the tail part into the manifest: a clean shutdown leaves
    // every part checksummed; only a crash leaves an unsealed tail.
    if dead.is_none()
        && let Err(err) = writer.seal_part()
    {
        fail(&ack_tx, &mut dead, format!("journal seal failed: {err}"));
    }
}

#[cfg(test)]
mod resume_hint_tests {
    use super::*;

    #[test]
    fn final_output_requires_sync_before_derived_metadata_is_read() {
        let mut core = SequencerCore::new(1);
        core.publish(
            RecordKind::Output,
            bytes::Bytes::from_static(b"resume hint"),
        );
        assert!(!core.is_fully_durable());
        core.note_journaled(1);
        assert!(!core.is_fully_durable());
        core.note_durable(1);
        assert!(core.is_fully_durable());
        core.publish(RecordKind::Lifecycle, bytes::Bytes::from_static(b"ended"));
        assert!(!core.is_fully_durable());
        core.note_durable(2);
        assert!(core.is_fully_durable());
        core.degrade("disk error");
        assert!(!core.is_fully_durable());
    }
}
