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
                let (snapshot, end_offset, rx, bracketed_paste_mode, app_cursor_keys) =
                    store.attach_snapshot_init(id).await?;
                (
                    AttachInit {
                        data: snapshot,
                        end_offset,
                        running: store.is_running(id),
                        modes: ModeSnapshot {
                            app_cursor_keys,
                            bracketed_paste_mode,
                        },
                        incarnation: init_incarnation,
                    },
                    rx,
                )
            }
            Some(offset) => {
                let (chunks, end, rx, bracketed_paste_mode, app_cursor_keys) =
                    store.attach_subscribe_init(id, Some(offset)).await?;
                (
                    AttachInit {
                        data: collect_chunk_bytes(&chunks),
                        end_offset: end,
                        running: store.is_running(id),
                        modes: ModeSnapshot {
                            app_cursor_keys,
                            bracketed_paste_mode,
                        },
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
            },
            init,
        ))
    }

    /// Wait for the next output event. Cancel-safe; ends the stream with
    /// [`AttachEvent::Done`] or [`AttachEvent::Closed`].
    pub async fn next(&mut self) -> AttachEvent {
        loop {
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
    use super::super::testsupport::{make_runtime, make_test_db, store_with};
    use super::*;
    use crate::session::{
        SessionStatus,
        persist::append_output_raw,
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
            cursor: None,
            offset,
            bytes: Bytes::copy_from_slice(bytes),
        });
    }

    #[tokio::test]
    async fn pump_streams_chunks_modes_and_completion() {
        let (store, rt) = running_store("pump1", "hello\r\n").await;
        let (mut pump, init) = AttachPump::subscribe(&store, "pump1", None, None)
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
        let (store, _rt) = running_store("pump3", "x").await;
        // The fixture runtime has no live journal, so every resume cursor is
        // stale: both a wrong incarnation and a missing one are refused
        // instead of streaming from an inferred offset.
        let result = AttachPump::subscribe(&store, "pump3", Some(0), Some(1)).await;
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
        let result = AttachPump::subscribe(&store, "pump3", Some(0), None).await;
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
            AttachPump::subscribe(&store, "pump3", None, None)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn pump_trims_overlapping_broadcast_chunks() {
        let (store, rt) = running_store("pump4", "").await;
        let (mut pump, _init) = AttachPump::subscribe(&store, "pump4", None, None)
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
            cursor: None,
            offset: 0,
            bytes: Bytes::from_static(b"abc"),
        });
        // A partially covered chunk is trimmed to the uncovered suffix.
        let _ = rt.read().broadcast_tx.send(SequencedChunk {
            cursor: None,
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
        let (mut pump, _init) = AttachPump::subscribe(&store, "pump5", None, None)
            .await
            .expect("subscribe");

        // Bytes 0..5 exist only in the persisted stream (the ring entries
        // carrying them were lost); the next broadcast chunk starts at 5.
        let dir = rt.read().dir.clone();
        append_output_raw(&dir, b"01234").unwrap();
        let _ = rt.read().broadcast_tx.send(SequencedChunk {
            cursor: None,
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
        let (mut pump, init) = AttachPump::subscribe(&store, "pump2", None, None)
            .await
            .expect("subscribe");
        assert_eq!(init.end_offset, 0);

        // Overflow the small broadcast ring (capacity 4 in the fixture) with
        // six chunks, each also persisted so the resync can replay them.
        let dir = rt.read().dir.clone();
        for i in 0..6u8 {
            let bytes = [b'0' + i];
            append_output_raw(&dir, &bytes).unwrap();
            emit(&rt, &bytes);
        }

        match tokio::time::timeout(Duration::from_secs(5), pump.next()).await {
            Ok(AttachEvent::Chunk { offset, data }) => {
                assert_eq!(offset, 0, "resync replays from the pump cursor");
                assert_eq!(data, b"012345", "no bytes lost or duplicated");
            }
            other => panic!("expected resync chunk, got {other:?}"),
        }
    }
}
