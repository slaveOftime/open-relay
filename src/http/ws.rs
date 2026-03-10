use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use tracing::{debug, info, trace, warn};

use crate::protocol::{RpcRequest, RpcResponse};

use super::{AppState, sessions::NodeParams};

// ---------------------------------------------------------------------------
// Protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    Snapshot {
        lines: Vec<String>,
        cursor: usize,
        running: bool,
    },
    Output {
        lines: Vec<String>,
        cursor: usize,
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
    Query(params): Query<NodeParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    debug!(session_id = %id, "WebSocket upgrade requested");
    ws.on_upgrade(move |socket| handle_ws(socket, state, id, params.node))
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
) -> Result<(Vec<String>, usize, bool), String> {
    if let Some(node_name) = node {
        let rpc = RpcRequest::AttachSnapshot { id: id.to_string() };
        return match state.node_registry.proxy_rpc(node_name, &rpc).await {
            Ok(RpcResponse::AttachSnapshot {
                lines,
                cursor,
                running,
                ..
            }) => Ok((lines, cursor, running)),
            Ok(RpcResponse::Error { message }) => Err(message),
            Err(err) => Err(err.to_string()),
            Ok(_) => Err("unexpected response from node".to_string()),
        };
    }

    let snapshot = {
        let mut store = state.store.lock().await;
        store.attach_snapshot(id).await
    };

    snapshot
        .map(
            |(lines, cursor, running, _bracketed_paste_mode, _app_cursor_keys)| {
                (lines, cursor, running)
            },
        )
        .map_err(|err| err.message(id))
}

async fn attach_poll(
    state: &AppState,
    id: &str,
    cursor: usize,
    node: Option<&str>,
) -> Result<(Vec<String>, usize, bool), String> {
    if let Some(node_name) = node {
        let rpc = RpcRequest::AttachPoll {
            id: id.to_string(),
            cursor,
        };
        return match state.node_registry.proxy_rpc(node_name, &rpc).await {
            Ok(RpcResponse::AttachPoll {
                lines,
                cursor,
                running,
                ..
            }) => Ok((lines, cursor, running)),
            Ok(RpcResponse::Error { message }) => Err(message),
            Err(err) => Err(err.to_string()),
            Ok(_) => Err("unexpected response from node".to_string()),
        };
    }

    let poll = {
        let mut store = state.store.lock().await;
        store.attach_poll(id, cursor).await
    };

    poll.map(
        |(lines, next_cursor, running, _bracketed_paste_mode, _app_cursor_keys)| {
            (lines, next_cursor, running)
        },
    )
    .map_err(|err| err.message(id))
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

async fn handle_ws(mut socket: WebSocket, state: AppState, id: String, node: Option<String>) {
    debug!(session_id = %id, node = ?node, "WebSocket attached");
    // ── Initial snapshot ──────────────────────────────────────────────────
    let snapshot = attach_snapshot(&state, &id, node.as_deref()).await;

    let (mut cursor, is_running) = match snapshot {
        Ok((lines, cursor, running)) => {
            let msg = ServerMessage::Snapshot {
                lines,
                cursor,
                running,
            };
            if !send_json(&mut socket, &msg).await {
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
        // Stopped session: just send snapshot and close
        debug!(session_id = %id, "WS attach: session already stopped, closing");
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
                            return;
                        }
                    }
                    Err(_) => {
                        warn!(session_id = %id, "WS poll error, closing");
                        let _ = send_json(&mut socket, &ServerMessage::End { exit_code: None }).await;
                        return;
                    }
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data }) => {
                                trace!(session_id = %id, bytes = data.len(), "WS input received");
                                attach_input(&state, &id, data, node.as_deref()).await;
                            }
                            Ok(ClientMessage::Resize { rows, cols }) => {
                                trace!(session_id = %id, rows, cols, "WS resize received");
                                attach_resize(&state, &id, rows, cols, node.as_deref()).await;
                            }
                            Ok(ClientMessage::Detach) => {
                                debug!(session_id = %id, "WS client detached");
                                return;
                            }
                            Err(_) => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        debug!(session_id = %id, "WS client disconnected");
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}
