use std::{collections::HashMap, sync::Arc};

use axum::extract::ws::{Message, WebSocket};
use axum::{
    Json,
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc, watch};
use tracing::{info, warn};

use crate::{
    federation_crypto,
    http::AppState,
    node::liveness::{NODE_SEND_TIMEOUT, wait_for_inbound_timeout},
    node::{NodeHandle, PendingRpc},
    notification::event::{NotificationEvent, NotificationKind, NotificationTriggerRule},
    protocol::{
        FEDERATION_VERSION, MAX_NODE_HANDSHAKE_BYTES, NODE_HANDSHAKE_TIMEOUT, NodeSummary,
        NodeWsMessage, RpcResponse,
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

pub async fn join_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    let client_ip = peer.ip();
    if state.node_registry.join_rate_limited(client_ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed identity attempts",
        )
            .into_response();
    }
    ws.max_frame_size(64 * 1024 * 1024)
        .max_message_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| handle_join(socket, state, client_ip))
}

async fn handle_join(socket: WebSocket, state: AppState, client_ip: std::net::IpAddr) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // No registry entry exists during the bounded confirmation exchange.
    let deadline = tokio::time::Instant::now() + NODE_HANDSHAKE_TIMEOUT;
    let established =
        tokio::time::timeout_at(deadline, primary_handshake(&mut ws_tx, &mut ws_rx, &state)).await;
    let (name, mut phase) = match established {
        Ok(Ok(connection)) => connection,
        result => {
            state.node_registry.record_join_failure(client_ip).await;
            let reason = match result {
                Ok(Err(err)) => err.to_string(),
                Err(_) => "node identity handshake timed out".to_string(),
                _ => unreachable!(),
            };
            send_error(&mut ws_tx, &reason).await;
            return;
        }
    };
    let (send_tx, mut send_rx) = mpsc::channel::<NodeWsMessage>(64);
    let pending: Arc<Mutex<HashMap<String, PendingRpc>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_recv = Arc::clone(&pending);
    let handle = NodeHandle { send_tx, pending };
    let generation = match state.node_registry.connect(name.clone(), handle).await {
        Ok(generation) => generation,
        Err(err) => {
            let _ = tokio::time::timeout_at(
                deadline,
                send_node_message(
                    &mut ws_tx,
                    &NodeWsMessage::Error {
                        message: err.to_string(),
                    },
                    &mut phase,
                ),
            )
            .await;
            return;
        }
    };
    if !matches!(
        tokio::time::timeout_at(
            deadline,
            send_node_message(&mut ws_tx, &NodeWsMessage::Joined, &mut phase)
        )
        .await,
        Ok(Ok(()))
    ) {
        state.node_registry.disconnect(&name, generation).await;
        return;
    }
    info!(node = %name, "secondary node connected after encrypted readiness");

    // ── Step 6: relay loop (single task, select! on send_rx and ws_rx) ───
    // Only authenticated application frames refresh liveness. Unsealed
    // transport pongs cannot keep an identity-authenticated link alive.
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(15));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (inbound_tx, inbound_rx) = watch::channel(tokio::time::Instant::now());
    let relay = async {
        loop {
            tokio::select! {
                _ = keepalive.tick() => {
                    if let Err(err) = send_node_message(&mut ws_tx, &NodeWsMessage::Ping, &mut phase).await {
                        break format!("failed to send sealed node keepalive: {err}");
                    }
                }
                // Outgoing: channel → WS
                msg = send_rx.recv() => {
                    let Some(ws_msg) = msg else {
                        break "node RPC relay channel closed".to_string();
                    };
                    if let Err(err) = send_node_message(&mut ws_tx, &ws_msg, &mut phase).await {
                        break format!("failed to send proxied RPC to node WebSocket: {err}");
                    }
                }
                // Incoming: WS → resolve pending RPC callers
                incoming = ws_rx.next() => {
                    match incoming {
                        Some(Ok(frame)) => {
                            // Tungstenite auto-frames Ping / Pong on the
                            // WebSocket transport — silently drop ones that
                            // surface here so we do not emit "failed to
                            // decode secondary node frame" warnings on every
                            // 15s keepalive tick.
                            match frame {
                                Message::Ping(_) | Message::Pong(_) => continue,
                                _ => {}
                            }
                            let message = match frame {
                                Message::Close(frame) => {
                                    break close_frame_disconnect_reason(frame);
                                }
                                other => match parse_node_message(other, &mut phase) {
                                    Ok(message) => message,
                                    Err(err) => {
                                        break format!("invalid sealed node frame: {err}");
                                    }
                                },
                            };
                            inbound_tx.send_replace(tokio::time::Instant::now());
                            match message {
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
                                                        if tokio::time::timeout(NODE_SEND_TIMEOUT, tx.send(Ok(rpc_resp))).await.is_err() {
                                                            break "node RPC response queue stalled for 15s".to_string();
                                                        }
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
                                                            if tokio::time::timeout(NODE_SEND_TIMEOUT, tx.send(Ok(rpc_resp))).await.is_err() {
                                                                break "node stream response queue stalled for 15s".to_string();
                                                            }
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
                                                    match tokio::time::timeout(NODE_SEND_TIMEOUT, tx.send(Ok(rpc_resp))).await {
                                                        Ok(Ok(())) => {},
                                                        Ok(Err(_)) => {
                                                            pending_recv.lock().await.remove(&id);
                                                        }
                                                        Err(_) => {
                                                            break format!("node stream response queue stalled for 15s for RPC {id}");
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    NodeWsMessage::Ping => {
                                        if let Err(err) = send_node_message(
                                            &mut ws_tx,
                                            &NodeWsMessage::Pong,
                                            &mut phase,
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
                                        last_total_bytes,
                                        enabled_for_channels,
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
                                            last_total_bytes,
                                            enabled_for_channels,
                                        };
                                        handle_forwarded_session_event(&state, &name, payload)
                                            .await;
                                    }
                                    NodeWsMessage::SessionEvent { payload } => {
                                        handle_forwarded_session_event(&state, &name, payload)
                                            .await;
                                    }
                                    NodeWsMessage::Pong => {},
                                    _ => break "unexpected message in established secondary channel".to_string(),
                            }
                        }
                        Some(Err(err)) => break format!("node WebSocket receive error: {err}"),
                        None => break "node WebSocket stream ended".to_string(),
                    }
                }
            }
        }
    };
    let disconnect_reason = tokio::select! {
        _ = wait_for_inbound_timeout(inbound_rx) => "node keepalive timeout (no authenticated inbound frames for 45s)".to_string(),
        reason = relay => reason,
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
                    // Never let a full abandoned stream queue block registry
                    // cleanup. Dropping its sender still closes the receiver.
                    let _ = tx.try_send(Err(err()));
                }
            }
        }
        drained_waiters
    };

    state.node_registry.disconnect(&name, generation).await;
    warn!(node = %name, reason = %disconnect_reason, drained_waiters, "secondary node disconnected");
}

async fn read_handshake_frame(
    rx: &mut futures_util::stream::SplitStream<WebSocket>,
) -> crate::error::Result<Vec<u8>> {
    match rx.next().await {
        Some(Ok(Message::Binary(bytes))) if bytes.len() <= MAX_NODE_HANDSHAKE_BYTES => {
            Ok(bytes.to_vec())
        }
        _ => Err(crate::error::AppError::Protocol(
            "expected bounded binary handshake frame".into(),
        )),
    }
}

async fn primary_handshake(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    rx: &mut futures_util::stream::SplitStream<WebSocket>,
    state: &AppState,
) -> crate::error::Result<(String, federation_crypto::Channel)> {
    use crate::error::AppError;
    let fail = |message: &str| AppError::Protocol(message.to_string());
    let signing_key = state
        .node_identity
        .signing_key()
        .ok_or_else(|| fail("node identity unavailable; federation disabled"))?;
    let hello: NodeWsMessage =
        federation_crypto::decode_handshake(&read_handshake_frame(rx).await?)?;
    let NodeWsMessage::Hello {
        version,
        name,
        public_key,
        nonce,
    } = hello
    else {
        return Err(fail("expected hello"));
    };
    if version != FEDERATION_VERSION {
        return Err(fail("unsupported federation version"));
    }
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(fail("invalid node name"));
    }
    let peer = federation_crypto::parse_public_key(&public_key)?;
    let primary_nonce = federation_crypto::fresh_nonce();
    // A fresh challenge is minted for this socket only. No in-band trust root.
    let challenge = federation_crypto::encode_handshake(&NodeWsMessage::Challenge {
        nonce: primary_nonce,
    })?;
    tx.send(Message::Binary(challenge.into()))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let transcript = federation_crypto::build_transcript(
        version,
        "secondary-proof",
        &name,
        peer.as_bytes(),
        signing_key.verifying_key().as_bytes(),
        &nonce,
        &primary_nonce,
    );
    let NodeWsMessage::Join { signature } =
        federation_crypto::decode_handshake(&read_handshake_frame(rx).await?)?
    else {
        return Err(fail("expected identity join proof"));
    };
    let registered = state.db.get_node_identity(&name).await?;
    if registered.is_none_or(|(_, key)| key != peer.to_line())
        || !federation_crypto::verify_transcript(&peer, &signature, &transcript)
    {
        return Err(fail("unauthorized node identity or transcript"));
    }
    let mut channel = federation_crypto::Channel::new(&signing_key, &peer, &transcript, false)?;
    let hash = channel.transcript_hash();
    let confirmation = channel.encode_message(&NodeWsMessage::KeyConfirm {
        transcript_hash: hash,
    })?;
    tx.send(Message::Binary(confirmation.into()))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    match channel.decode_message(&read_handshake_frame(rx).await?)? {
        NodeWsMessage::Ready { transcript_hash } if transcript_hash == hash => Ok((name, channel)),
        _ => Err(fail("expected encrypted readiness for this transcript")),
    }
}

async fn handle_forwarded_session_event(state: &AppState, node_name: &str, payload: SessionEvent) {
    let delivered = payload.for_delivery(Some(node_name));

    if let SessionEvent::SessionNotification {
        kind,
        title,
        description,
        body,
        navigation_url,
        session_ids,
        trigger_rule,
        trigger_detail,
        enabled_for_channels,
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
            "program_status" => Some(NotificationKind::ProgramStatus),
            "startup_recovery" => Some(NotificationKind::StartupRecovery),
            _ => None,
        };

        if *enabled_for_channels {
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
                let outcome = state.notifier.load_full().dispatch(&event).await;
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

async fn send_error(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &str,
) {
    let msg = NodeWsMessage::Error {
        message: message.to_string(),
    };
    if let Ok(payload) = federation_crypto::encode_handshake(&msg) {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            ws_tx.send(Message::Binary(payload.into())),
        )
        .await;
    }
}

async fn send_node_message(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &NodeWsMessage,
    phase: &mut federation_crypto::Channel,
) -> Result<(), String> {
    let payload = match phase.encode_message(message) {
        Ok(p) => p,
        Err(err) => {
            warn!(%err, "failed to encode node WebSocket frame");
            return Err(err.to_string());
        }
    };
    tokio::time::timeout(
        NODE_SEND_TIMEOUT,
        ws_tx.send(Message::Binary(payload.into())),
    )
    .await
    .map_err(|_| "node WebSocket send timed out after 15s".to_string())?
    .map_err(|err| err.to_string())
}

fn parse_node_message(
    frame: Message,
    phase: &mut federation_crypto::Channel,
) -> std::io::Result<NodeWsMessage> {
    match frame {
        Message::Binary(data) => phase.decode_message(&data).map_err(std::io::Error::other),
        _ => Err(std::io::Error::other("unsupported node WebSocket frame")),
    }
}

fn close_frame_disconnect_reason(frame: Option<axum::extract::ws::CloseFrame>) -> String {
    match frame {
        Some(frame) => format!("peer sent close frame: {frame:?}"),
        None => "peer sent close frame".to_string(),
    }
}
