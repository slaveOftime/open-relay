use std::{collections::HashMap, sync::Arc};

use axum::extract::ws::{Message, WebSocket};
use axum::{
    Json,
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

use crate::{
    http::AppState,
    node::{NodeHandle, PendingRpc},
    notification::event::{NotificationEvent, NotificationKind, NotificationTriggerRule},
    protocol::{
        NodeJoinAuth, NodeSummary, NodeWsMessage, RpcResponse, decode_node_ws_payload,
        encode_node_ws_payload,
    },
    session::SessionEvent,
    sshauth,
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
    // Rate-limit join attempts per IP to prevent brute-force API key guessing.
    let client_ip = peer.ip();
    if let Some(ref auth) = state.auth
        && let Some(locked_until) = auth.locked_until(client_ip).await
    {
        let secs = locked_until
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        warn!(ip = %client_ip, "node join: rate limited (locked for ~{}s)", secs);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, secs.to_string())],
            "too many failed attempts",
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| handle_join(socket, state, client_ip))
}

async fn handle_join(socket: WebSocket, state: AppState, client_ip: std::net::IpAddr) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ── Channel-mode state ───────────────────────────────────────────────────
    // Tracks whether we are still exchanging plaintext handshake frames
    // (Hello / Join) or post-handshake sealed traffic (the relay loop).
    // The socket flips to Sealed once the primary has SSH-key-authenticated
    // the secondary and derived the per-direction AES-256-GCM keys.
    let mut phase = sshauth::ChannelPhase::Plain;

    // ── Step 1: handshake — read Hello (optional for API-key flows) then
    // the mandatory Join. There is no in-band host-key challenge: the
    // primary's pubkey is whatever the operator pinned on the secondary
    // with `--ssh-pub-key`, so a man-in-the-middle has no opportunity to
    // substitute a different key during the handshake.
    let mut hello_record: Option<String> = None; // secondary's canonical pubkey line
    let (name, auth) = loop {
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

        match handshake {
            NodeWsMessage::Hello { public_key } => {
                // Stash the secondary's identity for channel-key
                // derivation post-credential-verification, then keep
                // waiting for the actual Join handshake. API-key flows
                // that never send Hello still work — they just stay Plain.
                hello_record = Some(public_key);
            }
            NodeWsMessage::Join { name, auth } => break (name, auth),
            _ => {
                send_error(&mut ws_tx, "expected join message").await;
                return;
            }
        }
    };

    // Capture the auth variant up front: the match below partially moves
    // string payloads out of `auth`, so we can't grep its tag at the
    // Sealed-phase decision point unless we record the choice here.
    let auth_was_ssh = matches!(&auth, NodeJoinAuth::SshKey { .. });

    // ── Step 2: validate authentication ─────────────────────────────────────────────
    let verified = match auth {
        NodeJoinAuth::ApiKey { key } => {
            let entries = match state.db.list_api_key_entries().await {
                Ok(e) => e,
                Err(_) => {
                    send_error(&mut ws_tx, "internal error").await;
                    return;
                }
            };

            let key_clone = key.clone();
            tokio::task::spawn_blocking(move || {
                !entries.is_empty()
                    && entries.iter().any(|(hash, scopes)| {
                        crate::http::auth::scopes_allow(scopes, crate::http::auth::SCOPE_NODE)
                            && crate::http::auth::verify_api_key_hash(&key_clone, hash)
                    })
            })
            .await
            .unwrap_or(false)
        }
        NodeJoinAuth::SshKey {
            signature,
            public_key,
        } => {
            // SSH-key auth proceeds without a per-connection host-key
            // challenge — the operator pinned the primary's pubkey on
            // the secondary out-of-band via `--ssh-pub-key`, and the
            // accept-table on this side is the trust anchor that maps a
            // node name to a registered signing pubkey. The signature
            // covers `NODE_JOIN_CONTEXT || name || pubkey`, which binds
            // it to the declared node identity.
            let canonical = match sshauth::normalize_public_key(&public_key) {
                Ok(k) => k,
                Err(_) => {
                    send_error(&mut ws_tx, "unauthorized").await;
                    return;
                }
            };
            match state.db.get_ssh_key_entry(&canonical).await {
                Ok(Some(_registered)) => {
                    let payload = sshauth::node_join_payload(&name, &canonical);
                    tokio::task::spawn_blocking(move || {
                        sshauth::verify_signature(&canonical, &signature, &payload)
                    })
                    .await
                    .unwrap_or(false)
                }
                Ok(None) => false,
                Err(_) => {
                    send_error(&mut ws_tx, "internal error").await;
                    return;
                }
            }
        }
    };

    if !verified {
        if let Some(ref auth) = state.auth {
            auth.record_failure(client_ip).await;
        }
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
    let (send_tx, mut send_rx) = mpsc::channel::<NodeWsMessage>(64);
    let pending: Arc<Mutex<HashMap<String, PendingRpc>>> = Arc::new(Mutex::new(HashMap::new()));
    let pending_recv = Arc::clone(&pending);

    let handle = NodeHandle { send_tx, pending };
    state.node_registry.connect(name.clone(), handle).await;

    // ── Step 4b: derive channel keys for SSH-key joins. The `Joined`
    // reply we send Plane is the *transition point* — both sides
    // seal from the next frame onwards, so neither side ever tries
    // to read a sealed message in pre-handshake phase. We only need the
    // secondary's canonical Ed25519 ssh-line (Hello's `public_key`); the
    // peer X25519 form is derived locally so it doesn't have to ride the
    // wire.
    let sealed_keys: Option<([u8; sshauth::AEAD_KEY_LEN], [u8; sshauth::AEAD_KEY_LEN])> =
        if auth_was_ssh {
            if let (Some(secondary_pub_line), Some(host_signing_key)) =
                (hello_record.as_ref(), state.node_identity.signing_key())
            {
                let secondary_canon =
                    match sshauth::normalize_public_key(secondary_pub_line) {
                        Ok(k) => k,
                        Err(err) => {
                            send_error(
                                &mut ws_tx,
                                &format!("hello carried an invalid ssh-ed25519 line: {err}"),
                            )
                            .await;
                            state.node_registry.disconnect(&name).await;
                            return;
                        }
                    };
                let secondary_pub_ed_bytes = match sshauth::b64_decode(
                    secondary_canon.split_whitespace().next_back().unwrap_or(""),
                ) {
                    Ok(b) if b.len() == 32 => {
                        let mut a = [0u8; 32];
                        a.copy_from_slice(&b);
                        a
                    }
                    _ => {
                        send_error(&mut ws_tx, "hello pubkey decode failed").await;
                        state.node_registry.disconnect(&name).await;
                        return;
                    }
                };
                let primary_pub_ed = {
                    let mut a = [0u8; 32];
                    a.copy_from_slice(&state.node_identity.public_key_bytes());
                    a
                };
                let priv_x = sshauth::ed25519_priv_to_x25519(&host_signing_key);
                let secondary_pub_x = sshauth::ed25519_pub_to_x25519(
                    &ed25519_dalek::VerifyingKey::from_bytes(&secondary_pub_ed_bytes)
                        .expect("secondary ed25519 pubkey from hello verified above"),
                );
                let keys = sshauth::derive_channel_keys(
                    &priv_x,
                    &secondary_pub_x,
                    &primary_pub_ed,
                    &secondary_pub_ed_bytes,
                );
                Some((keys.s2c, keys.c2s))
            } else {
                send_error(
                    &mut ws_tx,
                    "ssh-key join missing hello; refusing to fall back to plaintext",
                )
                .await;
                state.node_registry.disconnect(&name).await;
                return;
            }
        } else {
            None
        };

    // ── Step 5: send Joined (node is already visible in registry) ────────
    if send_node_message(&mut ws_tx, &NodeWsMessage::Joined, &phase)
        .await
        .is_err()
    {
        state.node_registry.disconnect(&name).await;
        return;
    }

    // Flip the channel to Sealed *after* Joined has been written, so the
    // primary's outbound RPCs, pongs, and notifications use AES-256-GCM
    // from here on. The connector performs the symmetric flip in its own
    // loop right after reading Joined.
    if let Some((send_key, recv_key)) = sealed_keys {
        phase.seal(send_key, recv_key);
    }

    // ── Step 6: relay loop (single task, select! on send_rx and ws_rx) ───
    // Keepalive (M5-3): protocol-level pings detect a silently dead
    // connection; any inbound frame (tungstenite auto-pongs included)
    // resets the liveness clock. Without this a half-open TCP connection
    // would leave proxied callers hanging until the next write fails.
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(15));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_inbound = std::time::Instant::now();
    let disconnect_reason = loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if last_inbound.elapsed() > std::time::Duration::from_secs(45) {
                    break "node keepalive timeout (no inbound frames for 45s)".to_string();
                }
                if let Err(err) = ws_tx.send(Message::Ping(Vec::new().into())).await {
                    break format!("failed to ping node WebSocket: {err}");
                }
            }
            // Outgoing: channel → WS
            msg = send_rx.recv() => {
                let Some(ws_msg) = msg else {
                    break "node RPC relay channel closed".to_string();
                };
                if let Err(err) = send_node_message(&mut ws_tx, &ws_msg, &phase).await {
                    break format!("failed to send proxied RPC to node WebSocket: {err}");
                }
            }
            // Incoming: WS → resolve pending RPC callers
            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(frame)) => {
                        last_inbound = std::time::Instant::now();
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
                            other => match parse_node_message(other, &phase) {
                                Ok(message) => message,
                                Err(err) => {
                                    warn!(node = %name, %err, "failed to decode secondary node frame");
                                    continue;
                                }
                            },
                        };
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
                                            if let Some(tx) = tx_clone
                                                && tx.send(Ok(rpc_resp)).await.is_err() {
                                                    let mut pm = pending_recv.lock().await;
                                                    pm.remove(&id);
                                                }
                                        }
                                    }
                                }
                                NodeWsMessage::Ping => {
                                    if let Err(err) = send_node_message(
                                        &mut ws_tx,
                                        &NodeWsMessage::Pong,
                                        &phase,
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
                                _ => {}
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
                    // A dropped future would silently lose the error frame.
                    let _ = tx.send(Err(err())).await;
                }
            }
        }
        drained_waiters
    };

    state.node_registry.disconnect(&name).await;
    warn!(node = %name, reason = %disconnect_reason, drained_waiters, "secondary node disconnected");
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
    if let Ok(payload) = encode_node_ws_payload(&msg) {
        let _ = ws_tx.send(Message::Binary(payload.into())).await;
    }
}

async fn send_node_message(
    ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &NodeWsMessage,
    phase: &sshauth::ChannelPhase,
) -> Result<(), String> {
    let payload = match sshauth::phase_encode_message(phase, message)
        .or_else(|_| encode_node_ws_payload(message))
    {
        Ok(p) => p,
        Err(err) => {
            warn!(%err, "failed to encode node WebSocket frame");
            return Err(err.to_string());
        }
    };
    ws_tx
        .send(Message::Binary(payload.into()))
        .await
        .map_err(|err| err.to_string())
}

fn parse_node_message(
    frame: Message,
    phase: &sshauth::ChannelPhase,
) -> std::io::Result<NodeWsMessage> {
    match frame {
        Message::Binary(data) => {
            sshauth::phase_decode_payload(phase, &data).map_err(std::io::Error::other)
        }
        _ => Err(std::io::Error::other("unsupported node WebSocket frame")),
    }
}

fn close_frame_disconnect_reason(frame: Option<axum::extract::ws::CloseFrame>) -> String {
    match frame {
        Some(frame) => format!("peer sent close frame: {frame:?}"),
        None => "peer sent close frame".to_string(),
    }
}
