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
    db::Database,
    http::AppState,
    node::{NodeHandle, PendingRpc},
    notification::{
        channel::{LocalOsNotificationChannel, NotificationChannel, WebPushChannel},
        dispatcher::Notifier,
        event::{NotificationEvent, NotificationKind, NotificationTriggerRule},
    },
    protocol::{NodeSummary, NodeWsMessage, RpcResponse},
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
        Some(Ok(Message::Text(t))) => t.to_string(),
        _ => return,
    };

    let handshake: NodeWsMessage = match serde_json::from_str(&first) {
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
    let notifier = build_notifier(state.db.clone(), &state.config);

    let handle = NodeHandle { send_tx, pending };
    state.node_registry.connect(name.clone(), handle).await;

    // ── Step 5: send Joined (node is already visible in registry) ────────
    let joined_text = match serde_json::to_string(&NodeWsMessage::Joined) {
        Ok(t) => t,
        Err(_) => {
            state.node_registry.disconnect(&name).await;
            return;
        }
    };
    if ws_tx.send(Message::Text(joined_text.into())).await.is_err() {
        state.node_registry.disconnect(&name).await;
        return;
    }

    // ── Step 6: relay loop (single task, select! on send_rx and ws_rx) ───
    loop {
        tokio::select! {
            // Outgoing: channel → WS
            msg = send_rx.recv() => {
                let Some((id, req_json)) = msg else { break };
                let ws_msg = NodeWsMessage::Rpc { id, request: req_json };
                let text = match serde_json::to_string(&ws_msg) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ws_tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            // Incoming: WS → resolve pending RPC callers
            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<NodeWsMessage>(&text) {
                            Ok(NodeWsMessage::RpcResponse { id, response }) => {
                                if let Ok(rpc_resp) = serde_json::from_value::<RpcResponse>(response) {
                                    let mut pm = pending_recv.lock().await;
                                    if let Some(sender) = pm.remove(&id) {
                                        match sender {
                                            PendingRpc::OneShot(tx) => {
                                                let _ = tx.send(Ok(rpc_resp));
                                            }
                                            PendingRpc::Stream(tx) => {
                                                let _ = tx.send(Ok(rpc_resp));
                                                // One-shot response on a stream channel — done.
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(NodeWsMessage::RpcStreamFrame { id, response, done }) => {
                                if let Ok(rpc_resp) = serde_json::from_value::<RpcResponse>(response) {
                                    let mut pm = pending_recv.lock().await;
                                    if done {
                                        // Final frame — remove and send.
                                        if let Some(sender) = pm.remove(&id) {
                                            match sender {
                                                PendingRpc::Stream(tx) => {
                                                    let _ = tx.send(Ok(rpc_resp));
                                                }
                                                PendingRpc::OneShot(tx) => {
                                                    let _ = tx.send(Ok(rpc_resp));
                                                }
                                            }
                                        }
                                    } else {
                                        // Intermediate frame — send but keep channel.
                                        if let Some(sender) = pm.get(&id) {
                                            match sender {
                                                PendingRpc::Stream(tx) => {
                                                    let _ = tx.send(Ok(rpc_resp));
                                                }
                                                PendingRpc::OneShot(_) => {
                                                    // Shouldn't happen; ignore.
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(NodeWsMessage::Ping) => {
                                let pong = serde_json::to_string(&NodeWsMessage::Pong).unwrap_or_default();
                                let _ = ws_tx.send(Message::Text(pong.into())).await;
                            }
                            Ok(NodeWsMessage::Notification {
                                kind,
                                summary,
                                body,
                                session_ids,
                                trigger_rule,
                                trigger_detail,
                            }) => {
                                let trigger_rule_enum =
                                    trigger_rule.as_deref().and_then(NotificationTriggerRule::parse);
                                let maybe_kind = match kind.as_str() {
                                    "input_needed" => Some(NotificationKind::InputNeeded),
                                    "startup_recovery" => Some(NotificationKind::StartupRecovery),
                                    _ => None,
                                };

                                if let Some(kind_enum) = maybe_kind {
                                    let event = NotificationEvent {
                                        kind: kind_enum,
                                        summary: summary.clone(),
                                        body: body.clone(),
                                        session_ids: session_ids.clone(),
                                        trigger_rule: trigger_rule_enum,
                                        trigger_detail: trigger_detail.clone(),
                                    };
                                    let outcome = notifier.dispatch(&event).await;
                                    if !outcome.any_delivered() {
                                        warn!(
                                            node = %name,
                                            kind = %kind,
                                            attempted = outcome.attempted,
                                            failed_channels = ?outcome.failed_channels,
                                            "forwarded notification delivery failed on all channels"
                                        );
                                    }
                                } else {
                                    warn!(node = %name, kind = %kind, "unknown forwarded notification kind");
                                }

                                let _ = state.event_tx.send(crate::http::SessionEvent::SessionNotification {
                                    kind,
                                    summary,
                                    body,
                                    session_ids,
                                    trigger_rule,
                                    trigger_detail,
                                });
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    // Drain pending waiters with an error so callers don't hang.
    {
        let mut pm = pending_recv.lock().await;
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
    }

    state.node_registry.disconnect(&name).await;
    warn!(node = %name, "secondary node disconnected");
}

fn build_notifier(db: std::sync::Arc<Database>, config: &crate::config::AppConfig) -> Notifier {
    let mut channels: Vec<Box<dyn NotificationChannel + Send + Sync>> =
        vec![Box::new(LocalOsNotificationChannel {
            hook: config.notification_hook.clone(),
        })];

    if let (Some(vapid_public_key), Some(vapid_private_key), Some(vapid_subject)) = (
        config.web_push_vapid_public_key.clone(),
        config.web_push_vapid_private_key.clone(),
        config.web_push_subject.clone(),
    ) {
        match WebPushChannel::new(&vapid_private_key, &vapid_public_key, &vapid_subject, db) {
            Ok(channel) => channels.push(Box::new(channel)),
            Err(err) => warn!(%err, "web push channel init failed on primary node relay"),
        }
    }

    Notifier::with_channels(channels)
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
    if let Ok(text) = serde_json::to_string(&msg) {
        let _ = ws_tx.send(Message::Text(text.into())).await;
    }
}
