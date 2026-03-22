use std::{collections::HashMap, sync::Arc};

use axum::extract::ws::{Message, WebSocket};
use axum::{
    Json,
    extract::{State, WebSocketUpgrade},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::{
    http::AppState,
    node::{NodeHandle, PendingRpc},
    notification::event::{NotificationEvent, NotificationKind, NotificationTriggerRule},
    protocol::{
        NodeSummary, NodeWsMessage, RpcResponse, decode_node_ws_payload, encode_node_ws_payload,
    },
    session::SessionEvent,
};

// ---------------------------------------------------------------------------
// GET /api/nodes
// ---------------------------------------------------------------------------

pub async fn list_nodes(State(state): State<AppState>) -> Json<Vec<NodeSummary>> {
    let names = state.node_registry.connected_names().await;
    let summaries = names
        .into_iter()
        .map(|name| NodeSummary {
            name,
            connected: true,
        })
        .collect();
    Json(summaries)
}

// ---------------------------------------------------------------------------
// GET /api/nodes/join  (WebSocket upgrade)
// ---------------------------------------------------------------------------

pub async fn join_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(|socket| handle_join(socket, state))
}

async fn handle_join(socket: WebSocket, state: AppState) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── Step 1: read handshake ────────────────────────────────────────────
    let first = match ws_rx.next().await {
        Some(Ok(Message::Binary(data))) => data,
        _ => return,
    };

    let handshake: NodeWsMessage = match decode_node_ws_payload(&first) {
        Ok(m) => m,
        Err(_) => {
            send_error(&mut ws_tx, "invalid handshake format").await;
            return;
        }
    };

    let (name, key) = match handshake {
        NodeWsMessage::Join { name, key } => (name, key),
        _ => {
            send_error(&mut ws_tx, "expected join message").await;
            return;
        }
    };

    // ── Step 2: validate API key against any registered key ──────────────
    let hashes = match state.db.list_api_key_hashes().await {
        Ok(h) => h,
        Err(_) => {
            send_error(&mut ws_tx, "internal error").await;
            return;
        }
    };

    if hashes.is_empty() || !hashes.iter().any(|h| verify_api_key(&key, h)) {
        send_error(&mut ws_tx, "unauthorized").await;
        return;
    }

    // ── Step 3: reject duplicate node names ──────────────────────────────
    if state.node_registry.is_connected(&name).await {
        send_error(&mut ws_tx, &format!("name '{name}' is already connected")).await;
        return;
    }

    info!(node = %name, "secondary node connected");

    // ── Step 4: set up RPC relay channel and register node ───────────────
    let (send_tx, mut send_rx) = mpsc::channel::<(String, serde_json::Value)>(64);
    let pending: Arc<Mutex<HashMap<String, PendingRpc>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_recv = Arc::clone(&pending);

    let handle = NodeHandle { send_tx, pending };
    state.node_registry.connect(name.clone(), handle).await;

    // ── Step 5: send Joined (node is already visible in registry) ────────
    if send_node_message(&mut ws_tx, &NodeWsMessage::Joined)
        .await
        .is_err()
    {
        state.node_registry.disconnect(&name).await;
        return;
    }

    // ── Step 6: relay loop (single task, select! on send_rx and ws_rx) ───
    let disconnect_reason = loop {
        tokio::select! {
            // Outgoing: channel → WS
            msg = send_rx.recv() => {
                let Some((id, req_json)) = msg else {
                    break "node RPC relay channel closed".to_string();
                };
                let ws_msg = NodeWsMessage::Rpc { id, request: req_json };
                if let Err(err) = send_node_message(&mut ws_tx, &ws_msg).await {
                    break format!("failed to send proxied RPC to node WebSocket: {err}");
                }
            }
            // Incoming: WS → resolve pending RPC callers
            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(frame)) => {
                        let message = match frame {
                            Message::Close(frame) => {
                                break close_frame_disconnect_reason(frame);
                            }
                            other => match parse_node_message(other) {
                                Ok(message) => message,
                                Err(err) => {
                                    warn!(node = %name, %err, "failed to decode secondary node frame");
                                    continue;
                                }
                            },
                        };
                        match message {
                            node_message => match node_message {
                                NodeWsMessage::RpcResponse { id, response } => {
                                    if let Ok(rpc_resp) = serde_json::from_value::<RpcResponse>(response) {
                                        let sender = {
                                            let mut pm = pending_recv.lock().await;
                                            pm.remove(&id)
                                        };
                                        if let Some(sender) = sender {
                                            match sender {
                                                PendingRpc::OneShot(tx) => {
                                                    let _ = tx.send(Ok(rpc_resp));
                                                }
                                                PendingRpc::Stream(tx) => {
                                                    let _ = tx.send(Ok(rpc_resp)).await;
                                                }
                                            }
                                        }
                                    }
                                }
                                NodeWsMessage::RpcStreamFrame { id, response, done } => {
                                    if let Ok(rpc_resp) = serde_json::from_value::<RpcResponse>(response) {
                                        if done {
                                            let sender = {
                                                let mut pm = pending_recv.lock().await;
                                                pm.remove(&id)
                                            };
                                            if let Some(sender) = sender {
                                                match sender {
                                                    PendingRpc::Stream(tx) => {
                                                        let _ = tx.send(Ok(rpc_resp)).await;
                                                    }
                                                    PendingRpc::OneShot(tx) => {
                                                        let _ = tx.send(Ok(rpc_resp));
                                                    }
                                                }
                                            }
                                        } else {
                                            let tx_clone = {
                                                let pm = pending_recv.lock().await;
                                                if let Some(PendingRpc::Stream(tx)) = pm.get(&id) {
                                                    Some(tx.clone())
                                                } else {
                                                    None
                                                }
                                            };
                                            if let Some(tx) = tx_clone {
                                                if tx.send(Ok(rpc_resp)).await.is_err() {
                                                    let mut pm = pending_recv.lock().await;
                                                    pm.remove(&id);
                                                }
                                            }
                                        }
                                    }
                                }
                                NodeWsMessage::Ping => {
                                    if let Err(err) = send_node_message(
                                        &mut ws_tx,
                                        &NodeWsMessage::Pong,
                                    )
                                    .await {
                                        break format!("failed to send pong to node WebSocket: {err}");
                                    }
                                }
                                NodeWsMessage::Notification {
                                    kind,
                                    title,
                                    description,
                                    body,
                                    navigation_url,
                                    session_ids,
                                    trigger_rule,
                                    trigger_detail,
                                } => {
                                    let payload = SessionEvent::SessionNotification {
                                        kind,
                                        title,
                                        description,
                                        body,
                                        navigation_url,
                                        session_ids,
                                        trigger_rule,
                                        trigger_detail,
                                        node: Some(name.clone()),
                                    };
                                    handle_forwarded_session_event(&state, &name, payload, true)
                                        .await;
                                }
                                NodeWsMessage::SessionEvent { payload } => {
                                    handle_forwarded_session_event(&state, &name, payload, false)
                                        .await;
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Err(err)) => break format!("node WebSocket receive error: {err}"),
                    None => break "node WebSocket stream ended".to_string(),
                }
            }
        }
    };

    // Drain pending waiters with an error so callers don't hang.
    let drained_waiters = {
        let mut pm = pending_recv.lock().await;
        let drained_waiters = pm.len();
        let err = || crate::error::AppError::NodeNotConnected(name.clone());
        for (_, sender) in pm.drain() {
            match sender {
                PendingRpc::OneShot(tx) => {
                    let _ = tx.send(Err(err()));
                }
                PendingRpc::Stream(tx) => {
                    let _ = tx.send(Err(err()));
                }
            }
        }
        drained_waiters
    };

    state.node_registry.disconnect(&name).await;
    warn!(node = %name, reason = %disconnect_reason, drained_waiters, "secondary node disconnected");
}

async fn handle_forwarded_session_event(
    state: &AppState,
    node_name: &str,
    payload: SessionEvent,
    send_to_channels: bool,
) {
    let delivered = crate::http::sse::session_event_for_delivery(&payload, Some(node_name));

    if let SessionEvent::SessionNotification {
        kind,
        title,
        description,
        body,
        navigation_url,
        session_ids,
        trigger_rule,
        trigger_detail,
        ..
    } = &delivered
    {
        let title = format!("[{}] {}", node_name, title);
        let delivered_node = match &delivered {
            SessionEvent::SessionNotification { node, .. } => node.clone(),
            _ => None,
        };
        let trigger_rule_enum = trigger_rule
            .as_deref()
            .and_then(NotificationTriggerRule::parse);
        let maybe_kind = match kind.as_str() {
            "input_needed" => Some(NotificationKind::InputNeeded),
            "startup_recovery" => Some(NotificationKind::StartupRecovery),
            _ => None,
        };

        if send_to_channels {
            if let Some(kind_enum) = maybe_kind {
                let event = NotificationEvent {
                    kind: kind_enum,
                    title,
                    description: description.clone(),
                    body: body.clone(),
                    navigation_url: navigation_url.clone(),
                    session_ids: session_ids.clone(),
                    trigger_rule: trigger_rule_enum,
                    trigger_detail: trigger_detail.clone(),
                    node: delivered_node,
                };
                let outcome = state.notifier.dispatch(&event).await;
                if !outcome.any_delivered() {
                    warn!(
                        node = %node_name,
                        kind = %kind,
                        attempted = outcome.attempted,
                        failed_channels = ?outcome.failed_channels,
                        "forwarded notification delivery failed on all channels"
                    );
                }
            } else {
                warn!(node = %node_name, kind = %kind, "unknown forwarded notification kind");
            }
        }
    }

    let _ = state.event_tx.send(delivered);
}
// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn verify_api_key(key: &str, hash: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(key.as_bytes(), &parsed)
        .is_ok()
}

async fn send_error(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &str,
) {
    let msg = NodeWsMessage::Error {
        message: message.to_string(),
    };
    if let Ok(payload) = encode_node_ws_payload(&msg) {
        let _ = ws_tx.send(Message::Binary(payload.into())).await;
    }
}

async fn send_node_message(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &NodeWsMessage,
) -> Result<(), String> {
    match encode_node_ws_payload(message) {
        Ok(payload) => ws_tx
            .send(Message::Binary(payload.into()))
            .await
            .map_err(|err| err.to_string()),
        Err(err) => {
            warn!(%err, "failed to encode node WebSocket frame");
            Err(err.to_string())
        }
    }
}

fn parse_node_message(frame: Message) -> std::io::Result<NodeWsMessage> {
    match frame {
        Message::Binary(data) => decode_node_ws_payload(&data),
        _ => Err(std::io::Error::other("unsupported node WebSocket frame")),
    }
}

fn close_frame_disconnect_reason(frame: Option<axum::extract::ws::CloseFrame>) -> String {
    match frame {
        Some(frame) => format!("peer sent close frame: {frame:?}"),
        None => "peer sent close frame".to_string(),
    }
}
