//! The transport-agnostic attach output pump (M3-2).
//!
//! Every live attach — CLI-over-IPC and WebSocket alike — used to carry its
//! own copy of the streaming state machine: follow the broadcast, coalesce
//! bursts, resync from the persisted stream on lag, flush the tail at
//! completion, and emit mode changes. [`AttachPump`] is that state machine,
//! exactly once. Transports are thin adapters that translate
//! [`AttachEvent`]s into their wire framing and feed client input back into
//! the store; all cursor/offset/mode/completion policy lives here so a fix
//! lands for every transport at once.
//!
//! `next()` is cancel-safe: internal state only mutates after awaits
//! complete, so transports may use it inside `tokio::select!` with their own
//! client-message arms.

use super::SessionStore;
use crate::session::runtime::{ModeSnapshot, SequencedChunk, SharedModes};
use crate::session::{SessionError, pty::collect_chunk_bytes};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::VecDeque, sync::Arc, time::Duration};
use tokio::sync::broadcast::{
    self,
    error::{RecvError, TryRecvError},
};
use tracing::{debug, info, trace, warn};

/// Upper bound for one coalesced pump frame: large enough that a paste burst
/// collapses into a handful of sends, small enough that interactive latency
/// stays snappy. Shared by all transports (was duplicated).
const MAX_COALESCED_CHUNK_BYTES: usize = 512 * 1024;

/// How often the pump checks for session completion. The IPC pump used
/// 100 ms and the WebSocket pump 200 ms; unified on the tighter value.
const COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Bounded resync window (M3-5, I7): a lagged attachment replays the
/// persisted stream in slices of this size instead of materializing the
/// whole lag in memory at once.
const RESYNC_WINDOW_BYTES: usize = 8 * 1024 * 1024;

/// The journal appender persists asynchronously, so a chunk can reach the
/// broadcast ring slightly before it is readable from disk. When a resync
/// gap or the completion tail is waiting on that flush, retry briefly
/// before giving up — loudly, never by skipping bytes (I2).
const FLUSH_RETRY_MAX: u8 = 50;
const FLUSH_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Credit-gating policy for one attached client (M5-1, I7; PLAN §7.4).
/// Credits are enforced, not advisory: the pump may run at most
/// `budget_bytes` ahead of the cursor the client reports as applied, and a
/// client that stops applying is disconnected loudly after
/// `stall_timeout` — never buffered into an unbounded queue.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CreditPolicy {
    /// Max in-flight bytes (pump cursor minus client-applied cursor). Both
    /// 1.0 clients ack every 1 MiB, so the budget is 4x that stride: one
    /// ack round-trip never gates a healthy client.
    pub budget_bytes: u64,
    /// How long the pump waits for credit before ending the stream loudly.
    pub stall_timeout: Duration,
    /// Poll cadence while waiting for credit. Acks land in a shared atomic
    /// updated by the transport's own select arm, so the pump only needs a
    /// cheap relaxed load per poll — no locks, no cross-task wakeups.
    pub poll_interval: Duration,
}

impl CreditPolicy {
    fn production() -> Self {
        Self {
            budget_bytes: 4 * 1024 * 1024,
            stall_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(25),
        }
    }
}

/// Whether an attachment's stream is credit-gated (M5-1).
///
/// Local IPC and WebSocket clients report applied-cursor acks, so their
/// streams are gated. Node-relayed subscriptions are the documented
/// exception: the relay carries one request per stream and cannot forward
/// mid-stream credits, so those pumps run uncredited (fail-open, same as
/// pre-M5-1) until direct remote attachment streams land (M5-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpCredit {
    Uncredited,
    Credited { attachment_id: u64 },
}

/// One server→client output event from an attach stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// Display-stream bytes starting at `offset` (the pump cursor at send
    /// time). Contiguous in stream order across events.
    Chunk { offset: u64, data: Vec<u8> },
    /// A terminal mode changed after the preceding chunk.
    Modes(ModeSnapshot),
    /// The session ended. `exit_code` is `Some` when the child exited on its
    /// own, `None` when it was killed/stopped externally. `final_offset` is
    /// the exact end-of-stream cursor so clients can verify they applied
    /// every byte (I2 at the completion boundary).
    Done {
        exit_code: Option<i32>,
        final_offset: u64,
    },
    /// The pump can no longer trust its state (store status lookup failed or
    /// the broadcast channel closed without a completion record). The
    /// transport should end the stream in whatever way its protocol allows.
    Closed,
}

/// The initial state handed to a fresh attach before live events start.
pub struct AttachInit {
    /// Snapshot bytes (fresh attach) or replayed stream bytes (resume from
    /// `from_offset`).
    pub data: Vec<u8>,
    /// Stream offset the pump's first live [`AttachEvent::Chunk`] continues
    /// from.
    pub end_offset: u64,
    /// Whether the session was still running when the pump attached.
    pub running: bool,
    /// Terminal modes at attach time.
    pub modes: ModeSnapshot,
    /// Journal incarnation the snapshot/cursor belongs to; 0 when the
    /// session predates journaling. A later resume must present the same
    /// incarnation (ADR-0004).
    pub incarnation: u64,
}

/// A live output stream for one attached client.
pub struct AttachPump {
    id: String,
    store: Arc<SessionStore>,
    broadcast_rx: broadcast::Receiver<SequencedChunk>,
    shared_modes: Option<Arc<SharedModes>>,
    last_modes: ModeSnapshot,
    current_offset: u64,
    completion: tokio::time::Interval,
    pending: VecDeque<AttachEvent>,
    /// Replaying persisted windows to close a gap (broadcast ring overflow
    /// or a held chunk that starts past the cursor).
    resync: bool,
    /// A broadcast chunk that starts past the cursor; held while resync
    /// windows fill the gap, then forwarded (trimmed if partially covered).
    held: Option<SequencedChunk>,
    /// Completion detected: draining the persisted tail before `Done`.
    completing: Option<Option<i32>>,
    /// Bounded waits for the async journal flush (see FLUSH_RETRY_MAX).
    flush_retries: u8,
    /// Credit gate (M5-1): the client's shared applied-cursor cell plus the
    /// enforcement policy. `None` for uncredited (node-relayed) streams.
    credit: Option<(Arc<AtomicU64>, CreditPolicy)>,
}

impl AttachPump {
    /// Attach to a session's output stream. `from_offset = None` attaches
    /// with a full-screen snapshot; `Some(offset)` resumes from a previously
    /// observed stream cursor (replay covers `offset..current end`).
    pub async fn subscribe(
        store: &Arc<SessionStore>,
        id: &str,
        from_offset: Option<u64>,
        incarnation: Option<u64>,
        credit: PumpCredit,
    ) -> Result<(Self, AttachInit), SessionError> {
        Self::subscribe_with_policy(
            store,
            id,
            from_offset,
            incarnation,
            credit,
            CreditPolicy::production(),
        )
        .await
    }

    pub(crate) async fn subscribe_with_policy(
        store: &Arc<SessionStore>,
        id: &str,
        from_offset: Option<u64>,
        incarnation: Option<u64>,
        credit: PumpCredit,
        credit_policy: CreditPolicy,
    ) -> Result<(Self, AttachInit), SessionError> {
        // Incarnation fencing (PLAN §7.3): a resume cursor is only valid for
        // the incarnation that issued it. Anything else is rejected up front
        // with a precise error instead of streaming from an inferred offset.
        if from_offset.is_some() {
            let current = store.journal_incarnation(id);
            let matches = matches!((incarnation, current), (Some(req), Some(cur)) if req == cur);
            if !matches {
                return Err(SessionError::StaleCursor {
                    requested: incarnation,
                    current,
                });
            }
        }
        let init_incarnation = store.journal_incarnation(id).unwrap_or(0);
        let (init, broadcast_rx) = match from_offset {
            None => {
                let (snapshot, end_offset, rx, modes) = store.attach_snapshot_init(id).await?;
                (
                    AttachInit {
                        data: snapshot,
                        end_offset,
                        running: store.is_running(id),
                        modes,
                        incarnation: init_incarnation,
                    },
                    rx,
                )
            }
            Some(offset) => {
                let (chunks, end, rx, modes) =
                    store.attach_subscribe_init(id, Some(offset)).await?;
                (
                    AttachInit {
                        data: collect_chunk_bytes(&chunks),
                        end_offset: end,
                        running: store.is_running(id),
                        modes,
                        incarnation: init_incarnation,
                    },
                    rx,
                )
            }
        };
        // Held for the lifetime of the stream so mode changes can be detected
        // with a relaxed atomic load per chunk instead of a session read lock.
        let shared_modes = store.shared_modes(id);
        let current_offset = init.end_offset;
        let last_modes = init.modes;
        let mut completion = tokio::time::interval(COMPLETION_POLL_INTERVAL);
        completion.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // M5-1: arm the credit gate. The cell starts at the init boundary:
        // the client is deemed to have applied everything up to
        // `end_offset` once it processes INIT (whose payload is separately
        // bounded), so only bytes streamed after attach count as in-flight.
        // Client acks report absolute cursors >= end_offset, so fetch_max
        // keeps the cell monotonic from here. A credited subscription
        // requires a registered attachment; a stale token is a loud error,
        // not a silent fallback to ungated streaming.
        let credit = match credit {
            PumpCredit::Uncredited => None,
            PumpCredit::Credited { attachment_id } => {
                let cell = store
                    .attachment_credit_cell(id, attachment_id)
                    .await
                    .ok_or(SessionError::StaleAttachment)?;
                cell.store(init.end_offset, Ordering::Relaxed);
                Some((cell, credit_policy))
            }
        };
        Ok((
            Self {
                id: id.to_string(),
                store: store.clone(),
                broadcast_rx,
                shared_modes,
                last_modes,
                current_offset,
                completion,
                pending: VecDeque::new(),
                resync: false,
                held: None,
                completing: None,
                flush_retries: 0,
                credit,
            },
            init,
        ))
    }

    /// Credit gate (M5-1, I7): wait until the client's applied cursor is
    /// within `budget_bytes` of the pump cursor. Bounded: a client that
    /// stops applying is ended loudly with [`AttachEvent::Closed`] after
    /// the stall timeout, never buffered without bound. Cancel-safe: no
    /// pump state mutates while waiting.
    async fn await_credit(&self) -> Option<AttachEvent> {
        let (cell, policy) = self.credit.as_ref()?;
        let in_flight = |applied: u64| self.current_offset.saturating_sub(applied);
        if in_flight(cell.load(Ordering::Relaxed)) <= policy.budget_bytes {
            return None;
        }
        let deadline = tokio::time::Instant::now() + policy.stall_timeout;
        loop {
            tokio::time::sleep(policy.poll_interval).await;
            let applied = cell.load(Ordering::Relaxed);
            if in_flight(applied) <= policy.budget_bytes {
                return None;
            }
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    session_id = %self.id,
                    cursor = self.current_offset,
                    applied,
                    budget = policy.budget_bytes,
                    "attach client stopped applying; closing stalled stream (credit gate)"
                );
                return Some(AttachEvent::Closed);
            }
        }
    }

    /// Wait for the next output event. Cancel-safe; ends the stream with
    /// [`AttachEvent::Done`] or [`AttachEvent::Closed`].
    pub async fn next(&mut self) -> AttachEvent {
        loop {
            // Credit gate first (M5-1, I7): before consuming any broadcast
            // chunk or advancing any offset. Placing the wait here — never
            // after `forward_chunk` — keeps `next()` cancel-safe: a
            // cancelled wait has consumed nothing, so no byte is lost.
            if let Some(closed) = self.await_credit().await {
                return closed;
            }
            if let Some(event) = self.pending.pop_front() {
                return event;
            }
            if self.resync {
                if let Some(event) = self.resync_step().await {
                    return event;
                }
                if self.resync {
                    continue; // bounded flush-wait retry
                }
            }
            // A held chunk whose gap has closed forwards immediately.
            if let Some(chunk) = self.held.take() {
                match self.forward_chunk(chunk) {
                    Forward::Chunk { offset, data } if !data.is_empty() => {
                        return AttachEvent::Chunk { offset, data };
                    }
                    _ => continue,
                }
            }
            tokio::select! {
                biased;

                _ = self.completion.tick() => {
                    if self.completing.is_some() {
                        continue; // already draining the tail
                    }
                    let (running, _output_closed, exit_code) =
                        match self.store.attach_stream_status(&self.id).await {
                            Ok(state) => state,
                            Err(err) => {
                                warn!(session_id = %self.id, error = err.message(&self.id),
                                    "attach pump status lookup failed");
                                return AttachEvent::Closed;
                            }
                        };
                    if !running {
                        // Flush the final persisted bytes in bounded windows
                        // before Done so the client sees the complete
                        // stream; Done fires when the drain catches up to
                        // the in-memory stream length.
                        self.completing = Some(exit_code);
                        self.resync = true;
                    }
                }

                chunk = self.broadcast_rx.recv() => {
                    match chunk {
                        Ok(chunk) => {
                            match self.forward_chunk(chunk) {
                                Forward::Chunk { offset, data } if !data.is_empty() => {
                                    return AttachEvent::Chunk { offset, data };
                                }
                                _ => continue, // fully covered, or gap -> resync loop
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            warn!(
                                session_id = %self.id,
                                skipped,
                                from_offset = self.current_offset,
                                "attach pump lagged behind broadcast output; resyncing from persisted stream"
                            );
                            self.resync = true;
                        }
                        Err(RecvError::Closed) => {
                            let exit_code = self.store.get_exit_code(&self.id);
                            info!(session_id = %self.id, ?exit_code, "attach pump broadcast channel closed");
                            // The completion flush normally reports Done before
                            // the channel closes; if shutdown raced us, the
                            // best-known cursor is what the client has seen.
                            return AttachEvent::Done {
                                exit_code,
                                final_offset: self.current_offset,
                            };
                        }
                    }
                }
            }
        }
    }

    /// The pump cursor: the stream offset the next [`AttachEvent::Chunk`]
    /// continues from.
    pub fn current_offset(&self) -> u64 {
        self.current_offset
    }

    /// Replay one bounded persisted window (I7); `None` means keep pumping
    /// (resync finished or a bounded flush-wait elapsed). Emits `Done` when
    /// the completion drain catches up to the in-memory stream length, and
    /// `Closed` — loudly, never skipping bytes — when a gap stays
    /// unpersisted past the retry budget.
    async fn resync_step(&mut self) -> Option<AttachEvent> {
        let from = self.current_offset;
        let data = match self
            .store
            .attach_resync_window(&self.id, from, RESYNC_WINDOW_BYTES)
            .await
        {
            Ok(data) => data,
            Err(err) => {
                warn!(session_id = %self.id, error = err.message(&self.id),
                    "attach pump resync failed");
                return Some(AttachEvent::Closed);
            }
        };
        if !data.is_empty() {
            let window_end = from + data.len() as u64;
            debug!(
                session_id = %self.id,
                resync_bytes = data.len(),
                from_offset = from,
                to_offset = window_end,
                "attach pump replayed a persisted window"
            );
            self.current_offset = window_end;
            self.flush_retries = 0;
            // A full window means more persisted data may follow; a short
            // window means the persisted stream is exhausted for now.
            self.resync = data.len() == RESYNC_WINDOW_BYTES;
            self.observe_modes();
            return Some(AttachEvent::Chunk { offset: from, data });
        }
        // Persisted stream exhausted at the cursor. The gap may still be
        // in flight in the journal appender (persistence lags the
        // broadcast by design): wait briefly, then fail loudly.
        let held_gap = self
            .held
            .as_ref()
            .is_some_and(|chunk| chunk.offset > self.current_offset);
        let completing_behind = self.completing.is_some()
            && self
                .store
                .attach_filtered_len(&self.id)
                .await
                .is_some_and(|len| len > self.current_offset);
        if held_gap || completing_behind {
            if self.flush_retries < FLUSH_RETRY_MAX {
                self.flush_retries += 1;
                tokio::time::sleep(FLUSH_RETRY_DELAY).await;
                return None; // stay in resync mode
            }
            warn!(
                session_id = %self.id,
                cursor = self.current_offset,
                held_offset = self.held.as_ref().map(|chunk| chunk.offset),
                "attach pump timed out waiting for the journal flush"
            );
            return Some(AttachEvent::Closed);
        }
        self.flush_retries = 0;
        if let Some(exit_code) = self.completing.take() {
            self.resync = false;
            info!(session_id = %self.id, ?exit_code,
                final_offset = self.current_offset, "attach pump completed");
            return Some(AttachEvent::Done {
                exit_code,
                final_offset: self.current_offset,
            });
        }
        self.resync = false;
        None
    }

    /// Offset-checked forwarding (I2): trims prefixes already covered by
    /// the pump cursor, holds chunks that start past it (the resync loop
    /// fills the gap), and coalesces contiguous bursts into one frame. A
    /// large paste echoes back as a burst of 64 KiB reads; forwarding them
    /// individually costs one framed write, one encode and one base64 pass
    /// each, which is what made pasting feel sluggish.
    fn forward_chunk(&mut self, mut chunk: SequencedChunk) -> Forward {
        if chunk.offset < self.current_offset {
            // Partially or fully covered by a persisted window we already
            // forwarded: trim the overlap, never double-send.
            let overlap = (self.current_offset - chunk.offset) as usize;
            if overlap >= chunk.bytes.len() {
                return Forward::Skip;
            }
            chunk.bytes = chunk.bytes.slice(overlap..);
            chunk.offset = self.current_offset;
        }
        if chunk.offset > self.current_offset {
            // A ring overflow dropped older chunks: replay the missing
            // persisted windows first, then this chunk aligns.
            warn!(
                session_id = %self.id,
                chunk_offset = chunk.offset,
                cursor = self.current_offset,
                "attach pump detected a broadcast gap; resyncing from persisted stream"
            );
            self.held = Some(chunk);
            self.resync = true;
            return Forward::Skip;
        }
        // Aligned at the cursor: coalesce contiguous buffered chunks.
        let offset = self.current_offset;
        let mut data: Vec<u8> = chunk.bytes.into();
        while data.len() < MAX_COALESCED_CHUNK_BYTES {
            match self.broadcast_rx.try_recv() {
                Ok(next) => {
                    let expected = self.current_offset + data.len() as u64;
                    if next.offset > expected {
                        self.held = Some(next);
                        self.resync = true;
                        break;
                    }
                    let skip = (expected - next.offset) as usize;
                    if skip < next.bytes.len() {
                        data.extend_from_slice(&next.bytes[skip..]);
                    }
                }
                Err(TryRecvError::Lagged(skipped)) => {
                    warn!(
                        session_id = %self.id,
                        skipped,
                        cursor = self.current_offset,
                        "attach pump lagged while coalescing; resyncing from persisted stream"
                    );
                    self.resync = true;
                    break;
                }
                Err(_) => break,
            }
        }
        self.current_offset += data.len() as u64;
        trace!(
            session_id = %self.id,
            filtered_bytes = data.len(),
            current_offset = self.current_offset,
            "attach pump forwarded live PTY output"
        );
        self.observe_modes();
        Forward::Chunk { offset, data }
    }

    /// Queue a mode event when the shared mode snapshot changed since the
    /// last forwarded chunk.
    fn observe_modes(&mut self) {
        if let Some(modes) = self.shared_modes.as_ref().map(|shared| shared.load())
            && modes != self.last_modes
        {
            debug!(
                session_id = %self.id,
                app_cursor_keys = modes.app_cursor_keys,
                bracketed_paste_mode = modes.bracketed_paste_mode,
                "attach pump terminal mode changed"
            );
            self.last_modes = modes;
            self.pending.push_back(AttachEvent::Modes(modes));
        }
    }
}

/// Result of offset-checked forwarding: an aligned (possibly coalesced)
/// chunk to emit, or nothing when the chunk was already covered / held for
/// a resync.
enum Forward {
    Chunk { offset: u64, data: Vec<u8> },
    Skip,
}

#[cfg(test)]
mod tests {
    use super::super::testsupport::{
        make_runtime, make_runtime_writable, make_test_db, store_with,
    };
    use super::*;
    use crate::session::{
        SessionStatus,
        runtime::{SequencedChunk, SessionRuntime},
    };
    use bytes::Bytes;
    use parking_lot::RwLock;
    use std::time::Duration;

    async fn running_store(
        id: &str,
        excerpt: &str,
    ) -> (Arc<SessionStore>, Arc<RwLock<SessionRuntime>>) {
        let rt = make_runtime(id, SessionStatus::Running, excerpt, None);
        let store = Arc::new(store_with(vec![Arc::clone(&rt)], make_test_db().await));
        (store, rt)
    }

    /// Feed one live chunk exactly as the PTY reader does: engine, filtered
    /// counter, then the sequenced broadcast.
    fn emit(rt: &Arc<RwLock<SessionRuntime>>, bytes: &[u8]) {
        let mut rt = rt.write();
        rt.feed_engine(bytes);
        rt.push_output(bytes, bytes.len());
        let offset = rt.filtered_stream_len() - bytes.len() as u64;
        let _ = rt.broadcast_tx.send(SequencedChunk {
            offset,
            bytes: Bytes::copy_from_slice(bytes),
        });
    }

    /// M5-1: a credited pump may run at most `budget_bytes` ahead of the
    /// client's applied cursor; output resumes the moment credits arrive.
    #[tokio::test]
    async fn credit_gate_holds_output_until_the_client_applies() {
        let (store, rt) = running_store("credit1", "base").await;
        let registration = store
            .attach_register(
                "credit1",
                crate::session::registry::AttachKind::Cli,
                crate::session::registry::ControlRequest::Controller,
                None,
            )
            .await
            .expect("register attachment");
        let policy = CreditPolicy {
            budget_bytes: 16,
            stall_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(5),
        };
        let (mut pump, init) = AttachPump::subscribe_with_policy(
            &store,
            "credit1",
            None,
            None,
            PumpCredit::Credited {
                attachment_id: registration.attachment_id,
            },
            policy,
        )
        .await
        .expect("subscribe");
        let cell = store
            .attachment_credit_cell("credit1", registration.attachment_id)
            .await
            .expect("credit cell");
        // The gate starts from the init boundary: bytes the client
        // receives in INIT are not in flight.
        assert_eq!(cell.load(Ordering::Relaxed), init.end_offset);

        // First chunk (32 bytes) flows: credits bound in-flight bytes to
        // budget + one frame (frames are never split), so the gate engages
        // on the NEXT chunk while 32 bytes are unapplied.
        emit(&rt, &[b'x'; 32]);
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(offset, init.end_offset);
                assert_eq!(data.len(), 32);
            }
            other => panic!("expected first chunk within budget+frame, got {other:?}"),
        }

        // 8 more bytes while the client has applied nothing: held.
        emit(&rt, &[b'z'; 8]);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), pump.next())
                .await
                .is_err(),
            "the credit gate must hold output beyond the budget"
        );

        // The client applies the first chunk: the held output flows.
        cell.store(init.end_offset + 32, Ordering::Relaxed);
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(offset, init.end_offset + 32);
                assert_eq!(data.len(), 8);
            }
            other => panic!("expected chunk after credit, got {other:?}"),
        }
    }

    /// M5-1: a client that never applies is disconnected loudly after the
    /// stall timeout — never buffered into an unbounded queue.
    #[tokio::test]
    async fn credit_gate_disconnects_a_stalled_client_loudly() {
        let (store, rt) = running_store("credit2", "base").await;
        let registration = store
            .attach_register(
                "credit2",
                crate::session::registry::AttachKind::Web,
                crate::session::registry::ControlRequest::Observer,
                None,
            )
            .await
            .expect("register attachment");
        let policy = CreditPolicy {
            budget_bytes: 8,
            stall_timeout: Duration::from_millis(200),
            poll_interval: Duration::from_millis(10),
        };
        let (mut pump, _init) = AttachPump::subscribe_with_policy(
            &store,
            "credit2",
            None,
            None,
            PumpCredit::Credited {
                attachment_id: registration.attachment_id,
            },
            policy,
        )
        .await
        .expect("subscribe");

        // The first 16 bytes flow (budget + one frame); with no acks the
        // next chunk trips the gate and the stall deadline ends the stream.
        emit(&rt, &[b'y'; 16]);
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { data, .. }) => assert_eq!(data.len(), 16),
            other => panic!("expected first chunk within budget+frame, got {other:?}"),
        }
        emit(&rt, &[b'q'; 8]);
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Closed) => {}
            other => panic!("a stalled client must be closed loudly, got {other:?}"),
        }
    }

    /// M5-1: a credited subscription names its attachment fencing token; a
    /// stale token fails loudly instead of degrading to ungated streaming.
    #[tokio::test]
    async fn credited_subscribe_rejects_a_stale_attachment_token() {
        let (store, _rt) = running_store("credit3", "base").await;
        let result = AttachPump::subscribe(
            &store,
            "credit3",
            None,
            None,
            PumpCredit::Credited {
                attachment_id: 4242,
            },
        )
        .await;
        assert!(
            matches!(result, Err(SessionError::StaleAttachment)),
            "stale attachment token must fail loudly, got: {}",
            result
                .err()
                .map(|e| e.message("credit3"))
                .unwrap_or_default()
        );
    }

    #[tokio::test]
    async fn pump_streams_chunks_modes_and_completion() {
        let (store, rt) = running_store("pump1", "hello\r\n").await;
        let (mut pump, init) =
            AttachPump::subscribe(&store, "pump1", None, None, PumpCredit::Uncredited)
                .await
                .expect("subscribe");
        assert!(init.running, "a running session must attach as running");

        emit(&rt, b"chunk-one");
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(offset, init.end_offset);
                assert_eq!(data, b"chunk-one");
            }
            other => panic!("expected chunk, got {other:?}"),
        }

        // A mode flip surfaces as a Modes event: DECCKM set via the engine.
        emit(&rt, b"\x1b[?1h");
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { data, .. }) => assert_eq!(data, b"\x1b[?1h"),
            other => panic!("expected chunk, got {other:?}"),
        }
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Modes(modes)) => assert!(modes.app_cursor_keys),
            other => panic!("expected modes, got {other:?}"),
        }

        // Session exit surfaces as Done (after the completion poll tick).
        {
            let mut rt = rt.write();
            rt.meta.status = SessionStatus::Stopped;
            rt.meta.exit_code = Some(7);
        }
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Done {
                exit_code,
                final_offset,
            }) => {
                assert_eq!(exit_code, Some(7));
                // chunk-one (9) + ESC[?1h (5) streamed after the 7-byte excerpt
                assert_eq!(final_offset, init.end_offset + 14);
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resume_with_stale_incarnation_is_rejected() {
        // No seeded excerpt: the fixture has no journal at all, so every
        // resume cursor is stale — both a wrong incarnation and a missing
        // one are refused instead of streaming from an inferred offset.
        let (store, _rt) = running_store("pump3", "").await;
        let result =
            AttachPump::subscribe(&store, "pump3", Some(0), Some(1), PumpCredit::Uncredited).await;
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("wrong incarnation must be rejected"),
        };
        assert!(matches!(
            err,
            SessionError::StaleCursor {
                requested: Some(1),
                current: None
            }
        ));
        let result =
            AttachPump::subscribe(&store, "pump3", Some(0), None, PumpCredit::Uncredited).await;
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("resume without incarnation must be rejected"),
        };
        assert!(matches!(
            err,
            SessionError::StaleCursor {
                requested: None,
                ..
            }
        ));
        // Fresh attaches carry no cursor and stay unaffected.
        assert!(
            AttachPump::subscribe(&store, "pump3", None, None, PumpCredit::Uncredited)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn pump_trims_overlapping_broadcast_chunks() {
        let (store, rt) = running_store("pump4", "").await;
        let (mut pump, _init) =
            AttachPump::subscribe(&store, "pump4", None, None, PumpCredit::Uncredited)
                .await
                .expect("subscribe");

        emit(&rt, b"abc");
        match pump.next().await {
            AttachEvent::Chunk { offset, data } => {
                assert_eq!((offset, data.as_slice()), (0, b"abc".as_slice()))
            }
            other => panic!("expected chunk, got {other:?}"),
        }

        // A duplicate of already-forwarded bytes (offset fully covered by
        // the cursor) must be skipped, never double-sent (I2).
        let _ = rt.read().broadcast_tx.send(SequencedChunk {
            offset: 0,
            bytes: Bytes::from_static(b"abc"),
        });
        // A partially covered chunk is trimmed to the uncovered suffix.
        let _ = rt.read().broadcast_tx.send(SequencedChunk {
            offset: 2,
            bytes: Bytes::from_static(b"cXY"),
        });
        match pump.next().await {
            AttachEvent::Chunk { offset, data } => {
                assert_eq!(
                    (offset, data.as_slice()),
                    (3, b"XY".as_slice()),
                    "overlap trimmed to exactly the uncovered suffix"
                )
            }
            other => panic!("expected trimmed chunk, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pump_resyncs_the_gap_before_a_far_ahead_chunk() {
        let (store, rt) = running_store("pump5", "").await;
        let (mut pump, _init) =
            AttachPump::subscribe(&store, "pump5", None, None, PumpCredit::Uncredited)
                .await
                .expect("subscribe");

        // Bytes 0..5 exist only in the persisted stream (the ring entries
        // carrying them were lost); the next broadcast chunk starts at 5.
        let dir = rt.read().dir.clone();
        super::super::testsupport::seed_journal_output(&dir, b"01234");
        let _ = rt.read().broadcast_tx.send(SequencedChunk {
            offset: 5,
            bytes: Bytes::from_static(b"56789"),
        });

        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(
                    (offset, data.as_slice()),
                    (0, b"01234".as_slice()),
                    "gap filled from the persisted stream first"
                )
            }
            other => panic!("expected resync chunk, got {other:?}"),
        }
        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(
                    (offset, data.as_slice()),
                    (5, b"56789".as_slice()),
                    "held chunk forwarded once the gap closed"
                )
            }
            other => panic!("expected held chunk, got {other:?}"),
        }
        assert_eq!(pump.current_offset(), 10, "stream contiguous end to end");
    }

    #[tokio::test]
    async fn pump_resyncs_from_persisted_stream_after_lag() {
        let (store, rt) = running_store("pump2", "").await;
        let (mut pump, init) =
            AttachPump::subscribe(&store, "pump2", None, None, PumpCredit::Uncredited)
                .await
                .expect("subscribe");
        assert_eq!(init.end_offset, 0);

        // Overflow the small broadcast ring (capacity 4 in the fixture) with
        // six chunks, each also persisted so the resync can replay them.
        {
            let dir = rt.read().dir.clone();
            super::super::testsupport::seed_journal_output(&dir, b"012345");
            for i in 0..6u8 {
                emit(&rt, &[b'0' + i]);
            }
        }

        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(offset, 0, "resync replays from the pump cursor");
                assert_eq!(data, b"012345", "no bytes lost or duplicated");
            }
            other => panic!("expected resync chunk, got {other:?}"),
        }
    }

    /// M3 exit stress (I2/I6/I7): ten mixed attachments — CLI and web,
    /// controller and observers — streaming through output bursts, ring
    /// overflows, resizes, a control takeover, and a detach/re-attach, then
    /// a clean session end. Every pump must observe exactly the same
    /// contiguous byte stream (its own suffix of the total), control
    /// gating must hold throughout, and every completion reports the same
    /// final cursor.
    #[tokio::test]
    async fn pump_stress_ten_mixed_clients_stay_consistent() {
        use crate::session::registry::{AttachKind, AttachRole, ControlRequest};

        struct Client {
            pump: AttachPump,
            attachment_id: u64,
            expect: u64,
            init_end: u64,
            received: Vec<u8>,
            done: bool,
        }

        async fn drain_once(client: &mut Client) {
            let event = tokio::time::timeout(Duration::from_secs(5), client.pump.next())
                .await
                .expect("pump event within timeout");
            match event {
                AttachEvent::Chunk { offset, data } => {
                    assert_eq!(
                        offset, client.expect,
                        "chunk must continue exactly at the client cursor (I2)"
                    );
                    client.received.extend_from_slice(&data);
                    client.expect += data.len() as u64;
                }
                AttachEvent::Modes(_) => {}
                AttachEvent::Done {
                    exit_code,
                    final_offset,
                } => {
                    assert_eq!(exit_code, Some(0));
                    assert_eq!(
                        final_offset, client.expect,
                        "final cursor matches everything the client applied (I2)"
                    );
                    client.done = true;
                }
                AttachEvent::Closed => panic!("pump closed unexpectedly"),
            }
        }

        /// Feed one chunk exactly as the reader does: journal first
        /// (durable before broadcast, ADR-0002), then broadcast.
        fn emit_persisted(
            journal: &mut crate::session::journal::ShadowJournal,
            seq: &mut u64,
            rt: &Arc<RwLock<SessionRuntime>>,
            bytes: &[u8],
        ) {
            journal
                .record_output(Bytes::copy_from_slice(bytes))
                .expect("record output");
            *seq += 1;
            super::super::testsupport::sync_journal(journal, *seq);
            emit(rt, bytes);
        }

        let (rt, mut writer_rx) = make_runtime_writable("stress1", SessionStatus::Running);
        let store = Arc::new(store_with(vec![Arc::clone(&rt)], make_test_db().await));
        let (mut journal, _, _) = crate::session::journal::ShadowJournal::open(&rt.read().dir)
            .expect("open fixture journal");
        let mut journal_seq = 0u64;

        // Ten attachments, alternating kinds: the first takes the control
        // lease, the rest join as observers (many observers, one
        // controller).
        let mut attachment_ids = Vec::new();
        for i in 0..10u8 {
            let reg = store
                .attach_register(
                    "stress1",
                    if i % 2 == 0 {
                        AttachKind::Cli
                    } else {
                        AttachKind::Web
                    },
                    ControlRequest::Controller,
                    None,
                )
                .await
                .expect("register attachment");
            attachment_ids.push(reg.attachment_id);
        }

        // Ten pumps, all fresh attaches (snapshot + live from the current
        // end). Half subscribe now, half after the first burst.
        let mut total: Vec<u8> = Vec::new();
        let mut clients: Vec<Client> = Vec::new();
        for (i, &attachment_id) in attachment_ids.iter().enumerate() {
            if i == 5 {
                for step in 0..5u8 {
                    let chunk = format!(
                        "pre{step:02}
"
                    )
                    .into_bytes();
                    total.extend_from_slice(&chunk);
                    emit_persisted(&mut journal, &mut journal_seq, &rt, &chunk);
                }
            }
            let (pump, init) =
                AttachPump::subscribe(&store, "stress1", None, None, PumpCredit::Uncredited)
                    .await
                    .expect("subscribe");
            clients.push(Client {
                pump,
                attachment_id,
                expect: init.end_offset,
                init_end: init.end_offset,
                received: Vec::new(),
                done: false,
            });
        }

        let mut controller = attachment_ids[0];
        for step in 0..120u32 {
            let mut chunk = format!(
                "line{step:03}
"
            )
            .into_bytes();
            if step == 30 {
                chunk.extend_from_slice(b"\x1b[?25l"); // mode flip mid-stream
            }
            total.extend_from_slice(&chunk);
            emit_persisted(&mut journal, &mut journal_seq, &rt, &chunk);

            // Group A (0..5) keeps up every step; group B (5..10) polls
            // only every 7 steps so its ring overflows and it resyncs from
            // the persisted stream in bounded windows (I7).
            let group_b_due = step % 7 == 0;
            for (i, client) in clients.iter_mut().enumerate() {
                if client.done || (i >= 5 && !group_b_due) {
                    continue;
                }
                drain_once(client).await;
            }

            match step {
                // Resize driven only by the controller.
                10 | 50 => {
                    store
                        .attach_resize("stress1", Some(controller), 30 + (step as u16 % 5), 100)
                        .await
                        .expect("controller resize");
                }
                // Takeover: attachment 3 seizes control mid-stream.
                40 => {
                    let outcome = store
                        .attach_acquire_control("stress1", attachment_ids[3])
                        .await
                        .expect("takeover");
                    assert_eq!(outcome.role, AttachRole::Controller);
                    controller = attachment_ids[3];
                    // The demoted controller is now gated (I6).
                    let err = store
                        .attach_input("stress1", Some(attachment_ids[0]), b"x", false)
                        .await
                        .expect_err("demoted controller must be gated");
                    assert!(matches!(err, SessionError::NotController));
                }
                // Controller input still lands.
                41 => {
                    store
                        .attach_input("stress1", Some(controller), b"k", false)
                        .await
                        .expect("controller input");
                    assert_eq!(writer_rx.try_recv().ok().as_deref(), Some(b"k".as_slice()));
                }
                // One client detaches mid-stream and a fresh client joins.
                60 => {
                    let leaving = clients.remove(4);
                    store
                        .attach_detach("stress1", leaving.attachment_id)
                        .await
                        .expect("detach");
                    drop(leaving);
                    let reg = store
                        .attach_register("stress1", AttachKind::Web, ControlRequest::Observer, None)
                        .await
                        .expect("late observer");
                    let (pump, init) = AttachPump::subscribe(
                        &store,
                        "stress1",
                        None,
                        None,
                        PumpCredit::Uncredited,
                    )
                    .await
                    .expect("late subscribe");
                    clients.push(Client {
                        pump,
                        attachment_id: reg.attachment_id,
                        expect: init.end_offset,
                        init_end: init.end_offset,
                        received: Vec::new(),
                        done: false,
                    });
                }
                _ => {}
            }
        }

        // Clean end: every client drains the tail and sees the same final
        // cursor.
        rt.write().mark_completed(SessionStatus::Stopped, Some(0));
        for _ in 0..500 {
            if clients.iter().all(|client| client.done) {
                break;
            }
            for client in &mut clients {
                if !client.done {
                    drain_once(client).await;
                }
            }
        }
        assert!(
            clients.iter().all(|client| client.done),
            "every client reached Done"
        );

        // Every client applied exactly its own suffix of the one true
        // stream — no gaps, no duplication, across lag/takeover/reconnect.
        for client in &clients {
            assert_eq!(
                client.received,
                total[client.init_end as usize..],
                "client starting at {} must observe the exact suffix",
                client.init_end
            );
        }
    }
}
