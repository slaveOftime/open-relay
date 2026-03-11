use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use tracing::{debug, info, warn};

use crate::protocol::{RpcRequest, RpcResponse};

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
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Snapshot {
        lines: Vec<String>,
        cursor: u64,
        running: bool,
    },
    Output {
        lines: Vec<String>,
        cursor: u64,
    },
    End {
        exit_code: Option<i32>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    Input { data: String },
    Resize { rows: u16, cols: u16 },
    Detach,
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

async fn attach_snapshot(
    state: &AppState,
    id: &str,
    node: Option<&str>,
) -> Result<(Vec<String>, u64, bool), String> {
    if let Some(node_name) = node {
        // For proxied sessions use logs_snapshot over the node RPC.
        let rpc = RpcRequest::LogsSnapshot {
            id: id.to_string(),
            tail: 1000,
        };
        return match state.node_registry.proxy_rpc(node_name, &rpc).await {
            Ok(RpcResponse::LogsSnapshot {
                lines,
                cursor,
                running,
            }) => Ok((lines, cursor, running)),
            Ok(RpcResponse::Error { message }) => Err(message),
            Err(err) => Err(err.to_string()),
            Ok(_) => Err("unexpected response from node".to_string()),
        };
    }

    let snapshot = {
        let mut store = state.store.lock().await;
        store.logs_snapshot(id, 1000).await
    };

    snapshot
        .map(|(lines, cursor, running)| (lines, cursor, running))
        .ok_or_else(|| format!("session not found: {id}"))
}

async fn attach_poll(
    state: &AppState,
    id: &str,
    cursor: u64,
    node: Option<&str>,
) -> Result<(Vec<String>, u64, bool), String> {
    if let Some(node_name) = node {
        let rpc = RpcRequest::LogsPoll {
            id: id.to_string(),
            cursor,
        };
        return match state.node_registry.proxy_rpc(node_name, &rpc).await {
            Ok(RpcResponse::LogsPoll {
                lines,
                cursor,
                running,
            }) => Ok((lines, cursor, running)),
            Ok(RpcResponse::Error { message }) => Err(message),
            Err(err) => Err(err.to_string()),
            Ok(_) => Err("unexpected response from node".to_string()),
        };
    }

    let poll = {
        let mut store = state.store.lock().await;
        store.raw_bytes_since_snapshot(id, cursor).await
    };

    poll.map(|(lines, next_cursor, running)| (lines, next_cursor, running))
        .ok_or_else(|| format!("session not found: {id}"))
}

async fn attach_input(state: &AppState, id: &str, data: String, node: Option<&str>) {
    if let Some(node_name) = node {
        let rpc = RpcRequest::AttachInput {
            id: id.to_string(),
            data,
        };
        let _ = state.node_registry.proxy_rpc(node_name, &rpc).await;
        return;
    }

    let mut store = state.store.lock().await;
    let _ = store.attach_input(id, &data).await;
}

async fn attach_resize(state: &AppState, id: &str, rows: u16, cols: u16, node: Option<&str>) {
    if let Some(node_name) = node {
        let rpc = RpcRequest::AttachResize {
            id: id.to_string(),
            rows,
            cols,
        };
        let _ = state.node_registry.proxy_rpc(node_name, &rpc).await;
        return;
    }

    let mut store = state.store.lock().await;
    let _ = store.attach_resize(id, rows, cols).await;
}

async fn attach_detach(state: &AppState, id: &str, node: Option<&str>) {
    if let Some(node_name) = node {
        let rpc = RpcRequest::AttachDetach { id: id.to_string() };
        let _ = state.node_registry.proxy_rpc(node_name, &rpc).await;
        return;
    }

    let mut store = state.store.lock().await;
    let _ = store.attach_detach(id).await;
}

async fn handle_ws(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    node: Option<String>,
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
) {
    debug!(session_id = %id, node = ?node, "WebSocket attached");

    // Capture the ring offset BEFORE the resize so the snapshot only includes
    // bytes that arrived AFTER SIGWINCH (the fresh TUI redraw), not the
    // historical content rendered at the old terminal size.
    // Only used for local sessions; node proxy sessions use the existing path.
    let pre_resize_offset: Option<u64> = if node.is_none()
        && initial_rows.is_some_and(|r| r > 0)
        && initial_cols.is_some_and(|c| c > 0)
    {
        Some(state.store.lock().await.get_ring_end_offset(&id))
    } else {
        None
    };

    // Resize the PTY then wait for the TUI to emit its post-SIGWINCH redraw
    // into the ring buffer before we sample it for the snapshot.
    if let (Some(rows), Some(cols)) = (initial_rows, initial_cols) {
        if rows > 0 && cols > 0 {
            attach_resize(&state, &id, rows, cols, node.as_deref()).await;
            debug!(session_id = %id, rows, cols, "PTY pre-resized from WS query params");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // ── Initial snapshot ──────────────────────────────────────────────────
    // For local sessions with a known pre-resize offset use the raw-bytes
    // path which: (a) includes only post-SIGWINCH content, (b) preserves \r
    // in \r\n sequences (logs_snapshot strips \r via .lines()).
    let snapshot = if let Some(offset) = pre_resize_offset {
        let result = {
            let mut store = state.store.lock().await;
            store.raw_bytes_since_snapshot(&id, offset).await
        };
        result.ok_or_else(|| format!("session not found: {id}"))
    } else {
        attach_snapshot(&state, &id, node.as_deref()).await
    };

    let (mut cursor, is_running) = match snapshot {
        Ok((lines, cursor, running)) => {
            let msg = ServerMessage::Snapshot {
                lines,
                cursor,
                running,
            };
            if !send_json(&mut socket, &msg).await {
                attach_detach(&state, &id, node.as_deref()).await;
                return;
            }
            (cursor, running)
        }
        Err(message) => {
            warn!(session_id = %id, error = %message, "WS attach failed");
            let msg = ServerMessage::Error { message };
            let _ = send_json(&mut socket, &msg).await;
            return;
        }
    };

    if !is_running {
        // Stopped session: send End frame so the client knows not to reconnect, then close.
        let exit_code = if node.is_none() {
            state.store.lock().await.get_exit_code(&id)
        } else {
            None
        };
        debug!(session_id = %id, ?exit_code, "WS attach: session already stopped, closing");
        let _ = send_json(&mut socket, &ServerMessage::End { exit_code }).await;
        attach_detach(&state, &id, node.as_deref()).await;
        return;
    }

    // ── Bidirectional loop ────────────────────────────────────────────────
    let mut poll_interval = tokio::time::interval(Duration::from_millis(50));

    loop {
        tokio::select! {
            _ = poll_interval.tick() => {
                let poll = attach_poll(&state, &id, cursor, node.as_deref()).await;

                match poll {
                    Ok((lines, new_cursor, running)) => {
                        if !lines.is_empty() {
                            let msg = ServerMessage::Output { lines, cursor: new_cursor };
                            if !send_json(&mut socket, &msg).await {
                                attach_detach(&state, &id, node.as_deref()).await;
                                return; // client disconnected
                            }
                        }
                        cursor = new_cursor;
                        if !running {
                            // For remote node sessions there is currently no RPC carrying
                            // exit code in attach poll responses, so report null.
                            let exit_code = if node.is_some() {
                                None
                            } else {
                                state.store.lock().await.get_exit_code(&id)
                            };
                            info!(session_id = %id, ?exit_code, "WS session ended");
                            let _ = send_json(&mut socket, &ServerMessage::End { exit_code }).await;
                            attach_detach(&state, &id, node.as_deref()).await;
                            return;
                        }
                    }
                    Err(_) => {
                        warn!(session_id = %id, "WS poll error, closing");
                        let _ = send_json(&mut socket, &ServerMessage::End { exit_code: None }).await;
                        attach_detach(&state, &id, node.as_deref()).await;
                        return;
                    }
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data }) => {
                                debug!(session_id = %id, bytes = data.len(), "WS input received");
                                attach_input(&state, &id, data, node.as_deref()).await;
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                debug!(session_id = %id, rows, cols, "WS resize received");
                                attach_resize(&state, &id, rows, cols, node.as_deref()).await;
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, "WS client detached");
                                attach_detach(&state, &id, node.as_deref()).await;
                                return;
                            }
                            Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!(session_id = %id, "WS client disconnected");
                        attach_detach(&state, &id, node.as_deref()).await;
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}
