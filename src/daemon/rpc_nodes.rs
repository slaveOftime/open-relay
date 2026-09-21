use interprocess::local_socket::tokio::Stream;
use std::{collections::HashMap, sync::Arc};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc, watch},
};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

use crate::{
    client::join::JoinConfig,
    config::AppConfig,
    db::Database,
    error::Result,
    ipc,
    node::NodeRegistry,
    protocol::{
        NodeJoinAuth, NodeWsMessage, RpcRequest, RpcResponse, decode_node_ws_payload,
        encode_node_ws_payload,
    },
    session::SessionEvent,
};

pub(super) fn spawn_join_connector(
    join: JoinConfig,
    local_config: Arc<AppConfig>,
    session_event_rx: broadcast::Receiver<SessionEvent>,
) -> (tokio::task::AbortHandle, watch::Sender<bool>) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        run_join_connector(join, local_config, session_event_rx, stop_rx).await;
    });
    (task.abort_handle(), stop_tx)
}

async fn run_join_connector(
    join: JoinConfig,
    local_config: Arc<AppConfig>,
    mut session_event_rx: broadcast::Receiver<SessionEvent>,
    mut stop_rx: watch::Receiver<bool>,
) {
    const BACKOFF: &[u64] = &[1, 2, 4, 8, 16, 32, 60];
    let mut attempt = 0usize;

    loop {
        match connect_and_relay(&join, &local_config, &mut session_event_rx, &mut stop_rx).await {
            Ok(true) => {
                info!(node = %join.name, "join connector stopped");
                return;
            }
            Ok(false) => {
                warn!(node = %join.name, "join connector disconnected");
            }
            Err(err) => {
                warn!(node = %join.name, %err, "join connector disconnected");
            }
        }

        if *stop_rx.borrow() {
            info!(node = %join.name, "join connector stopped");
            return;
        }

        let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
        warn!(node = %join.name, wait_secs = wait, "join connector retrying");
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
            _ = stop_rx.changed() => {
                info!(node = %join.name, "join connector stopped during backoff");
                return;
            }
        }
        attempt += 1;
    }
}

async fn connect_and_relay(
    join: &JoinConfig,
    local_config: &Arc<AppConfig>,
    session_event_rx: &mut broadcast::Receiver<SessionEvent>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<bool> {
    let base = join.primary_url.trim_end_matches('/');
    let ws_url = if base.starts_with("https://") {
        format!("{}/api/nodes/join", base.replacen("https://", "wss://", 1))
    } else {
        format!("{}/api/nodes/join", base.replacen("http://", "ws://", 1))
    };

    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| crate::error::AppError::Protocol(format!("WebSocket connect failed: {e}")))?;

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // ── Step 1: SSH-key authentication — fetch host key, receive the
    // primary's signed challenge, verify the host, then sign the join.
    let host = join
        .primary_url
        .strip_prefix("http://")
        .or_else(|| join.primary_url.strip_prefix("https://"))
        .and_then(|u| u.split('/').next())
        .map(|authority| {
            let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            // Strip a trailing :port unless that would break an IPv6 literal.
            match authority.rsplit_once(':') {
                Some((h, port)) if !h.is_empty() && port.parse::<u16>().is_ok() => h,
                _ => authority,
            }
        })
        .unwrap_or("localhost");

    let auth = if let Some(ssh_key_path) = &join.ssh_key_path {
        let signing_key = crate::sshauth::load_signing_key(std::path::Path::new(ssh_key_path))
            .map_err(|e| crate::error::AppError::Protocol(format!("ssh key auth: {e}")))?;
        let public_key = crate::sshauth::public_key_line(&signing_key);

        // Request the host-key challenge from the primary.
        let challenge_req = NodeWsMessage::GetHostKey;
        ws_tx
            .send(WsMessage::Binary(
                encode_node_ws_payload(&challenge_req)?.into(),
            ))
            .await
            .map_err(|e| {
                crate::error::AppError::Protocol(format!("host key request failed: {e}"))
            })?;

        let (host_public_key, nonce, host_signature) = match ws_rx.next().await {
            Some(Ok(frame)) => match decode_node_message(frame) {
                Ok(NodeWsMessage::HostKey {
                    public_key,
                    nonce,
                    host_signature,
                }) => (public_key, nonce, host_signature),
                Ok(NodeWsMessage::Error { message }) => {
                    return Err(crate::error::AppError::Protocol(format!(
                        "host key challenge rejected: {message}"
                    )));
                }
                Ok(_) => {
                    return Err(crate::error::AppError::Protocol(
                        "unexpected response to get_host_key".into(),
                    ));
                }
                Err(e) => {
                    return Err(crate::error::AppError::Protocol(format!(
                        "invalid host key challenge: {e}"
                    )));
                }
            },
            _ => {
                return Err(crate::error::AppError::Protocol(
                    "no response to get_host_key".into(),
                ));
            }
        };

        let nonce_bytes = crate::sshauth::b64_decode(&nonce).map_err(|e| {
            crate::error::AppError::Protocol(format!("invalid challenge nonce: {e}"))
        })?;
        let nonce: [u8; crate::sshauth::NONCE_LEN] = nonce_bytes.try_into().map_err(|_| {
            crate::error::AppError::Protocol("challenge nonce has unexpected length".into())
        })?;

        // The primary must prove ownership of its host key *on this
        // connection*. Abort before sending any credentials if not.
        if !crate::sshauth::verify_signature(
            &host_public_key,
            &host_signature,
            &crate::sshauth::host_challenge_payload(&nonce),
        ) {
            return Err(crate::error::AppError::Protocol(format!(
                "host key self-signature for {host} is invalid; aborting before credentials are sent"
            )));
        }

        // Pin/verify the host key against known_hosts (TOFU on first use).
        if let Some(kh_path) = &join.ssh_known_hosts {
            let kh = std::path::Path::new(kh_path);
            match crate::sshauth::lookup_known_hosts(kh, host, &host_public_key) {
                crate::sshauth::HostKeyStatus::Match => {}
                crate::sshauth::HostKeyStatus::Unknown => {
                    info!(node = %join.name, %host, "trust-on-first-use: pinning primary host key");
                    let _ = crate::sshauth::append_known_hosts(kh, host, &host_public_key);
                }
                crate::sshauth::HostKeyStatus::Mismatch => {
                    return Err(crate::error::AppError::Protocol(format!(
                        "host key verification failed for {host}: key mismatch in {kh_path} (possible MITM)"
                    )));
                }
            }
        }

        // Sign the challenge: binds protocol, node name, the primary's
        // fresh per-connection nonce, and our key.
        let payload = crate::sshauth::node_join_payload(&join.name, &nonce, &public_key);
        let signature = crate::sshauth::sign_b64(&signing_key, &payload);

        NodeJoinAuth::SshKey {
            signature,
            public_key,
        }
    } else {
        // API key authentication
        NodeJoinAuth::ApiKey {
            key: join.api_key.clone().unwrap_or_default(),
        }
    };
    let handshake = NodeWsMessage::Join {
        name: join.name.clone(),
        auth,
    };
    let handshake_payload = encode_node_ws_payload(&handshake)?;
    ws_tx
        .send(WsMessage::Binary(handshake_payload.into()))
        .await
        .map_err(|e| crate::error::AppError::Protocol(e.to_string()))?;

    match ws_rx.next().await {
        Some(Ok(frame)) => {
            match decode_node_message(frame)
                .map_err(|e| crate::error::AppError::Protocol(e.to_string()))?
            {
                NodeWsMessage::Joined => {
                    info!(node = %join.name, primary = %join.primary_url, "joined primary");
                }
                NodeWsMessage::Error { message } => {
                    return Err(crate::error::AppError::Protocol(format!(
                        "join rejected: {message}"
                    )));
                }
                _ => {
                    return Err(crate::error::AppError::Protocol(
                        "unexpected response to join".into(),
                    ));
                }
            }
        }
        _ => {
            return Err(crate::error::AppError::Protocol(
                "no response to join handshake".into(),
            ));
        }
    }

    let (stream_frame_tx, mut stream_frame_rx) = mpsc::channel::<(String, RpcResponse, bool)>(256);
    // Open streaming RPCs: rpc_id -> inbound client-message channel (M5-2).
    // Mid-stream messages from the primary (input/resize/credits/detach)
    // are routed into the stream's task, which writes them onto the
    // nested local IPC connection so the owning daemon's attachment-scoped
    // fencing and credit gate apply to remote clients exactly as local.
    let mut streams: HashMap<String, mpsc::Sender<RpcRequest>> = HashMap::new();
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    let _ = ws_tx.send(WsMessage::Close(None)).await;
                    return Ok(true);
                }
            }
            frame = stream_frame_rx.recv() => {
                let Some((id, response, done)) = frame else { break };
                if done {
                    // Stream finished: drop the inbound channel so a late
                    // mid-stream message fails loudly instead of vanishing.
                    streams.remove(&id);
                }
                let response_json = match serde_json::to_value(&response) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let reply = NodeWsMessage::RpcStreamFrame {
                    id,
                    response: response_json,
                    done,
                };
                if !send_node_message(&mut ws_tx, &reply).await {
                    break;
                }
            }
            incoming = ws_rx.next() => {
                let Some(msg_result) = incoming else { break };

                let node_msg = match msg_result {
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                    Ok(frame) => match decode_node_message(frame) {
                        Ok(message) => message,
                        Err(err) => {
                            warn!(node = %join.name, %err, "failed to decode primary node frame");
                            continue;
                        }
                    },
                };

                match node_msg {
                    NodeWsMessage::Rpc { id, request } => {
                        let req = match serde_json::from_value::<RpcRequest>(request) {
                            Ok(r) => {
                                if is_supported_proxied_rpc(&r) {
                                    r
                                } else {
                                    warn!(%id, request_type = r.name(), "unsupported proxied RPC method");
                                    continue;
                                }
                            }
                            Err(err) => {
                                warn!(%err, id = %id, "failed to deserialise proxied RPC");
                                continue;
                            }
                        };

                        if matches!(req, RpcRequest::AttachSubscribe { .. }) {
                            let local_cfg = Arc::clone(local_config);
                            let rpc_id = id.clone();
                            let frame_tx = stream_frame_tx.clone();
                            let (msg_tx, msg_rx) = mpsc::channel::<RpcRequest>(64);
                            streams.insert(rpc_id.clone(), msg_tx);
                            tokio::spawn(async move {
                                if let Err(err) =
                                    relay_streaming_rpc(&local_cfg, req, &rpc_id, &frame_tx, msg_rx).await
                                {
                                    warn!(%err, id = %rpc_id, "streaming relay failed");
                                    let resp = RpcResponse::Error {
                                        message: err.to_string(),
                                    };
                                    let _ = frame_tx.send((rpc_id, resp, true)).await;
                                }
                            });
                            continue;
                        }

                        let response = match ipc::send_request(local_config, req).await {
                            Ok(r) => r,
                            Err(err) => RpcResponse::Error {
                                message: err.to_string(),
                            },
                        };

                        let response_json = match serde_json::to_value(&response) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let reply = NodeWsMessage::RpcResponse {
                            id,
                            response: response_json,
                        };
                        if !send_node_message(&mut ws_tx, &reply).await {
                            break;
                        }
                    }
                    NodeWsMessage::RpcStreamMessage { id, request } => {
                        match serde_json::from_value::<RpcRequest>(request) {
                            Ok(req) if is_stream_message_relayable(&req) => {
                                match streams.get(&id) {
                                    Some(tx) => {
                                        if tx.send(req).await.is_err() {
                                            // Stream task ended without a done
                                            // frame yet; drop the route.
                                            streams.remove(&id);
                                        }
                                    }
                                    None => {
                                        debug!(%id, "mid-stream message for unknown/closed stream");
                                    }
                                }
                            }
                            Ok(other) => {
                                warn!(%id, request_type = other.name(), "rejected non-attach mid-stream message");
                            }
                            Err(err) => {
                                warn!(%id, %err, "failed to deserialise mid-stream message");
                            }
                        }
                    }
                    NodeWsMessage::Ping => {
                        let _ = send_node_message(&mut ws_tx, &NodeWsMessage::Pong).await;
                    }
                    _ => {}
                }
            }
            session_event = session_event_rx.recv() => {
                match session_event {
                    Ok(event) => {
                        let relay = NodeWsMessage::from_session_event(&event, Some(join.name.as_str()));
                        debug!(node = %join.name, event = ?event, "relaying session event to primary");
                        if !send_node_message(&mut ws_tx, &relay).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(node = %join.name, skipped, "session event relay lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(false)
}

async fn send_node_message(
    ws_tx: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMessage,
    >,
    message: &NodeWsMessage,
) -> bool {
    match encode_node_ws_payload(message) {
        Ok(payload) => ws_tx.send(WsMessage::Binary(payload.into())).await.is_ok(),
        Err(err) => {
            warn!(%err, "failed to encode node connector frame");
            false
        }
    }
}

fn decode_node_message(frame: WsMessage) -> std::io::Result<NodeWsMessage> {
    match frame {
        WsMessage::Binary(data) => decode_node_ws_payload(&data),
        _ => Err(std::io::Error::other("unsupported node connector frame")),
    }
}

fn is_supported_proxied_rpc(request: &RpcRequest) -> bool {
    matches!(
        request,
        RpcRequest::Health
            | RpcRequest::List { .. }
            | RpcRequest::Start { .. }
            | RpcRequest::NotifySet { .. }
            | RpcRequest::NotifySend { .. }
            | RpcRequest::AttachSubscribe { .. }
            | RpcRequest::AttachInput { .. }
            | RpcRequest::AttachBusy { .. }
            | RpcRequest::UploadFile { .. }
            | RpcRequest::AttachResize { .. }
            | RpcRequest::AttachDetach { .. }
            | RpcRequest::Stop { .. }
            | RpcRequest::Kill { .. }
            | RpcRequest::LogsWait { .. }
            | RpcRequest::LogsTail { .. }
            | RpcRequest::LogsPagination { .. }
    )
}

/// Mid-stream client messages the relay forwards into an open attach
/// stream (M5-2). Everything else is rejected: the stream channel is not
/// a general RPC tunnel.
fn is_stream_message_relayable(request: &RpcRequest) -> bool {
    matches!(
        request,
        RpcRequest::AttachInput { .. }
            | RpcRequest::AttachResize { .. }
            | RpcRequest::AttachAcquireControl { .. }
            | RpcRequest::AttachAppliedCursor { .. }
            | RpcRequest::AttachDetach { .. }
    )
}

/// Serve a relayed streaming attach on the secondary: open a nested local
/// IPC connection to this daemon (single implementation — the same
/// streaming handler local clients use), forward its frames to the
/// primary, and write mid-stream client messages from the primary onto
/// the connection so attachment-scoped fencing, control leases, and
/// applied-cursor credits apply to remote clients (M5-2).
async fn relay_streaming_rpc(
    config: &AppConfig,
    request: RpcRequest,
    rpc_id: &str,
    frame_tx: &mpsc::Sender<(String, RpcResponse, bool)>,
    mut msg_rx: mpsc::Receiver<RpcRequest>,
) -> Result<()> {
    let stream = ipc::connect(config).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    ipc::write_request_to_writer(&mut write_half, request).await?;

    // Mid-stream inbound channel state: open until the primary's channel
    // closes or the client detaches; afterwards we only drain frames until
    // the stream handler ends the stream.
    let mut msgs_open = true;
    // M6-3: the nested IPC attach stream sends its init (or an error) as a
    // JSON line, then switches to binary frames (ADR-0004). The relay
    // re-encodes them into the node-WS `RpcResponse` shape.
    let mut stream_binary = false;
    loop {
        tokio::select! {
            biased;

            // Mid-stream client messages from the primary: write onto the
            // nested connection. Channel closed = primary gone; closing
            // the write half makes the stream handler see EOF and detach.
            msg = msg_rx.recv(), if msgs_open => {
                let Some(req) = msg else {
                    msgs_open = false;
                    let _ = write_half.shutdown().await;
                    continue;
                };
                let is_detach = matches!(req, RpcRequest::AttachDetach { .. });
                if ipc::write_request_to_writer(&mut write_half, req).await.is_err() {
                    break;
                }
                if is_detach {
                    // The handler ends the stream; drain until it does.
                    msgs_open = false;
                }
            }

            frame = async {
                if stream_binary {
                    ipc::read_attach_frame(&mut reader).await.map(|frame| {
                        match frame {
                            ipc::AttachFrame::Output { offset, data } => {
                                RpcResponse::AttachStreamChunk { offset, data }
                            }
                            ipc::AttachFrame::Control(resp) => *resp,
                        }
                    })
                } else {
                    ipc::read_response_from_reader(&mut reader).await
                }
            } => {
                match frame {
                    Ok(resp) => {
                        if matches!(resp, RpcResponse::AttachStreamInit { .. }) {
                            stream_binary = true;
                        }
                        let is_done = matches!(
                            resp,
                            RpcResponse::AttachStreamDone { .. } | RpcResponse::Error { .. }
                        );
                        if frame_tx
                            .send((rpc_id.to_string(), resp, is_done))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if is_done {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = frame_tx
                            .send((
                                rpc_id.to_string(),
                                RpcResponse::AttachStreamDone {
                                    exit_code: None,
                                    final_offset: 0,
                                },
                                true,
                            ))
                            .await;
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

pub(super) async fn handle_node_proxy(
    node: String,
    inner: RpcRequest,
    node_registry: &Arc<NodeRegistry>,
) -> RpcResponse {
    match node_registry.proxy_rpc(&node, &inner).await {
        Ok(r) => r,
        Err(e) => RpcResponse::Error {
            message: e.to_string(),
        },
    }
}

pub(super) async fn handle_node_list(node_registry: &Arc<NodeRegistry>) -> RpcResponse {
    let nodes = node_registry.connected_names().await;
    RpcResponse::NodeList { nodes }
}

pub(super) async fn handle_node_accept_ssh_pubkey(
    name: String,
    public_key: String,
    db: &Arc<Database>,
) -> RpcResponse {
    // Validate and normalize the key so lookups at join time (which use the
    // canonical form) always match, regardless of how the key was pasted in.
    let canonical = match crate::sshauth::normalize_public_key(&public_key) {
        Ok(key) => key,
        Err(e) => {
            return RpcResponse::Error {
                message: e.to_string(),
            };
        }
    };
    if let Err(e) = db.insert_ssh_key_entry(&name, &canonical).await {
        return RpcResponse::Error {
            message: format!("failed to register SSH key: {e}"),
        };
    }
    info!(node = %name, "registered SSH public key");
    RpcResponse::Empty
}

/// Handle a node-proxied streaming attach: open `proxy_rpc_stream()` to the
/// secondary node and relay all streaming frames back to the CLI via IPC.
/// Client messages (input/resize/credits/control/detach) read from the IPC
/// reader are forwarded as mid-stream messages on the same stream (M5-2),
/// so the owning node's attachment-scoped fencing and credit gate apply to
/// remote clients exactly as to local ones.
pub(super) async fn handle_node_proxy_streaming(
    node: String,
    inner: RpcRequest,
    reader: BufReader<tokio::io::ReadHalf<Stream>>,
    mut writer: tokio::io::WriteHalf<Stream>,
    node_registry: &Arc<NodeRegistry>,
) -> Result<()> {
    let (stream_rpc_id, mut stream_rx) = match node_registry.proxy_rpc_stream(&node, &inner).await {
        Ok(pair) => pair,
        Err(e) => {
            ipc::write_response_to_writer(
                &mut writer,
                RpcResponse::Error {
                    message: e.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let (client_msg_tx, mut client_msg_rx) = mpsc::channel::<Result<RpcRequest>>(64);
    let client_reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match ipc::read_request_from_reader(&mut reader).await {
                Ok(req) => {
                    if client_msg_tx.send(Ok(req)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = client_msg_tx.send(Err(e)).await;
                    break;
                }
            }
        }
    });

    let session_id = match &inner {
        RpcRequest::AttachSubscribe { id, .. } => id.clone(),
        _ => String::new(),
    };

    // M6-3: local IPC attach streams switch to binary frames after the
    // init line (ADR-0004) — this relay is no exception.
    let mut stream_binary = false;

    loop {
        tokio::select! {
            biased;

            frame = stream_rx.recv() => {
                match frame {
                    Some(Ok(resp)) => {
                        let is_done = matches!(
                            resp,
                            RpcResponse::AttachStreamDone { .. } | RpcResponse::Error { .. }
                        );
                        let written = if stream_binary {
                            match resp {
                                RpcResponse::AttachStreamChunk { offset, data } => {
                                    ipc::write_attach_output_frame(&mut writer, offset, &data).await
                                }
                                other => {
                                    ipc::write_attach_control_frame(&mut writer, &other).await
                                }
                            }
                        } else {
                            if matches!(resp, RpcResponse::AttachStreamInit { .. }) {
                                stream_binary = true;
                            }
                            ipc::write_response_to_writer(&mut writer, resp).await
                        };
                        if written.is_err() {
                            break;
                        }
                        if is_done {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let resp = RpcResponse::Error {
                            message: e.to_string(),
                        };
                        let _ = if stream_binary {
                            ipc::write_attach_control_frame(&mut writer, &resp).await
                        } else {
                            ipc::write_response_to_writer(&mut writer, resp).await
                        };
                        break;
                    }
                    None => {
                        let resp = RpcResponse::AttachStreamDone {
                            exit_code: None,
                            final_offset: 0,
                        };
                        let _ = if stream_binary {
                            ipc::write_attach_control_frame(&mut writer, &resp).await
                        } else {
                            ipc::write_response_to_writer(&mut writer, resp).await
                        };
                        break;
                    }
                }
            }

            client_msg = client_msg_rx.recv() => {
                match client_msg {
                    Some(Ok(req)) => {
                        let is_detach = matches!(req, RpcRequest::AttachDetach { .. });
                        let _ = node_registry
                            .proxy_rpc_stream_message(&node, &stream_rpc_id, &req)
                            .await;
                        if is_detach {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    client_reader_task.abort();
    // Clean up the pending entry so it doesn't linger if the secondary
    // hasn't sent a done frame yet, and ask the owning node to end the
    // stream (which detaches the remote attachment).
    node_registry.remove_pending(&node, &stream_rpc_id).await;
    let _ = node_registry
        .proxy_rpc_stream_message(
            &node,
            &stream_rpc_id,
            &RpcRequest::AttachDetach { id: session_id },
        )
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_stream_message_relayable, is_supported_proxied_rpc};
    use crate::protocol::{ListQuery, ListSortField, RpcRequest, SortOrder};

    /// M5-2: only attach stream messages may ride the mid-stream channel
    /// — it is not a general RPC tunnel.
    #[test]
    fn only_attach_messages_are_relayable_mid_stream() {
        for req in [
            RpcRequest::AttachInput {
                id: "s".into(),
                data: b"x".to_vec(),
                wait_for_change: false,
                attachment_id: None,
            },
            RpcRequest::AttachResize {
                id: "s".into(),
                rows: 24,
                cols: 80,
            },
            RpcRequest::AttachAcquireControl { id: "s".into() },
            RpcRequest::AttachAppliedCursor {
                id: "s".into(),
                cursor: 7,
            },
            RpcRequest::AttachDetach { id: "s".into() },
        ] {
            assert!(is_stream_message_relayable(&req), "{req:?} must relay");
        }
        let other = RpcRequest::Kill { id: "s".into() };
        assert!(!is_stream_message_relayable(&other));
    }

    /// The serde default for `credited` is `false` (fail-open): an absent
    /// flag must never silently enable gating.
    #[test]
    fn subscribe_credited_defaults_to_false() {
        let decoded: RpcRequest =
            serde_json::from_str(r#"{"type":"attach_subscribe","id":"s","from_byte_offset":null}"#)
                .expect("decode without credited field");
        let RpcRequest::AttachSubscribe { credited, .. } = decoded else {
            panic!("wrong variant");
        };
        assert!(!credited, "absent credited must default to false");
    }

    #[test]
    fn proxied_detach_cleanup_is_supported() {
        assert!(is_supported_proxied_rpc(&RpcRequest::AttachDetach {
            id: "session-123".to_string(),
        }));
    }

    #[test]
    fn proxied_notify_send_is_supported() {
        assert!(is_supported_proxied_rpc(&RpcRequest::NotifySend {
            source: Some("session-123".to_string()),
            title: "Deploy ready".to_string(),
            description: Some("Build finished".to_string()),
            body: Some("Open the session for details.".to_string()),
            url: None,
        }));
    }

    #[test]
    fn nested_node_proxy_is_rejected() {
        assert!(!is_supported_proxied_rpc(&RpcRequest::NodeProxy {
            node: "secondary-a".to_string(),
            inner: Box::new(RpcRequest::List {
                query: ListQuery {
                    search: None,
                    tags: Vec::new(),
                    statuses: Vec::new(),
                    since: None,
                    until: None,
                    limit: 10,
                    offset: 0,
                    sort: ListSortField::CreatedAt,
                    order: SortOrder::Desc,
                },
            }),
        }));
    }
}
