use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::IntoResponse,
};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};

use tracing::{debug, error, info, trace, warn};

use crate::protocol::{RpcRequest, RpcResponse};
use crate::session::registry::{AttachKind, ControlRequest};
use crate::session::resize::ResizeSubscriber;
use crate::session::{AttachEvent, AttachPump, SessionError};

use super::AppState;

#[derive(Debug, Deserialize)]
pub struct AttachParams {
    pub node: Option<String>,
    /// Initial terminal width (cols) reported by the browser xterm instance.
    pub cols: Option<u16>,
    /// Initial terminal height (rows) reported by the browser xterm instance.
    pub rows: Option<u16>,
    /// Requested control role: observer | controller (default) | takeover.
    pub role: Option<String>,
}

// ---------------------------------------------------------------------------
// Protocol types — unified for both local and proxied sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// Initial terminal snapshot. `data` contains filtered stream bytes
    /// covering the stream up to `end_offset` in journal `incarnation`.
    Init {
        data: Vec<u8>,
        /// Stream offset immediately after the snapshot (C in ADR-0004).
        end_offset: u64,
        /// Journal incarnation the snapshot/cursor belongs to (0 = legacy).
        incarnation: u64,
        /// Whether the session was still running at attach time.
        running: bool,
        #[serde(rename = "appCursorKeys")]
        app_cursor_keys: bool,
        #[serde(rename = "bracketedPasteMode")]
        bracketed_paste_mode: bool,
        /// This attachment's fencing token and granted role (M3-4).
        attachment_id: u64,
        role: &'static str,
    },
    /// Incremental PTY output chunk; `offset` is the stream offset of the
    /// first byte so clients can verify contiguity (I2).
    Data {
        offset: u64,
        data: Vec<u8>,
    },
    /// Terminal mode changed mid-stream.
    ModeChanged {
        #[serde(rename = "appCursorKeys")]
        app_cursor_keys: bool,
        #[serde(rename = "bracketedPasteMode")]
        bracketed_paste_mode: bool,
    },
    /// Another attached client resized the PTY.
    Resized {
        rows: u16,
        cols: u16,
    },
    /// Session ended. `final_offset` is the end-of-stream cursor (I2
    /// completion check); 0 when unknown (proxy errors).
    SessionEnded {
        exit_code: Option<i32>,
        final_offset: u64,
    },
    Error {
        message: String,
    },
    /// Control handoff notice: this attachment's role after the change.
    Control {
        role: &'static str,
    },
    Pong,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Input {
        data: String,
        #[serde(rename = "waitForChange")]
        wait_for_change: bool,
    },
    Busy,
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Take over the control lease from an observer position.
    AcquireControl,
    /// Applied-cursor credit (M3-5, I7): highest stream offset the client
    /// has rendered.
    Ack {
        offset: u64,
    },
    Detach,
    Ping,
}

const WS_FRAME_INIT: u8 = 1;
const WS_FRAME_DATA: u8 = 2;
const WS_FRAME_MODE_CHANGED: u8 = 3;
const WS_FRAME_RESIZED: u8 = 4;
const WS_FRAME_SESSION_ENDED: u8 = 5;
const WS_FRAME_ERROR: u8 = 6;
const WS_FRAME_PONG: u8 = 7;
const WS_FRAME_CONTROL: u8 = 8;
const WS_FLAG_APP_CURSOR_KEYS: u8 = 1 << 0;
const WS_FLAG_BRACKETED_PASTE_MODE: u8 = 1 << 1;

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn attach_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<AttachParams>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // ADR-0007 (M5-4): browser cross-site WebSockets must not ride the
    // ambient auth cookie — Origin host must match the request Host.
    if !crate::http::auth::ws_origin_allowed(&headers) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            "websocket origin rejected",
        )
            .into_response();
    }
    debug!(session_id = %id, "WebSocket upgrade requested");
    ws.on_upgrade(move |socket| async move {
        let panic_session_id = id.clone();
        let panic_node = params.node.clone();
        let result = std::panic::AssertUnwindSafe(handle_ws(
            socket,
            state,
            id,
            params.node,
            AttachConnectionParams {
                initial_rows: params.rows,
                initial_cols: params.cols,
                role: params.role,
                headers,
            },
        ))
        .catch_unwind()
        .await;

        if let Err(payload) = result {
            error!(
                session_id = %panic_session_id,
                node = ?panic_node,
                panic = %panic_payload_message(payload.as_ref()),
                "attach WebSocket handler panicked"
            );
        }
    })
    .into_response()
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return message;
    }

    if let Some(message) = payload.downcast_ref::<String>() {
        return message.as_str();
    }

    "non-string panic payload"
}

fn mode_flags(app_cursor_keys: bool, bracketed_paste_mode: bool) -> u8 {
    let mut flags = 0;
    if app_cursor_keys {
        flags |= WS_FLAG_APP_CURSOR_KEYS;
    }
    if bracketed_paste_mode {
        flags |= WS_FLAG_BRACKETED_PASTE_MODE;
    }
    flags
}

/// Encode a server message into its binary wire frame. Pure so the layout
/// is unit-testable against the TypeScript decoder (web/src/api/client.ts).
fn encode_server_message(msg: &ServerMessage) -> Vec<u8> {
    match msg {
        ServerMessage::Init {
            data,
            end_offset,
            incarnation,
            running,
            app_cursor_keys,
            bracketed_paste_mode,
            attachment_id,
            role,
        } => {
            let mut payload = Vec::with_capacity(28 + data.len());
            payload.push(WS_FRAME_INIT);
            payload.push(mode_flags(*app_cursor_keys, *bracketed_paste_mode));
            payload.extend_from_slice(&end_offset.to_be_bytes());
            payload.extend_from_slice(&incarnation.to_be_bytes());
            payload.push(u8::from(*running));
            payload.extend_from_slice(&attachment_id.to_be_bytes());
            payload.push(u8::from(*role == "controller"));
            payload.extend_from_slice(data);
            payload
        }
        ServerMessage::Data { offset, data } => {
            let mut payload = Vec::with_capacity(9 + data.len());
            payload.push(WS_FRAME_DATA);
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.extend_from_slice(data);
            payload
        }
        ServerMessage::ModeChanged {
            app_cursor_keys,
            bracketed_paste_mode,
        } => vec![
            WS_FRAME_MODE_CHANGED,
            mode_flags(*app_cursor_keys, *bracketed_paste_mode),
        ],
        ServerMessage::Resized { rows, cols } => {
            let mut payload = Vec::with_capacity(5);
            payload.push(WS_FRAME_RESIZED);
            payload.extend_from_slice(&rows.to_be_bytes());
            payload.extend_from_slice(&cols.to_be_bytes());
            payload
        }
        ServerMessage::SessionEnded {
            exit_code,
            final_offset,
        } => {
            let mut payload = Vec::with_capacity(14);
            payload.push(WS_FRAME_SESSION_ENDED);
            match exit_code {
                Some(code) => {
                    payload.push(1);
                    payload.extend_from_slice(&code.to_be_bytes());
                }
                None => payload.push(0),
            }
            payload.extend_from_slice(&final_offset.to_be_bytes());
            payload
        }
        ServerMessage::Error { message } => {
            let mut payload = Vec::with_capacity(1 + message.len());
            payload.push(WS_FRAME_ERROR);
            payload.extend_from_slice(message.as_bytes());
            payload
        }
        ServerMessage::Control { role } => vec![WS_FRAME_CONTROL, u8::from(*role == "controller")],
        ServerMessage::Pong => vec![WS_FRAME_PONG],
    }
}

async fn send_server_message(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    socket
        .send(Message::Binary(encode_server_message(msg).into()))
        .await
        .is_ok()
}

/// Per-connection attach parameters bundled to keep handler signatures
/// under clippy's argument limit.
struct AttachConnectionParams {
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
    role: Option<String>,
    headers: axum::http::HeaderMap,
}

async fn handle_ws(
    socket: WebSocket,
    state: AppState,
    id: String,
    node: Option<String>,
    params: AttachConnectionParams,
) {
    debug!(session_id = %id, node = ?node, "WebSocket connected");

    if let Some(node_name) = node {
        handle_ws_proxied_streaming(socket, state, id, node_name, params).await;
        return;
    }

    handle_ws_streaming(socket, state, id, params).await;
}

// ---------------------------------------------------------------------------
// Streaming attach (local sessions) — unified with IPC protocol
// ---------------------------------------------------------------------------

async fn handle_ws_streaming(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    params: AttachConnectionParams,
) {
    let AttachConnectionParams {
        initial_rows,
        initial_cols,
        role,
        headers,
    } = params;
    // M3-4: register the attachment (control lease + viewport) before the
    // snapshot, so a controller's authorized initial geometry is applied
    // through the sequencer and already reflected in the init.
    let request = match ControlRequest::parse(role.as_deref()) {
        Ok(request) => request,
        Err(message) => {
            let _ = send_server_message(&mut socket, &ServerMessage::Error { message }).await;
            return;
        }
    };
    let viewport = match (initial_rows, initial_cols) {
        (Some(rows), Some(cols)) if rows > 0 && cols > 0 => Some((rows, cols)),
        _ => None,
    };
    let registration = match state
        .store
        .attach_register(&id, AttachKind::Web, request, viewport)
        .await
    {
        Ok(registration) => registration,
        Err(err) => {
            let _ = send_server_message(
                &mut socket,
                &ServerMessage::Error {
                    message: err.message(&id),
                },
            )
            .await;
            return;
        }
    };
    let attachment_id = registration.attachment_id;

    // M5-1: local WebSocket clients ack applied cursors every 1 MiB, so
    // their stream is credit-gated.
    let (mut pump, init) = match AttachPump::subscribe(
        &state.store,
        &id,
        None,
        None,
        crate::session::PumpCredit::Credited { attachment_id },
    )
    .await
    {
        Ok(pair) => pair,
        Err(err) => {
            let _ = state.store.attach_detach(&id, attachment_id).await;
            warn!(session_id = %id, error = err.message(&id), "local WebSocket stream init failed");
            let _ = send_server_message(
                &mut socket,
                &ServerMessage::Error {
                    message: err.message(&id),
                },
            )
            .await;
            return;
        }
    };

    let init_msg = ServerMessage::Init {
        data: init.data,
        end_offset: init.end_offset,
        incarnation: init.incarnation,
        running: init.running,
        app_cursor_keys: init.modes.app_cursor_keys,
        bracketed_paste_mode: init.modes.bracketed_paste_mode,
        attachment_id,
        role: registration.role.as_str(),
    };
    if !send_server_message(&mut socket, &init_msg).await {
        debug!(session_id = %id, "local WebSocket closed before init frame could be sent");
        return;
    }

    // Control-handoff notices for this session.
    let mut control_rx = state.store.subscribe_control(&id);

    // ADR-0007 (M5-4): logout/revocation closes live control streams not
    // just future requests — watch the revocation epoch and re-validate the
    // connection's token.
    let revoke_token = crate::http::auth::extract_request_token_parts(&headers, None);
    let mut revocation_rx = state.auth.as_ref().map(|auth| auth.revocation_watch());

    // Subscribe to resize broadcasts so we can notify this client when
    // another attached client changes the PTY size.
    let mut resize_sub = ResizeSubscriber::new(state.store.subscribe_resize(&id), id.clone());

    debug!(
        session_id = %id,
        snapshot_bytes = init_msg_data_len(&init_msg),
        end_offset = init.end_offset,
        app_cursor_keys = init.modes.app_cursor_keys,
        bracketed_paste_mode = init.modes.bracketed_paste_mode,
        "local WebSocket stream initialized"
    );

    loop {
        // Session revocation check armed only while auth is enabled and the
        // connection carried a token.
        let revoked = async {
            match (&mut revocation_rx, &revoke_token) {
                (Some(rx), Some(_)) => {
                    let _ = rx.changed().await;
                }
                _ => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(revoked);
        tokio::select! {
            biased;

            _ = &mut revoked => {
                if let (Some(auth), Some(token)) = (&state.auth, &revoke_token)
                    && !auth.validate_token(token).await
                {
                    info!(session_id = %id, "WebSocket closed — session token revoked");
                    let _ = send_server_message(
                        &mut socket,
                        &ServerMessage::Error {
                            message: "session revoked (logout)".to_string(),
                        },
                    )
                    .await;
                    return;
                }
            }

            // Session output: the shared attach pump (M3-2) owns follow /
            // coalesce / lag resync / completion flush / mode tracking.
            event = pump.next() => {
                match event {
                    AttachEvent::Chunk { offset, data } => {
                        if !send_server_message(&mut socket, &ServerMessage::Data { offset, data }).await {
                            let _ = state.store.attach_detach(&id, attachment_id).await;
                            return;
                        }
                    }
                    AttachEvent::Modes(modes) => {
                        if !send_server_message(&mut socket, &ServerMessage::ModeChanged {
                            app_cursor_keys: modes.app_cursor_keys,
                            bracketed_paste_mode: modes.bracketed_paste_mode,
                        }).await {
                            let _ = state.store.attach_detach(&id, attachment_id).await;
                            return;
                        }
                    }
                    AttachEvent::Done {
                        exit_code,
                        final_offset,
                    } => {
                        info!(session_id = %id, ?exit_code, final_offset, "WS session ended");
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code, final_offset }).await;
                        let _ = state.store.attach_detach(&id, attachment_id).await;
                        return;
                    }
                    AttachEvent::Closed => {
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded {
                            exit_code: None,
                            final_offset: pump.current_offset(),
                        }).await;
                        let _ = state.store.attach_detach(&id, attachment_id).await;
                        return;
                    }
                }
            }

            // Client messages.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data, wait_for_change }) => {
                                debug!(session_id = %id, bytes = data.len(), "WS input received");
                                if let Err(err) = state.store.attach_input(&id, Some(attachment_id), &data, wait_for_change).await {
                                    // Control-gate violations are reported but
                                    // keep the stream; transport failures end it.
                                    let gated = matches!(err, SessionError::NotController | SessionError::StaleAttachment);
                                    if !send_server_message(&mut socket, &ServerMessage::Error {
                                        message: err.message(&id),
                                    }).await {
                                        let _ = state.store.attach_detach(&id, attachment_id).await;
                                        return;
                                    }
                                    if !gated {
                                        let _ = state.store.attach_detach(&id, attachment_id).await;
                                        return;
                                    }
                                }
                            }
                            Ok(ClientMessage::Busy) => {
                                trace!(session_id = %id, "WS attach busy received");
                                if let Err(err) = state.store.attach_busy(&id).await {
                                    let _ = send_server_message(&mut socket, &ServerMessage::Error {
                                        message: err.message(&id),
                                    }).await;
                                    let _ = state.store.attach_detach(&id, attachment_id).await;
                                    return;
                                }
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                debug!(session_id = %id, rows, cols, "WS resize received");
                                resize_sub.mark_sent(rows, cols);
                                match state.store.attach_resize(&id, Some(attachment_id), rows, cols).await {
                                    Ok(()) => {}
                                    // Observers never resize the shared PTY;
                                    // their declared size is a viewport only.
                                    Err(SessionError::NotController | SessionError::StaleAttachment) => {
                                        resize_sub.mark_sent(0, 0);
                                    }
                                    Err(err) => {
                                        let _ = send_server_message(&mut socket, &ServerMessage::Error {
                                            message: err.message(&id),
                                        }).await;
                                        let _ = state.store.attach_detach(&id, attachment_id).await;
                                        return;
                                    }
                                }
                            }
                            Ok(ClientMessage::AcquireControl) => {
                                debug!(session_id = %id, attachment_id, "local WebSocket control takeover requested");
                                match state.store.attach_acquire_control(&id, attachment_id).await {
                                    Ok(outcome) => {
                                        if !send_server_message(
                                            &mut socket,
                                            &ServerMessage::Control {
                                                role: outcome.role.as_str(),
                                            },
                                        )
                                        .await
                                        {
                                            let _ = state.store.attach_detach(&id, attachment_id).await;
                                            return;
                                        }
                                    }
                                    Err(err) => {
                                        warn!(session_id = %id, error = err.message(&id), "control takeover failed");
                                    }
                                }
                            }
                            Ok(ClientMessage::Ack { offset }) => {
                                state
                                    .store
                                    .attach_report_applied(&id, attachment_id, offset)
                                    .await;
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, "WS client detached");
                                let _ = state.store.attach_detach(&id, attachment_id).await;
                                return;
                            }
                            Ok(ClientMessage::Ping) => {
                                trace!(session_id = %id, "local WebSocket ping received");
                                let _ = send_server_message(&mut socket, &ServerMessage::Pong).await;
                            }
                            Err(err) => {
                                warn!(session_id = %id, %err, "failed to parse local WebSocket client message");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!(session_id = %id, "WS client disconnected");
                        let _ = state.store.attach_detach(&id, attachment_id).await;
                        return;
                    }
                    _ => {}
                }
            }

            // Control handoffs: every notice carries the current controller
            // id; derive this attachment's role from it.
            notice = async {
                match control_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Ok(controller) = notice {
                    let role: &'static str = if controller == Some(attachment_id) {
                        "controller"
                    } else {
                        "observer"
                    };
                    if !send_server_message(&mut socket, &ServerMessage::Control { role }).await {
                        let _ = state.store.attach_detach(&id, attachment_id).await;
                        return;
                    }
                }
            }

            // Resize notifications from other attached clients.
            Some((rows, cols)) = resize_sub.recv_foreign() => {
                debug!(
                    session_id = %id,
                    rows, cols,
                    "forwarding resize notification to local WebSocket client"
                );
                if !send_server_message(&mut socket, &ServerMessage::Resized { rows, cols }).await {
                    let _ = state.store.attach_detach(&id, attachment_id).await;
                    return;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming attach (node-proxied sessions) — uses proxy_rpc_stream()
// ---------------------------------------------------------------------------

async fn handle_ws_proxied_streaming(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    node: String,
    params: AttachConnectionParams,
) {
    let AttachConnectionParams {
        initial_rows,
        initial_cols,
        role,
        headers,
    } = params;
    info!(session_id = %id, node = %node, "starting proxied WebSocket stream");

    // Open streaming subscription via node proxy. Proxied streams are
    // credited (M5-2): mid-stream messages — including applied-cursor
    // acks — travel the relay as stream messages, so the owning node's
    // credit gate applies to remote clients exactly as to local ones.
    let rpc = RpcRequest::AttachSubscribe {
        id: id.to_string(),
        from_byte_offset: None,
        incarnation: None,
        rows: initial_rows.filter(|rows| *rows > 0),
        cols: initial_cols.filter(|cols| *cols > 0),
        role,
        credited: true,
    };
    let (stream_rpc_id, mut stream_rx) = match state
        .node_registry
        .proxy_rpc_stream(&node, &rpc)
        .await
    {
        Ok(pair) => pair,
        Err(err) => {
            warn!(session_id = %id, node = %node, %err, "failed to open proxied WebSocket stream");
            let _ = send_server_message(
                &mut socket,
                &ServerMessage::Error {
                    message: format!("failed to open proxy stream: {err}"),
                },
            )
            .await;
            return;
        }
    };

    let mut init_sent = false;

    // ADR-0007 (M5-4): revocation closes live proxied streams too.
    let revoke_token = crate::http::auth::extract_request_token_parts(&headers, None);
    let mut revocation_rx = state.auth.as_ref().map(|auth| auth.revocation_watch());

    loop {
        let revoked = async {
            match (&mut revocation_rx, &revoke_token) {
                (Some(rx), Some(_)) => {
                    let _ = rx.changed().await;
                }
                _ => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(revoked);
        tokio::select! {
            biased;

            _ = &mut revoked => {
                if let (Some(auth), Some(token)) = (&state.auth, &revoke_token)
                    && !auth.validate_token(token).await
                {
                    info!(session_id = %id, node = %node, "proxied WebSocket closed — session token revoked");
                    let _ = send_server_message(
                        &mut socket,
                        &ServerMessage::Error {
                            message: "session revoked (logout)".to_string(),
                        },
                    )
                    .await;
                    return;
                }
            }

            // Streaming frames from the node proxy.
            frame = stream_rx.recv() => {
                match frame {
                    Some(Ok(resp)) => {
                        match resp {
                            RpcResponse::AttachStreamInit {
                                data,
                                end_offset,
                                running,
                                app_cursor_keys,
                                bracketed_paste_mode,
                                incarnation,
                                attachment_id,
                                role,
                                ..
                            } => {
                                let replay_bytes = data.len();
                                let msg = ServerMessage::Init {
                                    data,
                                    end_offset,
                                    incarnation,
                                    running,
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                    attachment_id,
                                    role: if role == "controller" { "controller" } else { "observer" },
                                };
                                if !send_server_message(&mut socket, &msg).await {
                                    break;
                                }
                                debug!(
                                    session_id = %id,
                                    node = %node,
                                    snapshot_bytes = replay_bytes,
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                    "proxied WebSocket init frame received"
                                );
                                init_sent = true;
                            }
                            RpcResponse::AttachStreamChunk { offset, data } => {
                                if !data.is_empty() {
                                    trace!(session_id = %id, node = %node, bytes = data.len(), "forwarding proxied PTY output");
                                    let msg = ServerMessage::Data {
                                        offset,
                                        data,
                                    };
                                    if !send_server_message(&mut socket, &msg).await {
                                        break;
                                    }
                                }
                            }
                            RpcResponse::AttachControlChanged { role } => {
                                debug!(session_id = %id, node = %node, %role, "proxied WebSocket control handoff");
                                let _ = send_server_message(&mut socket, &ServerMessage::Control {
                                    role: if role == "controller" { "controller" } else { "observer" },
                                }).await;
                            }
                            RpcResponse::AttachModeChanged {
                                app_cursor_keys,
                                bracketed_paste_mode,
                            } => {
                                debug!(
                                    session_id = %id,
                                    node = %node,
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                    "proxied WebSocket terminal mode changed"
                                );
                                let _ = send_server_message(&mut socket, &ServerMessage::ModeChanged {
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                }).await;
                            }
                            RpcResponse::AttachResized { rows, cols } => {
                                debug!(
                                    session_id = %id,
                                    node = %node,
                                    rows, cols,
                                    "proxied WebSocket resize notification received"
                                );
                                let _ = send_server_message(&mut socket, &ServerMessage::Resized {
                                    rows,
                                    cols,
                                }).await;
                            }
                            RpcResponse::AttachStreamDone {
                                exit_code,
                                final_offset,
                            } => {
                                info!(session_id = %id, node = %node, ?exit_code, "proxied WebSocket stream ended");
                                let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code, final_offset }).await;
                                break;
                            }
                            RpcResponse::Error { message } => {
                                warn!(session_id = %id, node = %node, %message, "proxied WebSocket stream returned an error");
                                let _ = send_server_message(&mut socket, &ServerMessage::Error { message }).await;
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Err(err)) => {
                        warn!(session_id = %id, %err, "proxy stream error");
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded {
                                    exit_code: None,
                                    final_offset: 0,
                                })
                                .await;
                        break;
                    }
                    None => {
                        // Stream channel closed.
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded {
                                    exit_code: None,
                                    final_offset: 0,
                                })
                                .await;
                        break;
                    }
                }
            }

            // Client messages (input, resize, detach).
            msg = socket.recv(), if init_sent => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data, wait_for_change }) => {
                                debug!(session_id = %id, node = %node, bytes = data.len(), "proxied WebSocket input received");
                                let rpc = RpcRequest::AttachInput {
                                    id: id.to_string(),
                                    data,
                                    wait_for_change,
                                    attachment_id: None,
                                };
                                if let Err(err) = state.node_registry
                                    .proxy_rpc_stream_message(&node, &stream_rpc_id, &rpc)
                                    .await
                                {
                                    warn!(session_id = %id, node = %node, %err, "failed to proxy WebSocket input");
                                }
                            }
                            Ok(ClientMessage::Busy) => {
                                trace!(session_id = %id, node = %node, "proxied WebSocket attach busy received");
                                let rpc = RpcRequest::AttachBusy { id: id.to_string() };
                                if let Err(err) = state.node_registry.proxy_rpc(&node, &rpc).await {
                                    warn!(session_id = %id, node = %node, %err, "failed to proxy WebSocket attach busy");
                                }
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                debug!(session_id = %id, node = %node, rows, cols, "proxied WebSocket resize received");
                                let rpc = RpcRequest::AttachResize {
                                    id: id.to_string(),
                                    rows,
                                    cols,
                                };
                                if let Err(err) = state.node_registry
                                    .proxy_rpc_stream_message(&node, &stream_rpc_id, &rpc)
                                    .await
                                {
                                    warn!(session_id = %id, node = %node, rows, cols, %err, "failed to proxy WebSocket resize");
                                }
                            }
                            Ok(ClientMessage::AcquireControl) => {
                                debug!(session_id = %id, node = %node, "proxied WebSocket control takeover requested");
                                let rpc = RpcRequest::AttachAcquireControl { id: id.to_string() };
                                if let Err(err) = state.node_registry
                                    .proxy_rpc_stream_message(&node, &stream_rpc_id, &rpc)
                                    .await
                                {
                                    warn!(session_id = %id, node = %node, %err, "failed to proxy WebSocket control takeover");
                                }
                            }
                            Ok(ClientMessage::Ack { offset }) => {
                                let rpc = RpcRequest::AttachAppliedCursor {
                                    id: id.to_string(),
                                    cursor: offset,
                                };
                                if let Err(err) = state.node_registry
                                    .proxy_rpc_stream_message(&node, &stream_rpc_id, &rpc)
                                    .await
                                {
                                    warn!(session_id = %id, node = %node, %err, "failed to proxy WebSocket applied-cursor credit");
                                }
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, node = %node, "proxied WebSocket detach requested");
                                let rpc = RpcRequest::AttachDetach { id: id.to_string() };
                                if let Err(err) = state.node_registry
                                    .proxy_rpc_stream_message(&node, &stream_rpc_id, &rpc)
                                    .await
                                {
                                    warn!(session_id = %id, node = %node, %err, "failed to proxy WebSocket detach");
                                }
                                break;
                            }
                            Ok(ClientMessage::Ping) => {
                                trace!(session_id = %id, node = %node, "proxied WebSocket ping received");
                                let _ = send_server_message(&mut socket, &ServerMessage::Pong).await;
                            }
                            Err(err) => {
                                warn!(session_id = %id, node = %node, %err, "failed to parse proxied WebSocket client message");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!(session_id = %id, node = %node, "proxied WebSocket client disconnected");
                        let rpc = RpcRequest::AttachDetach { id: id.to_string() };
                        if let Err(err) = state.node_registry
                            .proxy_rpc_stream_message(&node, &stream_rpc_id, &rpc)
                            .await
                        {
                            warn!(session_id = %id, node = %node, %err, "failed to proxy WebSocket disconnect cleanup");
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Clean up the pending entry so it doesn't linger if the secondary
    // hasn't sent a done frame yet.
    state
        .node_registry
        .remove_pending(&node, &stream_rpc_id)
        .await;
    debug!(session_id = %id, node = %node, "proxied WebSocket stream cleanup complete");
}

fn init_msg_data_len(msg: &ServerMessage) -> usize {
    match msg {
        ServerMessage::Init { data, .. } => data.len(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ServerMessage, WS_FRAME_CONTROL, WS_FRAME_DATA, WS_FRAME_INIT, WS_FRAME_SESSION_ENDED,
        encode_server_message, panic_payload_message,
    };

    #[test]
    fn panic_payload_message_formats_static_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("attach panic");
        assert_eq!(panic_payload_message(payload.as_ref()), "attach panic");
    }

    #[test]
    fn panic_payload_message_formats_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("attach panic"));
        assert_eq!(panic_payload_message(payload.as_ref()), "attach panic");
    }

    /// Protocol evidence (post-review corrective increment, W4): the shared
    /// fixture tests/fixtures/ws_frames.json pins the wire format for BOTH
    /// endpoints. Here: encoding each `expect` description reproduces the
    /// pinned bytes exactly. The web decoder test decodes the same bytes
    /// back to the same description — encoder and decoder can never drift
    /// from each other without one side failing.
    #[test]
    fn ws_frame_fixture_matches_the_encoder() {
        fn decode_hex(hex: &str) -> Vec<u8> {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex byte"))
                .collect()
        }
        fn to_hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ws_frames.json");
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read ws_frames.json"))
                .expect("parse ws_frames.json");
        let frames = fixture["frames"].as_array().expect("frames array");
        assert!(frames.len() >= 10, "the fixture covers every frame kind");

        for frame in frames {
            let name = frame["name"].as_str().expect("name");
            let pinned = frame["hex"].as_str().expect("hex");
            let expect = &frame["expect"];
            let kind = expect["type"].as_str().expect("expect.type");
            let role = match expect["role"].as_str() {
                Some("controller") => "controller",
                _ => "observer",
            };
            let message = match kind {
                "init" => ServerMessage::Init {
                    data: decode_hex(expect["data_hex"].as_str().unwrap_or("")),
                    end_offset: expect["end_offset"].as_u64().expect("end_offset"),
                    incarnation: expect["incarnation"].as_u64().expect("incarnation"),
                    running: expect["running"].as_bool().expect("running"),
                    app_cursor_keys: expect["app_cursor_keys"].as_bool().expect("ack"),
                    bracketed_paste_mode: expect["bracketed_paste_mode"].as_bool().expect("bpm"),
                    attachment_id: expect["attachment_id"].as_u64().expect("attachment_id"),
                    role,
                },
                "data" => ServerMessage::Data {
                    offset: expect["offset"].as_u64().expect("offset"),
                    data: decode_hex(expect["data_hex"].as_str().unwrap_or("")),
                },
                "mode_changed" => ServerMessage::ModeChanged {
                    app_cursor_keys: expect["app_cursor_keys"].as_bool().expect("ack"),
                    bracketed_paste_mode: expect["bracketed_paste_mode"].as_bool().expect("bpm"),
                },
                "resized" => ServerMessage::Resized {
                    rows: expect["rows"].as_u64().expect("rows") as u16,
                    cols: expect["cols"].as_u64().expect("cols") as u16,
                },
                "session_ended" => ServerMessage::SessionEnded {
                    exit_code: expect["exit_code"].as_i64().map(|c| c as i32),
                    final_offset: expect["final_offset"].as_u64().expect("final_offset"),
                },
                "error" => ServerMessage::Error {
                    message: expect["message"].as_str().expect("message").to_string(),
                },
                "control" => ServerMessage::Control { role },
                "pong" => ServerMessage::Pong,
                other => panic!("{name}: unknown frame type {other}"),
            };
            let encoded = to_hex(&encode_server_message(&message));
            assert_eq!(encoded, pinned, "frame {name} diverged from the fixture");
        }
    }

    /// Frame-layout golden checks: the TypeScript decoder
    /// (web/src/api/client.ts) must read exactly these bytes.
    #[test]
    fn encode_init_frame_carries_cursor_and_incarnation() {
        let payload = encode_server_message(&ServerMessage::Init {
            data: b"hi".to_vec(),
            end_offset: 0x0102_0304_0506_0708,
            incarnation: 9,
            running: true,
            app_cursor_keys: true,
            bracketed_paste_mode: false,
            attachment_id: 42,
            role: "controller",
        });
        assert_eq!(payload[0], WS_FRAME_INIT);
        assert_eq!(payload[1], 1); // app-cursor-keys flag only
        assert_eq!(payload[2..10], 0x0102_0304_0506_0708_u64.to_be_bytes());
        assert_eq!(payload[10..18], 9_u64.to_be_bytes());
        assert_eq!(payload[18], 1); // running
        assert_eq!(payload[19..27], 42_u64.to_be_bytes()); // attachment id
        assert_eq!(payload[27], 1); // role: controller
        assert_eq!(&payload[28..], b"hi");
    }

    #[test]
    fn encode_control_frame_layout() {
        assert_eq!(
            encode_server_message(&ServerMessage::Control { role: "controller" }),
            vec![WS_FRAME_CONTROL, 1]
        );
        assert_eq!(
            encode_server_message(&ServerMessage::Control { role: "observer" }),
            vec![WS_FRAME_CONTROL, 0]
        );
    }

    #[test]
    fn encode_data_frame_carries_chunk_offset() {
        let payload = encode_server_message(&ServerMessage::Data {
            offset: 42,
            data: b"xy".to_vec(),
        });
        assert_eq!(payload[0], WS_FRAME_DATA);
        assert_eq!(payload[1..9], 42_u64.to_be_bytes());
        assert_eq!(&payload[9..], b"xy");
    }

    #[test]
    fn encode_session_ended_frame_carries_final_offset() {
        let payload = encode_server_message(&ServerMessage::SessionEnded {
            exit_code: Some(3),
            final_offset: 99,
        });
        assert_eq!(payload[0], WS_FRAME_SESSION_ENDED);
        assert_eq!(payload[1], 1);
        assert_eq!(payload[2..6], 3_i32.to_be_bytes());
        assert_eq!(payload[6..14], 99_u64.to_be_bytes());
    }

    #[test]
    fn panic_payload_message_handles_unknown_payloads() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_u8);
        assert_eq!(
            panic_payload_message(payload.as_ref()),
            "non-string panic payload"
        );
    }
}
