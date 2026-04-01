use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use tracing::{debug, info, trace, warn};

use crate::protocol::{RpcRequest, RpcResponse};
use crate::session::mode_tracker::ModeSnapshot;
use crate::session::pty::collect_chunk_bytes;
use crate::session::resize::ResizeSubscriber;

use super::AppState;

#[derive(Debug, Deserialize)]
pub struct AttachParams {
    pub node: Option<String>,
    /// Initial terminal width (cols) reported by the browser xterm instance.
    pub cols: Option<u16>,
    /// Initial terminal height (rows) reported by the browser xterm instance.
    pub rows: Option<u16>,
}

// ---------------------------------------------------------------------------
// Protocol types — unified for both local and proxied sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// Initial ring-buffer replay. `data` contains filtered stream bytes.
    Init {
        data: Vec<u8>,
        #[serde(rename = "appCursorKeys")]
        app_cursor_keys: bool,
        #[serde(rename = "bracketedPasteMode")]
        bracketed_paste_mode: bool,
    },
    /// Incremental PTY output chunk.
    Data {
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
    /// Session ended.
    SessionEnded {
        exit_code: Option<i32>,
    },
    Error {
        message: String,
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
const WS_FLAG_APP_CURSOR_KEYS: u8 = 1 << 0;
const WS_FLAG_BRACKETED_PASTE_MODE: u8 = 1 << 1;

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn attach_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<AttachParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    debug!(session_id = %id, "WebSocket upgrade requested");
    ws.on_upgrade(move |socket| handle_ws(socket, state, id, params.node, params.rows, params.cols))
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

async fn send_server_message(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    let payload = match msg {
        ServerMessage::Init {
            data,
            app_cursor_keys,
            bracketed_paste_mode,
        } => {
            let mut payload = Vec::with_capacity(2 + data.len());
            payload.push(WS_FRAME_INIT);
            payload.push(mode_flags(*app_cursor_keys, *bracketed_paste_mode));
            payload.extend_from_slice(data);
            payload
        }
        ServerMessage::Data { data } => {
            let mut payload = Vec::with_capacity(1 + data.len());
            payload.push(WS_FRAME_DATA);
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
        ServerMessage::SessionEnded { exit_code } => {
            let mut payload = Vec::with_capacity(6);
            payload.push(WS_FRAME_SESSION_ENDED);
            match exit_code {
                Some(code) => {
                    payload.push(1);
                    payload.extend_from_slice(&code.to_be_bytes());
                }
                None => payload.push(0),
            }
            payload
        }
        ServerMessage::Error { message } => {
            let mut payload = Vec::with_capacity(1 + message.len());
            payload.push(WS_FRAME_ERROR);
            payload.extend_from_slice(message.as_bytes());
            payload
        }
        ServerMessage::Pong => vec![WS_FRAME_PONG],
    };
    socket.send(Message::Binary(payload.into())).await.is_ok()
}

async fn handle_ws(
    socket: WebSocket,
    state: AppState,
    id: String,
    node: Option<String>,
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
) {
    debug!(session_id = %id, node = ?node, "WebSocket connected");

    if let Some(node_name) = node {
        handle_ws_proxied_streaming(socket, state, id, node_name, initial_rows, initial_cols).await;
        return;
    }

    handle_ws_streaming(socket, state, id, initial_rows, initial_cols).await;
}

// ---------------------------------------------------------------------------
// Streaming attach (local sessions) — unified with IPC protocol
// ---------------------------------------------------------------------------

async fn handle_ws_streaming(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
) {
    use tokio::sync::broadcast::error::RecvError;

    // Subscribe to broadcast + get ring replay, all under one lock.
    let subscribe_result = { state.store.attach_subscribe_init(&id, None).await };

    let (replay_chunks, end_offset, mut broadcast_rx, bracketed_paste_mode, app_cursor_keys) =
        match subscribe_result {
            Ok(t) => t,
            Err(err) => {
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

    let replay_bytes = collect_chunk_bytes(&replay_chunks);
    let replay_bytes_len = replay_bytes.len();

    let init_msg = ServerMessage::Init {
        data: replay_bytes,
        app_cursor_keys,
        bracketed_paste_mode,
    };
    if !send_server_message(&mut socket, &init_msg).await {
        debug!(session_id = %id, "local WebSocket closed before init frame could be sent");
        return;
    }

    state.store.register_attach_client(&id).await;

    // Subscribe to resize broadcasts so we can notify this client when
    // another attached client changes the PTY size.
    let mut resize_sub = ResizeSubscriber::new(state.store.subscribe_resize(&id), id.clone());

    debug!(
        session_id = %id,
        replay_chunks = replay_chunks.len(),
        replay_bytes = replay_bytes_len,
        end_offset,
        app_cursor_keys,
        bracketed_paste_mode,
        "local WebSocket stream initialized"
    );

    // Resize PTY to browser dimensions AFTER init is sent.
    if let (Some(rows), Some(cols)) = (initial_rows, initial_cols) {
        if rows > 0 && cols > 0 {
            resize_sub.mark_sent(rows, cols);
            match state.store.attach_resize(&id, rows, cols).await {
                Ok(()) => debug!(session_id = %id, rows, cols, "PTY resized after WS init"),
                Err(err) => {
                    warn!(session_id = %id, rows, cols, error = err.message(&id), "PTY resize after WS init failed")
                }
            }
        }
    }

    // Track last known mode state to detect changes.
    let mut last_modes = ModeSnapshot {
        app_cursor_keys,
        bracketed_paste_mode,
    };

    let mut current_offset = end_offset;
    let mut completion_check = tokio::time::interval(Duration::from_millis(200));
    completion_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;

            // Check session completion periodically.
            _ = completion_check.tick() => {
                let status = state.store.attach_stream_status(&id).await;
                match status {
                    Ok((running, _output_closed, exit_code)) => {
                        if !running {
                            let resync = state.store.attach_subscribe_init(&id, Some(current_offset)).await;
                            if let Ok((chunks, _new_end, _rx, _bpm, _ack)) = resync {
                                let raw = collect_chunk_bytes(&chunks);
                                if !raw.is_empty() {
                                    debug!(
                                        session_id = %id,
                                        resync_chunks = chunks.len(),
                                        resync_bytes = raw.len(),
                                        current_offset,
                                        "sending final buffered output before local WebSocket shutdown"
                                    );
                                    let msg = ServerMessage::Data {
                                        data: raw,
                                    };
                                    if !send_server_message(&mut socket, &msg).await {
                                        let _ = state.store.attach_detach(&id).await;
                                        return;
                                    }
                                }
                            }
                            info!(session_id = %id, ?exit_code, "WS session ended");
                            let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code }).await;
                            let _ = state.store.attach_detach(&id).await;
                            return;
                        }
                    }
                    Err(_) => {
                        warn!(session_id = %id, "local WebSocket stream status lookup failed");
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                        let _ = state.store.attach_detach(&id).await;
                        return;
                    }
                }
            }

            // PTY output from broadcast channel.
            chunk = broadcast_rx.recv() => {
                match chunk {
                    Ok(filtered_arc) => {
                        if !filtered_arc.is_empty() {
                            let msg = ServerMessage::Data {
                                data: filtered_arc.to_vec(),
                            };
                            if !send_server_message(&mut socket, &msg).await {
                                let _ = state.store.attach_detach(&id).await;
                                return;
                            }
                        }
                        current_offset += filtered_arc.len() as u64;
                        trace!(
                            session_id = %id,
                            filtered_bytes = filtered_arc.len(),
                            current_offset,
                            "forwarded live PTY output over local WebSocket"
                        );

                        // Check for mode changes.
                        let current_modes = state.store.get_mode_snapshot(&id);
                        if let Some(modes) = current_modes {
                            if modes != last_modes {
                                debug!(
                                    session_id = %id,
                                    app_cursor_keys = modes.app_cursor_keys,
                                    bracketed_paste_mode = modes.bracketed_paste_mode,
                                    "local WebSocket terminal mode changed"
                                );
                                if !send_server_message(&mut socket, &ServerMessage::ModeChanged {
                                    app_cursor_keys: modes.app_cursor_keys,
                                    bracketed_paste_mode: modes.bracketed_paste_mode,
                                }).await {
                                    let _ = state.store.attach_detach(&id).await;
                                    return;
                                }
                                last_modes = modes;
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(
                            session_id = %id,
                            skipped,
                            current_offset,
                            "local WebSocket lagged behind broadcast output; replaying from ring"
                        );
                        // Re-sync from ring.
                        let resync = state.store.attach_subscribe_init(&id, Some(current_offset)).await;
                        match resync {
                            Ok((chunks, new_end, rx, bpm, ack)) => {
                                broadcast_rx = rx;
                                let raw = collect_chunk_bytes(&chunks);
                                if !raw.is_empty() {
                                    debug!(
                                        session_id = %id,
                                        resync_chunks = chunks.len(),
                                        resync_bytes = raw.len(),
                                        from_offset = current_offset,
                                        to_offset = new_end,
                                        "replayed buffered PTY output for local WebSocket resync"
                                    );
                                    let msg = ServerMessage::Data {
                                        data: raw,
                                    };
                                    if !send_server_message(&mut socket, &msg).await {
                                        let _ = state.store.attach_detach(&id).await;
                                        return;
                                    }
                                }
                                current_offset = new_end;
                                last_modes = ModeSnapshot {
                                    app_cursor_keys: ack,
                                    bracketed_paste_mode: bpm,
                                };
                            }
                            Err(_) => {
                                warn!(session_id = %id, "local WebSocket resync failed");
                                let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                                let _ = state.store.attach_detach(&id).await;
                                return;
                            }
                        }
                    }
                    Err(RecvError::Closed) => {
                        let exit_code = state.store.get_exit_code(&id);
                        info!(session_id = %id, ?exit_code, "local WebSocket broadcast channel closed");
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code }).await;
                        let _ = state.store.attach_detach(&id).await;
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
                                if let Err(err) = state.store.attach_input(&id, &data, wait_for_change).await {
                                    let _ = send_server_message(&mut socket, &ServerMessage::Error {
                                        message: err.message(&id),
                                    }).await;
                                    let _ = state.store.attach_detach(&id).await;
                                    return;
                                }
                            }
                            Ok(ClientMessage::Busy) => {
                                trace!(session_id = %id, "WS attach busy received");
                                if let Err(err) = state.store.attach_busy(&id).await {
                                    let _ = send_server_message(&mut socket, &ServerMessage::Error {
                                        message: err.message(&id),
                                    }).await;
                                    let _ = state.store.attach_detach(&id).await;
                                    return;
                                }
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                debug!(session_id = %id, rows, cols, "WS resize received");
                                resize_sub.mark_sent(rows, cols);
                                if let Err(err) = state.store.attach_resize(&id, rows, cols).await {
                                    let _ = send_server_message(&mut socket, &ServerMessage::Error {
                                        message: err.message(&id),
                                    }).await;
                                    let _ = state.store.attach_detach(&id).await;
                                    return;
                                }
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, "WS client detached");
                                let _ = state.store.attach_detach(&id).await;
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
                        let _ = state.store.attach_detach(&id).await;
                        return;
                    }
                    _ => {}
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
                    let _ = state.store.attach_detach(&id).await;
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
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
) {
    info!(session_id = %id, node = %node, "starting proxied WebSocket stream");

    // Resize PTY via node proxy before subscribing.
    if let (Some(rows), Some(cols)) = (initial_rows, initial_cols) {
        if rows > 0 && cols > 0 {
            let rpc = RpcRequest::AttachResize {
                id: id.to_string(),
                rows,
                cols,
            };
            if let Err(err) = state.node_registry.proxy_rpc(&node, &rpc).await {
                warn!(session_id = %id, node = %node, rows, cols, %err, "proxied WebSocket pre-resize failed");
            } else {
                debug!(session_id = %id, node = %node, rows, cols, "PTY pre-resized via node proxy");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Open streaming subscription via node proxy.
    let rpc = RpcRequest::AttachSubscribe {
        id: id.to_string(),
        from_byte_offset: None,
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

    loop {
        tokio::select! {
            biased;

            // Streaming frames from the node proxy.
            frame = stream_rx.recv() => {
                match frame {
                    Some(Ok(resp)) => {
                        match resp {
                            RpcResponse::AttachStreamInit {
                                data,
                                app_cursor_keys,
                                bracketed_paste_mode,
                                ..
                            } => {
                                let replay_bytes = data.len();
                                let msg = ServerMessage::Init {
                                    data,
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                };
                                if !send_server_message(&mut socket, &msg).await {
                                    break;
                                }
                                debug!(
                                    session_id = %id,
                                    node = %node,
                                    replay_bytes,
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                    "proxied WebSocket init frame received"
                                );
                                init_sent = true;
                            }
                            RpcResponse::AttachStreamChunk { data, .. } => {
                                if !data.is_empty() {
                                    trace!(session_id = %id, node = %node, bytes = data.len(), "forwarding proxied PTY output");
                                    let msg = ServerMessage::Data {
                                        data,
                                    };
                                    if !send_server_message(&mut socket, &msg).await {
                                        break;
                                    }
                                }
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
                            RpcResponse::AttachStreamDone { exit_code } => {
                                info!(session_id = %id, node = %node, ?exit_code, "proxied WebSocket stream ended");
                                let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code }).await;
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
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                        break;
                    }
                    None => {
                        // Stream channel closed.
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
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
                                };
                                if let Err(err) = state.node_registry.proxy_rpc(&node, &rpc).await {
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
                                if let Err(err) = state.node_registry.proxy_rpc(&node, &rpc).await {
                                    warn!(session_id = %id, node = %node, rows, cols, %err, "failed to proxy WebSocket resize");
                                }
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, node = %node, "proxied WebSocket detach requested");
                                let rpc = RpcRequest::AttachDetach { id: id.to_string() };
                                if let Err(err) = state.node_registry.proxy_rpc(&node, &rpc).await {
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
                        if let Err(err) = state.node_registry.proxy_rpc(&node, &rpc).await {
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
