//! Attach-source abstraction for the WebSocket attach path.
//!
//! ARCHITECTURE.md says a federated attach is a local attach plus one
//! transport hop. `http/ws.rs` used to spell that out as two structurally
//! parallel copies of the same loop (`handle_ws_streaming` for local
//! attachments; `handle_ws_proxied_streaming` for node-proxied attaches).
//! This module is the single canonical source of both halves.
//!
//! Two constructors, **identical** `next_event()` semantics. Inbound
//! `ClientMessage`s are dispatched per arm inside `serve_attach` in
//! `http/ws.rs`, which is the single shared loop end-to-end.

use std::{result::Result as StdResult, sync::Arc, time::Instant};

use tokio::sync::mpsc;
#[cfg(test)]
use tracing::debug;

use crate::node::NodeRegistry;
use crate::protocol::{RpcRequest, RpcResponse};
use crate::session::{AttachEvent, AttachPump, SessionStore, resize::ResizeSubscriber};

use super::ws::{ServerMessage, WsModes};

// ---------------------------------------------------------------------------
// Init frame — the data the wire carries exactly once, sourced differently
// (a local snapshot + scrollback seed, or a relayed AttachStreamInit frame).
// ---------------------------------------------------------------------------

pub(crate) struct InitFrame {
    pub(crate) data: Vec<u8>,
    pub(crate) end_offset: u64,
    pub(crate) incarnation: u64,
    pub(crate) running: bool,
    pub(crate) modes: WsModes,
    pub(crate) attachment_id: u64,
}

impl InitFrame {
    pub(crate) fn into_message(self) -> ServerMessage {
        ServerMessage::Init {
            data: self.data,
            end_offset: self.end_offset,
            incarnation: self.incarnation,
            running: self.running,
            modes: self.modes,
            attachment_id: self.attachment_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Stream event — what `next_event()` yields after the initial frame.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum AttachStreamEvent {
    /// Display-stream chunk with the canonical stream offset of byte 0.
    Chunk { offset: u64, data: Vec<u8> },
    /// A terminal mode changed after the preceding chunk.
    Modes(WsModes),
    /// Another attached client resized the PTY (forwarded from peer states).
    Resized { rows: u16, cols: u16 },
    /// Session ended cleanly.
    Done {
        exit_code: Option<i32>,
        final_offset: u64,
    },
    /// Stream closed without a completion record (relay dropped, broadcast
    /// closed, store status unavailable). The transport should end the
    /// stream in whatever way its protocol allows.
    Closed,
}

// ---------------------------------------------------------------------------
// Attach source: local-vs-relayed transport unification.
// ---------------------------------------------------------------------------

// Variants carry buffer state for hot attach paths; boxing would add an
// allocation to every subscribe and is not worth the memory savings at
// this call site.
#[allow(clippy::large_enum_variant)]
pub(crate) enum AttachSource {
    Local(LocalSource),
    Relayed(RelayedSource),
}

impl AttachSource {
    /// Local attach: register the attachment, subscribe to the live attach
    /// pump, return the init frame ready to be sent on the wire.
    pub(crate) async fn local(
        store: Arc<SessionStore>,
        id: String,
        initial_rows: Option<u16>,
        initial_cols: Option<u16>,
    ) -> StdResult<LocalSourceOutput, InitError> {
        let viewport = match (initial_rows, initial_cols) {
            (Some(rows), Some(cols)) if rows > 0 && cols > 0 => Some((rows, cols)),
            _ => None,
        };
        // Web attaches are always controllers: no observer concept on the
        // web side.
        let registration = store
            .attach_register(
                &id,
                crate::session::registry::AttachKind::Web,
                crate::session::registry::AttachRole::Controller,
                viewport,
            )
            .await
            .map_err(|err| InitError::Abort(err.message(&id)))?;
        let attachment_id = registration.attachment_id;

        let (pump, init) = match AttachPump::subscribe(
            &store,
            &id,
            None,
            None,
            crate::session::PumpCredit::Credited { attachment_id },
        )
        .await
        {
            Ok(pair) => pair,
            Err(err) => {
                let _ = store.attach_detach(&id, attachment_id).await;
                return Err(InitError::Abort(err.message(&id)));
            }
        };
        let seed = match viewport {
            Some((rows, _)) => store.attach_scrollback_seed(&id, rows).await,
            None => None,
        };
        let data = super::ws::seed_web_init_data(seed, viewport.map(|(rows, _)| rows), init.data);

        let init_frame = InitFrame {
            data,
            end_offset: init.end_offset,
            incarnation: init.incarnation,
            running: init.running,
            modes: init.modes.into(),
            attachment_id,
        };

        let mut resize_sub = ResizeSubscriber::new(store.subscribe_resize(&id), id.clone());
        if let Some((rows, cols)) = viewport {
            resize_sub.mark_sent(rows, cols);
        }

        Ok(LocalSourceOutput {
            source: LocalSource {
                pump,
                resize_sub,
                attachment_id,
            },
            init_frame,
            tti_start: Instant::now(),
        })
    }

    /// Relayed attach: open `proxy_rpc_stream()` for the given owning node,
    /// pull the init frame eagerly (every caller expects it before the loop
    /// enters `select!`), and return the relay driver's internals.
    pub(crate) async fn relayed(
        registry: Arc<NodeRegistry>,
        id: String,
        node: String,
        initial_rows: Option<u16>,
        initial_cols: Option<u16>,
    ) -> StdResult<RelayedSourceOutput, InitError> {
        // Relayed web attaches are controllers too: no role token.
        let rpc = RpcRequest::AttachSubscribe {
            id: id.to_string(),
            from_byte_offset: None,
            incarnation: None,
            rows: initial_rows.filter(|rows| *rows > 0),
            cols: initial_cols.filter(|cols| *cols > 0),
            role: None,
            credited: true,
        };
        let (stream_rpc_id, mut stream_rx) = registry
            .proxy_rpc_stream(&node, &rpc)
            .await
            .map_err(|err| InitError::Abort(format!("failed to open proxy stream: {err}")))?;

        let init_frame = match stream_rx.recv().await {
            Some(Ok(RpcResponse::AttachStreamInit {
                data,
                end_offset,
                running,
                app_cursor_keys,
                bracketed_paste_mode,
                mouse_report,
                sgr_mouse,
                focus_events,
                incarnation,
                attachment_id,
                scrollback,
                ..
            })) => {
                let rows = initial_rows.filter(|rows| *rows > 0);
                let data = super::ws::seed_web_init_data(
                    (!scrollback.is_empty()).then_some(scrollback),
                    rows,
                    data,
                );
                InitFrame {
                    data,
                    end_offset,
                    incarnation,
                    running,
                    modes: WsModes {
                        app_cursor_keys,
                        bracketed_paste_mode,
                        mouse_report,
                        sgr_mouse,
                        focus_events,
                    },
                    attachment_id,
                }
            }
            Some(Ok(RpcResponse::Error { message })) => return Err(InitError::Abort(message)),
            Some(Ok(other)) => {
                return Err(InitError::Abort(format!(
                    "unexpected first proxy frame: {}",
                    other.label()
                )));
            }
            Some(Err(err)) => return Err(InitError::Abort(format!("proxy stream error: {err}"))),
            None => {
                return Err(InitError::Abort(
                    "proxy stream closed before init".to_string(),
                ));
            }
        };

        Ok(RelayedSourceOutput {
            source: RelayedSource {
                node,
                stream_rpc_id,
                stream_rx,
                init_sent: false,
            },
            init_frame,
            tti_start: Instant::now(),
        })
    }
}

// ---------------------------------------------------------------------------
// Local arm
// ---------------------------------------------------------------------------

pub(crate) struct LocalSource {
    pub(crate) pump: AttachPump,
    pub(crate) resize_sub: ResizeSubscriber,
    pub(crate) attachment_id: u64,
}

pub(crate) struct LocalSourceOutput {
    pub(crate) source: LocalSource,
    pub(crate) init_frame: InitFrame,
    pub(crate) tti_start: Instant,
}

impl LocalSource {
    pub(crate) async fn next_event(&mut self) -> Option<AttachStreamEvent> {
        match self.pump.next().await {
            AttachEvent::Chunk { offset, data } => Some(AttachStreamEvent::Chunk { offset, data }),
            AttachEvent::Modes(modes) => Some(AttachStreamEvent::Modes(modes.into())),
            AttachEvent::Done {
                exit_code,
                final_offset,
            } => Some(AttachStreamEvent::Done {
                exit_code,
                final_offset,
            }),
            AttachEvent::Closed => Some(AttachStreamEvent::Closed),
        }
    }
}

// ---------------------------------------------------------------------------
// Relayed arm
// ---------------------------------------------------------------------------

pub(crate) struct RelayedSource {
    pub(crate) node: String,
    pub(crate) stream_rpc_id: String,
    pub(crate) stream_rx: mpsc::Receiver<StdResult<RpcResponse, crate::error::AppError>>,
    pub(crate) init_sent: bool,
}

pub(crate) struct RelayedSourceOutput {
    pub(crate) source: RelayedSource,
    pub(crate) init_frame: InitFrame,
    pub(crate) tti_start: Instant,
}

impl RelayedSource {
    pub(crate) async fn next_event(&mut self) -> Option<AttachStreamEvent> {
        match self.stream_rx.recv().await {
            Some(Ok(resp)) => Some(resp.into_stream_event(&mut self.init_sent)),
            Some(Err(_err)) => Some(AttachStreamEvent::Closed),
            None => Some(AttachStreamEvent::Closed),
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound framing (relayed stream → wire)
// ---------------------------------------------------------------------------

impl RpcResponse {
    fn into_stream_event(self, _init_sent: &mut bool) -> AttachStreamEvent {
        match self {
            RpcResponse::AttachStreamInit { .. } => AttachStreamEvent::Closed,
            RpcResponse::AttachStreamChunk { offset, data } => {
                AttachStreamEvent::Chunk { offset, data }
            }
            RpcResponse::AttachModeChanged {
                app_cursor_keys,
                bracketed_paste_mode,
                mouse_report,
                sgr_mouse,
                focus_events,
            } => AttachStreamEvent::Modes(WsModes {
                app_cursor_keys,
                bracketed_paste_mode,
                mouse_report,
                sgr_mouse,
                focus_events,
            }),
            RpcResponse::AttachResized { rows, cols } => AttachStreamEvent::Resized { rows, cols },
            RpcResponse::AttachStreamDone {
                exit_code,
                final_offset,
            } => AttachStreamEvent::Done {
                exit_code,
                final_offset,
            },
            RpcResponse::Error { .. } => AttachStreamEvent::Closed,
            _ => AttachStreamEvent::Closed,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            RpcResponse::AttachStreamInit { .. } => "AttachStreamInit",
            RpcResponse::AttachStreamChunk { .. } => "AttachStreamChunk",
            RpcResponse::AttachModeChanged { .. } => "AttachModeChanged",
            RpcResponse::AttachResized { .. } => "AttachResized",
            RpcResponse::AttachStreamDone { .. } => "AttachStreamDone",
            RpcResponse::Error { .. } => "Error",
            _ => "Other",
        }
    }
}

// ---------------------------------------------------------------------------
// Initialisation error → mutates into a ServerMessage the caller's loop
// can forward before tearing the connection down.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(crate) enum InitError {
    Abort(String),
}

impl InitError {
    pub(crate) fn into_message(self) -> ServerMessage {
        match self {
            InitError::Abort(message) => ServerMessage::Error { message },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relayed_init_into_stream_event_is_closed() {
        let resp = RpcResponse::AttachStreamInit {
            data: b"snap".to_vec(),
            end_offset: 7,
            running: true,
            app_cursor_keys: true,
            bracketed_paste_mode: false,
            mouse_report: true,
            sgr_mouse: false,
            focus_events: true,
            scrollback: vec![],
            incarnation: 3,
            attachment_id: 42,
            role: "controller".to_string(),
        };
        let mut init_sent = true;
        let evt = resp.into_stream_event(&mut init_sent);
        assert!(matches!(evt, AttachStreamEvent::Closed));
    }

    #[test]
    fn chunk_maps_to_attachevent_chunk() {
        let resp = RpcResponse::AttachStreamChunk {
            offset: 5,
            data: b"hello".to_vec(),
        };
        let mut init_sent = true;
        let evt = resp.into_stream_event(&mut init_sent);
        match evt {
            AttachStreamEvent::Chunk { offset, data } => {
                assert_eq!(offset, 5);
                assert_eq!(data, b"hello");
            }
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn modes_map_to_attachevent_modes() {
        let resp = RpcResponse::AttachModeChanged {
            app_cursor_keys: true,
            bracketed_paste_mode: false,
            mouse_report: false,
            sgr_mouse: false,
            focus_events: true,
        };
        let mut init_sent = true;
        let evt = resp.into_stream_event(&mut init_sent);
        match evt {
            AttachStreamEvent::Modes(modes) => {
                assert!(modes.app_cursor_keys);
                assert!(modes.focus_events);
            }
            other => panic!("expected Modes, got {other:?}"),
        }
    }

    #[test]
    fn done_maps_to_attachevent_done() {
        let resp = RpcResponse::AttachStreamDone {
            exit_code: Some(0),
            final_offset: 99,
        };
        let mut init_sent = true;
        let evt = resp.into_stream_event(&mut init_sent);
        match evt {
            AttachStreamEvent::Done {
                exit_code,
                final_offset,
            } => {
                assert_eq!(exit_code, Some(0));
                assert_eq!(final_offset, 99);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn error_response_maps_to_closed() {
        let resp = RpcResponse::Error {
            message: "boom".to_string(),
        };
        let mut init_sent = true;
        let evt = resp.into_stream_event(&mut init_sent);
        assert!(matches!(evt, AttachStreamEvent::Closed));
    }

    #[test]
    fn init_error_into_message_is_error() {
        let msg = InitError::Abort("nope".to_string()).into_message();
        match msg {
            ServerMessage::Error { message } => assert_eq!(message, "nope"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn init_frame_into_message_carries_fields() {
        let frame = InitFrame {
            data: b"abc".to_vec(),
            end_offset: 12,
            incarnation: 1,
            running: true,
            modes: WsModes {
                app_cursor_keys: true,
                ..WsModes::default()
            },
            attachment_id: 7,
        };
        let msg = frame.into_message();
        match msg {
            ServerMessage::Init {
                data,
                end_offset,
                incarnation,
                running,
                attachment_id,
                ..
            } => {
                assert_eq!(data, b"abc");
                assert_eq!(end_offset, 12);
                assert_eq!(incarnation, 1);
                assert!(running);
                assert_eq!(attachment_id, 7);
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn attach_source_doc_anchor_compiles() {
        let _: fn(LocalSource, LocalSourceOutput, InitFrame, AttachStreamEvent) = |l, o, f, e| {
            drop(l);
            drop(o);
            drop(f);
            drop(e);
        };
        let _: fn(RelayedSource, RelayedSourceOutput) = |r, o| {
            drop(r);
            drop(o);
        };
        debug!("attach_source anchors exercised");
    }
}
