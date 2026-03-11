use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use tracing::{debug, info, warn};

use crate::protocol::{RpcRequest, RpcResponse};
use crate::session::mode_tracker::ModeSnapshot;
use crate::session::pty::EscapeFilter;

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
    /// Initial ring-buffer replay. `data` is base64-encoded raw PTY bytes.
    Init {
        data: String,
        #[serde(rename = "appCursorKeys")]
        app_cursor_keys: bool,
        #[serde(rename = "bracketedPasteMode")]
        bracketed_paste_mode: bool,
    },
    /// Incremental PTY output chunk. `data` is base64-encoded.
    Data {
        data: String,
    },
    /// Terminal mode changed mid-stream.
    ModeChanged {
        #[serde(rename = "appCursorKeys")]
        app_cursor_keys: bool,
        #[serde(rename = "bracketedPasteMode")]
        bracketed_paste_mode: bool,
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
    Input { data: String },
    Resize { rows: u16, cols: u16 },
    Detach,
    Ping,
}

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

async fn send_json(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => socket.send(Message::Text(json.into())).await.is_ok(),
        Err(err) => {
            warn!(%err, "failed to serialize WebSocket message");
            false
        }
    }
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
    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    use tokio::sync::broadcast::error::RecvError;

    // Subscribe to broadcast + get ring replay, all under one lock.
    let subscribe_result = {
        let mut store = state.store.lock().await;
        store.attach_subscribe_init(&id, None).await
    };

    let (replay_chunks, _end_offset, mut broadcast_rx, bracketed_paste_mode, app_cursor_keys) =
        match subscribe_result {
            Ok(t) => t,
            Err(err) => {
                let _ = send_json(
                    &mut socket,
                    &ServerMessage::Error {
                        message: err.message(&id),
                    },
                )
                .await;
                return;
            }
        };

    // Filter CPR/DSR from replay and send as init frame.
    let mut init_filter = EscapeFilter::new();
    let replay_bytes: Vec<u8> = replay_chunks
        .iter()
        .flat_map(|(_, b)| init_filter.filter(b))
        .collect();

    let init_msg = ServerMessage::Init {
        data: B64.encode(&replay_bytes),
        app_cursor_keys,
        bracketed_paste_mode,
    };
    if !send_json(&mut socket, &init_msg).await {
        let mut store = state.store.lock().await;
        let _ = store.attach_detach(&id).await;
        return;
    }

    // Mark attach presence.
    {
        let mut store = state.store.lock().await;
        let _ = store.mark_attach_presence(&id).await;
    }

    // Resize PTY to browser dimensions AFTER init is sent.
    if let (Some(rows), Some(cols)) = (initial_rows, initial_cols) {
        if rows > 0 && cols > 0 {
            let mut store = state.store.lock().await;
            let _ = store.attach_resize(&id, rows, cols).await;
            debug!(session_id = %id, rows, cols, "PTY resized after WS init");
        }
    }

    // Track last known mode state to detect changes.
    let mut last_modes = ModeSnapshot {
        app_cursor_keys,
        bracketed_paste_mode,
    };

    let mut chunk_filter = EscapeFilter::new();
    let mut completion_check = tokio::time::interval(Duration::from_millis(200));
    completion_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;

            // Check session completion periodically.
            _ = completion_check.tick() => {
                let status = {
                    let mut store = state.store.lock().await;
                    store.attach_stream_status(&id).await
                };
                match status {
                    Ok((running, _output_closed, exit_code)) => {
                        if !running {
                            info!(session_id = %id, ?exit_code, "WS session ended");
                            let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code }).await;
                            let mut store = state.store.lock().await;
                            let _ = store.attach_detach(&id).await;
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                        return;
                    }
                }
            }

            // PTY output from broadcast channel.
            chunk = broadcast_rx.recv() => {
                match chunk {
                    Ok(raw_arc) => {
                        let filtered = chunk_filter.filter(&raw_arc);
                        if !filtered.is_empty() {
                            let msg = ServerMessage::Data {
                                data: B64.encode(&filtered),
                            };
                            if !send_json(&mut socket, &msg).await {
                                let mut store = state.store.lock().await;
                                let _ = store.attach_detach(&id).await;
                                return;
                            }
                        }

                        // Check for mode changes.
                        let current_modes = {
                            let store = state.store.lock().await;
                            // Borrow the runtime to read mode snapshot.
                            store.get_mode_snapshot(&id)
                        };
                        if let Some(modes) = current_modes {
                            if modes != last_modes {
                                let _ = send_json(&mut socket, &ServerMessage::ModeChanged {
                                    app_cursor_keys: modes.app_cursor_keys,
                                    bracketed_paste_mode: modes.bracketed_paste_mode,
                                }).await;
                                last_modes = modes;
                            }
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Re-sync from ring.
                        let resync = {
                            let mut store = state.store.lock().await;
                            store.attach_subscribe_init(&id, None).await
                        };
                        match resync {
                            Ok((chunks, _, rx, bpm, ack)) => {
                                broadcast_rx = rx;
                                let mut resync_filter = EscapeFilter::new();
                                let raw: Vec<u8> = chunks
                                    .iter()
                                    .flat_map(|(_, b)| resync_filter.filter(b))
                                    .collect();
                                if !raw.is_empty() {
                                    let msg = ServerMessage::Data {
                                        data: B64.encode(&raw),
                                    };
                                    if !send_json(&mut socket, &msg).await {
                                        let mut store = state.store.lock().await;
                                        let _ = store.attach_detach(&id).await;
                                        return;
                                    }
                                }
                                last_modes = ModeSnapshot {
                                    app_cursor_keys: ack,
                                    bracketed_paste_mode: bpm,
                                };
                            }
                            Err(_) => {
                                let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                                return;
                            }
                        }
                    }
                    Err(RecvError::Closed) => {
                        let exit_code = state.store.lock().await.get_exit_code(&id);
                        let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code }).await;
                        let mut store = state.store.lock().await;
                        let _ = store.attach_detach(&id).await;
                        return;
                    }
                }
            }

            // Client messages.
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data }) => {
                                debug!(session_id = %id, bytes = data.len(), "WS input received");
                                let mut store = state.store.lock().await;
                                let _ = store.attach_input(&id, &data).await;
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                debug!(session_id = %id, rows, cols, "WS resize received");
                                let mut store = state.store.lock().await;
                                let _ = store.attach_resize(&id, rows, cols).await;
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, "WS client detached");
                                let mut store = state.store.lock().await;
                                let _ = store.attach_detach(&id).await;
                                return;
                            }
                            Ok(ClientMessage::Ping) => {
                                let _ = send_json(&mut socket, &ServerMessage::Pong).await;
                            }
                            Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!(session_id = %id, "WS client disconnected");
                        let mut store = state.store.lock().await;
                        let _ = store.attach_detach(&id).await;
                        return;
                    }
                    _ => {}
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
    use base64::{Engine, engine::general_purpose::STANDARD as B64};

    // Resize PTY via node proxy before subscribing.
    if let (Some(rows), Some(cols)) = (initial_rows, initial_cols) {
        if rows > 0 && cols > 0 {
            let rpc = RpcRequest::AttachResize {
                id: id.to_string(),
                rows,
                cols,
            };
            let _ = state.node_registry.proxy_rpc(&node, &rpc).await;
            debug!(session_id = %id, rows, cols, "PTY pre-resized via node proxy");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Open streaming subscription via node proxy.
    let rpc = RpcRequest::AttachSubscribe {
        id: id.to_string(),
        from_byte_offset: None,
    };
    let mut stream_rx = match state.node_registry.proxy_rpc_stream(&node, &rpc).await {
        Ok(rx) => rx,
        Err(err) => {
            let _ = send_json(
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
                                let msg = ServerMessage::Init {
                                    data: B64.encode(&data),
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                };
                                if !send_json(&mut socket, &msg).await {
                                    return;
                                }
                                init_sent = true;
                            }
                            RpcResponse::AttachStreamChunk { data, .. } => {
                                if !data.is_empty() {
                                    let msg = ServerMessage::Data {
                                        data: B64.encode(&data),
                                    };
                                    if !send_json(&mut socket, &msg).await {
                                        return;
                                    }
                                }
                            }
                            RpcResponse::AttachModeChanged {
                                app_cursor_keys,
                                bracketed_paste_mode,
                            } => {
                                let _ = send_json(&mut socket, &ServerMessage::ModeChanged {
                                    app_cursor_keys,
                                    bracketed_paste_mode,
                                }).await;
                            }
                            RpcResponse::AttachStreamDone { exit_code } => {
                                let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code }).await;
                                return;
                            }
                            RpcResponse::Error { message } => {
                                let _ = send_json(&mut socket, &ServerMessage::Error { message }).await;
                                return;
                            }
                            _ => {}
                        }
                    }
                    Some(Err(err)) => {
                        warn!(session_id = %id, %err, "proxy stream error");
                        let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                        return;
                    }
                    None => {
                        // Stream channel closed.
                        let _ = send_json(&mut socket, &ServerMessage::SessionEnded { exit_code: None }).await;
                        return;
                    }
                }
            }

            // Client messages (input, resize, detach).
            msg = socket.recv(), if init_sent => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data }) => {
                                let rpc = RpcRequest::AttachInput {
                                    id: id.to_string(),
                                    data,
                                };
                                let _ = state.node_registry.proxy_rpc(&node, &rpc).await;
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                let rpc = RpcRequest::AttachResize {
                                    id: id.to_string(),
                                    rows,
                                    cols,
                                };
                                let _ = state.node_registry.proxy_rpc(&node, &rpc).await;
                            }
                            Ok(ClientMessage::Detach) => {
                                let rpc = RpcRequest::AttachDetach { id: id.to_string() };
                                let _ = state.node_registry.proxy_rpc(&node, &rpc).await;
                                return;
                            }
                            Ok(ClientMessage::Ping) => {
                                let _ = send_json(&mut socket, &ServerMessage::Pong).await;
                            }
                            Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let rpc = RpcRequest::AttachDetach { id: id.to_string() };
                        let _ = state.node_registry.proxy_rpc(&node, &rpc).await;
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}
