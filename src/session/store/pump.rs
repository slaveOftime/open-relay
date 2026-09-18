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
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{debug, info, trace, warn};

/// Upper bound for one coalesced pump frame: large enough that a paste burst
/// collapses into a handful of sends, small enough that interactive latency
/// stays snappy. Shared by all transports (was duplicated).
const MAX_COALESCED_CHUNK_BYTES: usize = 512 * 1024;

/// How often the pump checks for session completion. The IPC pump used
/// 100 ms and the WebSocket pump 200 ms; unified on the tighter value.
const COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
            },
            init,
        ))
    }

    /// Wait for the next output event. Cancel-safe; ends the stream with
    /// [`AttachEvent::Done`] or [`AttachEvent::Closed`].
    pub async fn next(&mut self) -> AttachEvent {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        loop {
            tokio::select! {
                biased;

                _ = self.completion.tick() => {
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
                        // Flush the final buffered bytes before reporting
                        // completion so the client sees the complete stream.
                        let data = match self
                            .store
                            .attach_subscribe_init(&self.id, Some(self.current_offset))
                            .await
                        {
                            Ok((chunks, new_end, _rx, _bpm, _ack)) => {
                                // The persisted view may lag the live
                                // broadcast (async journal appender); the
                                // pump cursor must never move backwards.
                                if new_end < self.current_offset {
                                    warn!(
                                        session_id = %self.id,
                                        persisted_end = new_end,
                                        cursor = self.current_offset,
                                        "attach pump completion flush: persisted end behind cursor"
                                    );
                                    Vec::new()
                                } else {
                                    self.current_offset = new_end;
                                    collect_chunk_bytes(&chunks)
                                }
                            }
                            Err(_) => Vec::new(),
                        };
                        info!(session_id = %self.id, ?exit_code,
                            final_offset = self.current_offset, "attach pump completed");
                        if data.is_empty() {
                            return AttachEvent::Done {
                                exit_code,
                                final_offset: self.current_offset,
                            };
                        }
                        self.pending.push_back(AttachEvent::Done {
                            exit_code,
                            final_offset: self.current_offset,
                        });
                        return AttachEvent::Chunk {
                            offset: self.current_offset - data.len() as u64,
                            data,
                        };
                    }
                }

                chunk = self.broadcast_rx.recv() => {
                    match chunk {
                        Ok(chunk) => {
                            let (offset, data) = self.coalesce(chunk);
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
                            if data.is_empty() {
                                continue; // return a queued mode event if any
                            }
                            return AttachEvent::Chunk { offset, data };
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            warn!(
                                session_id = %self.id,
                                skipped,
                                from_offset = self.current_offset,
                                "attach pump lagged behind broadcast output; resyncing from persisted stream"
                            );
                            match self
                                .store
                                .attach_subscribe_init(&self.id, Some(self.current_offset))
                                .await
                            {
                                Ok((chunks, new_end, rx, _bpm, _ack)) => {
                                    self.broadcast_rx = rx;
                                    // Never move the cursor backwards: the
                                    // persisted view can lag the live
                                    // broadcast we already forwarded.
                                    if new_end < self.current_offset {
                                        warn!(
                                            session_id = %self.id,
                                            persisted_end = new_end,
                                            cursor = self.current_offset,
                                            "attach pump resync: persisted end behind cursor"
                                        );
                                        continue;
                                    }
                                    let data = collect_chunk_bytes(&chunks);
                                    debug!(
                                        session_id = %self.id,
                                        resync_chunks = chunks.len(),
                                        resync_bytes = data.len(),
                                        from_offset = self.current_offset,
                                        to_offset = new_end,
                                        "attach pump replayed buffered output after lag"
                                    );
                                    let offset = self.current_offset;
                                    self.current_offset = new_end;
                                    if data.is_empty() {
                                        continue;
                                    }
                                    return AttachEvent::Chunk { offset, data };
                                }
                                Err(err) => {
                                    warn!(session_id = %self.id, error = err.message(&self.id),
                                        "attach pump resync failed");
                                    return AttachEvent::Closed;
                                }
                            }
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

    /// Coalesce every chunk the reader has already produced into one frame.
    /// A large paste echoes back as a burst of 64 KiB reads; forwarding them
    /// individually costs one framed write, one encode and one base64 pass
    /// each, which is what made pasting feel sluggish.
    fn coalesce(&mut self, mut chunk: SequencedChunk) -> (u64, Vec<u8>) {
        let mut coalesced: Option<Vec<u8>> = None;
        while coalesced.as_ref().map_or(chunk.bytes.len(), Vec::len) < MAX_COALESCED_CHUNK_BYTES {
            let Ok(next) = self.broadcast_rx.try_recv() else {
                break;
            };
            let buffer = coalesced.get_or_insert_with(|| chunk.bytes.as_ref().to_vec());
            buffer.extend_from_slice(&next.bytes);
        }
        let batch_len = coalesced.as_ref().map_or(chunk.bytes.len(), Vec::len);
        let offset = self.current_offset;
        self.current_offset += batch_len as u64;
        trace!(
            session_id = %self.id,
            filtered_bytes = batch_len,
            current_offset = self.current_offset,
            journal_cursor = ?chunk.cursor,
            "attach pump forwarded live PTY output"
        );
        let data = if batch_len > 0 {
            coalesced.unwrap_or_else(|| std::mem::take(&mut chunk.bytes).into())
        } else {
            Vec::new()
        };
        (offset, data)
    }
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
        let _ = rt.broadcast_tx.send(SequencedChunk {
            cursor: None,
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
