use interprocess::local_socket::tokio::Stream;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    sync::{broadcast, mpsc, watch},
};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, info, warn};

use crate::{
    config::AppConfig,
    error::Result,
    federation_crypto, ipc,
    node::config::{ConnectorState, NodeStatus, PrimaryConnection},
    node::{
        NodeRegistry,
        liveness::{NODE_SEND_TIMEOUT, wait_for_inbound_timeout},
    },
    protocol::{
        FEDERATION_VERSION, MAX_NODE_HANDSHAKE_BYTES, NODE_HANDSHAKE_TIMEOUT, NodeWsMessage,
        RpcRequest, RpcResponse,
    },
    session::SessionEvent,
};

/// Owns cancellation and derived outbound diagnostics.
pub(crate) struct NodeConnector {
    task: tokio::task::JoinHandle<()>,
    stop_tx: watch::Sender<bool>,
    state: Arc<RwLock<ConnectorState>>,
}

impl NodeConnector {
    pub(crate) fn state(&self) -> ConnectorState {
        self.state.read().unwrap().clone()
    }

    /// Await teardown so successful mutations do not leave an old relay alive.
    pub(crate) async fn stop(mut self) {
        let _ = self.stop_tx.send(true);
        // Cooperative cancellation reaches the descendant cleanup barrier.
        // Abort is only a bounded fallback for an unexpectedly stuck task.
        if tokio::time::timeout(NODE_SEND_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

impl Drop for NodeConnector {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
        self.task.abort();
    }
}

pub(super) fn spawn_node_connector(
    join: PrimaryConnection,
    local_config: Arc<AppConfig>,
    session_event_tx: broadcast::Sender<SessionEvent>,
) -> NodeConnector {
    let (stop_tx, stop_rx) = watch::channel(false);
    let state = Arc::new(RwLock::new(ConnectorState::default()));
    let task_state = Arc::clone(&state);
    let task = tokio::spawn(run_node_connector(
        join,
        local_config,
        session_event_tx.subscribe(),
        stop_rx,
        task_state,
    ));
    NodeConnector {
        task,
        stop_tx,
        state,
    }
}

#[derive(Default)]
struct RetryBackoff {
    attempt: usize,
}

impl RetryBackoff {
    fn reset(&mut self) {
        self.attempt = 0;
    }

    fn next_wait_secs(&mut self) -> u64 {
        const BACKOFF: &[u64] = &[1, 2, 4, 8, 16, 32, 60];
        let wait = BACKOFF[self.attempt];
        self.attempt = (self.attempt + 1).min(BACKOFF.len() - 1);
        wait
    }
}

async fn run_node_connector(
    join: PrimaryConnection,
    local_config: Arc<AppConfig>,
    mut session_event_rx: broadcast::Receiver<SessionEvent>,
    mut stop_rx: watch::Receiver<bool>,
    state: Arc<RwLock<ConnectorState>>,
) {
    let mut backoff = RetryBackoff::default();
    loop {
        {
            let mut s = state.write().unwrap();
            s.status = NodeStatus::Connecting;
            s.next_retry_at = None;
        }
        let reason = match connect_and_relay(
            &join,
            &local_config,
            &mut session_event_rx,
            &mut stop_rx,
            &state,
            &mut backoff,
        )
        .await
        {
            Ok(true) => return,
            Ok(false) => "primary disconnected".to_string(),
            Err(err) => err.to_string(),
        };
        if *stop_rx.borrow() {
            return;
        }
        let wait = backoff.next_wait_secs();
        {
            let mut s = state.write().unwrap();
            s.status = NodeStatus::Retrying;
            s.last_error = Some(reason.clone());
            s.next_retry_at = Some(chrono::Utc::now() + chrono::Duration::seconds(wait as i64));
        }
        warn!(node = %join.name, %reason, wait_secs = wait, "node connector retrying");
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
            _ = stop_rx.changed() => return,
        }
    }
}

async fn connect_and_relay(
    join: &PrimaryConnection,
    local_config: &Arc<AppConfig>,
    session_event_rx: &mut broadcast::Receiver<SessionEvent>,
    stop_rx: &mut watch::Receiver<bool>,
    state: &Arc<RwLock<ConnectorState>>,
    backoff: &mut RetryBackoff,
) -> Result<bool> {
    let base = join.url.trim_end_matches('/');
    let ws_url = if base.starts_with("https://") {
        format!("{}/api/nodes/join", base.replacen("https://", "wss://", 1))
    } else {
        format!("{}/api/nodes/join", base.replacen("http://", "ws://", 1))
    };

    // Cancellation and one deadline cover DNS/TCP/WS plus all handshake IO.
    if *stop_rx.borrow() {
        return Ok(true);
    }
    let established = tokio::select! {
        biased;
        _ = stop_rx.changed() => return Ok(true),
        result = tokio::time::timeout(NODE_HANDSHAKE_TIMEOUT,
            secondary_handshake(&ws_url, join, local_config)) => result,
    };
    let (mut ws_tx, mut ws_rx, mut phase) = match established {
        Ok(Ok(connection)) => connection,
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            return Err(crate::error::AppError::Protocol(
                "node identity handshake timed out before encrypted Joined".into(),
            ));
        }
    };
    info!(node = %join.name, primary = %join.url, "connected to primary after encrypted confirmation");
    {
        let mut s = state.write().unwrap();
        s.status = NodeStatus::Connected;
        s.last_error = None;
        s.next_retry_at = None;
    }
    // An old outage must not leave the next independent outage at a 60s wait.
    backoff.reset();
    let (inbound_tx, inbound_rx) = watch::channel(tokio::time::Instant::now());

    // Keep the cancellation and authenticated-inbound watchdog outside every
    // relay await, including WebSocket sends and local channel operations.
    let mut relay_tasks = tokio::task::JoinSet::new();
    let relay = async {
        let (rpc_tx, mut rpc_rx) = mpsc::channel::<(String, RpcResponse)>(64);
        let (stream_frame_tx, mut stream_frame_rx) =
            mpsc::channel::<(String, RpcResponse, bool)>(256);
        // Open streaming RPCs: rpc_id -> inbound client-message channel.
        // Mid-stream messages from the primary (input/resize/credits/detach)
        // are routed into the stream's task, which writes them onto the
        // nested local IPC connection so the owning daemon's attachment-scoped
        // fencing and credit gate apply to remote clients exactly as local.
        let mut streams: HashMap<String, mpsc::Sender<RpcRequest>> = HashMap::new();
        loop {
            tokio::select! {
                finished = relay_tasks.join_next(), if !relay_tasks.is_empty() => {
                    if let Some(Err(err)) = finished {
                        return Err(crate::error::AppError::Protocol(format!("node relay task failed: {err}")));
                    }
                }
                response = rpc_rx.recv() => {
                    let Some((id, response)) = response else { break };
                    let reply = NodeWsMessage::RpcResponse { id, response: serde_json::to_value(response)? };
                    send_node_message(&mut ws_tx, &reply, &mut phase).await?;
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
                    send_node_message(&mut ws_tx, &reply, &mut phase).await?;
                }
                incoming = ws_rx.next() => {
                    let Some(msg_result) = incoming else { break };

                    let frame = match msg_result {
                        Ok(WsMessage::Close(frame)) => return Err(crate::error::AppError::Protocol(format!("primary sent close frame: {frame:?}"))),
                        Err(err) => return Err(crate::error::AppError::Protocol(format!("primary WebSocket receive error: {err}"))),
                        // Tungstenite already frames protocol-level Ping / Pong
                        // handling on the WebSocket layer (auto-pong on inbound
                        // pings, dropping inbound pongs), so anything left here
                        // is application traffic — silently skip these rather
                        // than emitting "unsupported node connector frame"
                        // warnings that would otherwise drown the log every
                        // keepalive tick.
                        Ok(WsMessage::Ping(_) | WsMessage::Pong(_)) => continue,
                        Ok(frame) => frame,
                    };
                    let node_msg = match decode_node_message(frame, &mut phase) {
                        Ok(message) => message,
                        Err(err) => {
                            warn!(node = %join.name, %err, "invalid sealed primary frame; closing");
                            break;
                        }
                    };

                    inbound_tx.send_replace(tokio::time::Instant::now());
                    match node_msg {
                        NodeWsMessage::Rpc { id, request } => {
                            if relay_tasks.len() >= 64 {
                                return Err(crate::error::AppError::Protocol("node relay task capacity exceeded".into()));
                            }
                            let response = match serde_json::from_value::<RpcRequest>(request) {
                                Ok(req) if is_supported_proxied_rpc(&req) => {
                                    if matches!(req, RpcRequest::AttachSubscribe { .. }) {
                                        let local_cfg = Arc::clone(local_config);
                                        let rpc_id = id.clone();
                                        let frame_tx = stream_frame_tx.clone();
                                        let (msg_tx, msg_rx) = mpsc::channel::<RpcRequest>(64);
                                        streams.insert(rpc_id.clone(), msg_tx);
                                        relay_tasks.spawn(async move {
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

                                    // Local RPCs can involve slow journal replay. They must
                                    // not prevent sealed keepalives or stop from being read.
                                    let local_cfg = Arc::clone(local_config);
                                    let reply_tx = rpc_tx.clone();
                                    relay_tasks.spawn(async move {
                                        let response = match ipc::send_request(&local_cfg, req).await {
                                            Ok(response) => response,
                                            Err(err) => RpcResponse::Error { message: err.to_string() },
                                        };
                                        let _ = reply_tx.send((id, response)).await;
                                    });
                                    continue;
                                }
                                Ok(req) => {
                                    warn!(%id, request_type = req.name(), "unsupported proxied RPC method");
                                    RpcResponse::Error {
                                        message: format!("unsupported proxied RPC method: {}", req.name()),
                                    }
                                }
                                Err(err) => {
                                    warn!(%err, id = %id, "failed to deserialise proxied RPC");
                                    RpcResponse::Error {
                                        message: format!("invalid proxied RPC request: {err}"),
                                    }
                                }
                            };

                            let response_json = match serde_json::to_value(&response) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };

                            let reply = NodeWsMessage::RpcResponse {
                                id,
                                response: response_json,
                            };
                            send_node_message(&mut ws_tx, &reply, &mut phase).await?;
                        }
                        NodeWsMessage::RpcStreamMessage { id, request } => {
                            match serde_json::from_value::<RpcRequest>(request) {
                                Ok(req) if is_stream_message_relayable(&req) => {
                                    match streams.get(&id) {
                                        Some(tx) => {
                                            match tokio::time::timeout(NODE_SEND_TIMEOUT, tx.send(req)).await {
                                                Ok(Ok(())) => {},
                                                Ok(Err(_)) => { streams.remove(&id); },
                                                Err(_) => {
                                                    return Err(crate::error::AppError::Protocol(format!("node attach input queue stalled for 15s for RPC {id}")));
                                                }
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
                            send_node_message(&mut ws_tx, &NodeWsMessage::Pong, &mut phase).await?;
                        }
                        NodeWsMessage::Pong => {},
                        _ => break,
                    }
                }
                session_event = next_relay_event(session_event_rx, &join.as_name) => {
                    let Some(event) = session_event else { break };
                    let relay = NodeWsMessage::from_session_event(&event, Some(join.as_name.as_str()));
                    debug!(node = %join.name, event = ?event, "relaying session event to primary");
                    send_node_message(&mut ws_tx, &relay, &mut phase).await?;
                }
            }
        }

        Ok(false)
    };
    let result = tokio::select! {
        biased;
        _ = stop_rx.changed() => Ok(true),
        _ = wait_for_inbound_timeout(inbound_rx) => Err(crate::error::AppError::Protocol(
            "primary keepalive timeout (no authenticated inbound frames for 45s)".into()
        )),
        result = relay => result,
    };
    drain_relay_tasks(&mut relay_tasks).await;
    result
}

async fn drain_relay_tasks(tasks: &mut tokio::task::JoinSet<()>) {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

/// Convert a local relay gap into a marker for this primary only. Publishing
/// it back into the source ring lets lagged connectors keep each other lagging
/// indefinitely (and flood stderr), especially after sleep or a stalled send.
async fn next_relay_event(
    rx: &mut broadcast::Receiver<SessionEvent>,
    node: &str,
) -> Option<SessionEvent> {
    match rx.recv().await {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!(node, skipped, "session event relay lagged");
            Some(SessionEvent::ResyncRequired {
                // The primary applies our registered secondary name on delivery.
                node: None,
                reason: Some("relay_lagged".to_string()),
            })
        }
        Err(broadcast::error::RecvError::Closed) => None,
    }
}

type NodeSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type NodeSink = futures_util::stream::SplitSink<NodeSocket, WsMessage>;
type NodeSource = futures_util::stream::SplitStream<NodeSocket>;

async fn read_handshake_frame(rx: &mut NodeSource) -> Result<Vec<u8>> {
    match rx.next().await {
        Some(Ok(WsMessage::Binary(bytes))) if bytes.len() <= MAX_NODE_HANDSHAKE_BYTES => {
            Ok(bytes.to_vec())
        }
        _ => Err(crate::error::AppError::Protocol(
            "expected bounded binary handshake frame".into(),
        )),
    }
}

async fn secondary_handshake(
    url: &str,
    join: &PrimaryConnection,
    config: &AppConfig,
) -> Result<(NodeSink, NodeSource, federation_crypto::Channel)> {
    use crate::error::AppError;
    let fail = |message: &str| AppError::Protocol(message.to_string());
    let seed =
        federation_crypto::load_seed(&federation_crypto::seed_path(&config.paths.state_dir))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let primary = federation_crypto::parse_public_key(&join.key)?;
    let secondary_nonce = federation_crypto::fresh_nonce();
    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(64 * 1024 * 1024))
        .max_frame_size(Some(64 * 1024 * 1024));
    let (socket, _) = tokio_tungstenite::connect_async_with_config(url, Some(ws_config), false)
        .await
        .map_err(|e| fail(&format!("WebSocket connect failed: {e}")))?;
    let (mut tx, mut rx) = socket.split();
    let hello = federation_crypto::encode_handshake(&NodeWsMessage::Hello {
        version: FEDERATION_VERSION,
        name: join.as_name.clone(),
        public_key: federation_crypto::public_key_line(&signing_key),
        nonce: secondary_nonce,
    })?;
    tx.send(WsMessage::Binary(hello.into()))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let challenge: NodeWsMessage =
        federation_crypto::decode_handshake(&read_handshake_frame(&mut rx).await?)?;
    let primary_nonce = match challenge {
        NodeWsMessage::Challenge { nonce } => nonce,
        NodeWsMessage::Error { message } => return Err(fail(&format!("join rejected: {message}"))),
        _ => return Err(fail("expected freshness challenge, not success")),
    };
    let transcript = federation_crypto::build_transcript(
        FEDERATION_VERSION,
        "secondary-proof",
        &join.as_name,
        signing_key.verifying_key().as_bytes(),
        primary.as_bytes(),
        &secondary_nonce,
        &primary_nonce,
    );
    let proof = federation_crypto::encode_handshake(&NodeWsMessage::Join {
        signature: federation_crypto::sign_transcript(&signing_key, &transcript),
    })?;
    tx.send(WsMessage::Binary(proof.into()))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let mut channel = federation_crypto::Channel::new(&signing_key, &primary, &transcript, true)?;
    let hash = channel.transcript_hash();
    // No plaintext fallback, including an attacker-supplied Joined.
    let confirmation = channel
        .decode_message(&read_handshake_frame(&mut rx).await?)
        .map_err(|err| {
            fail(&format!(
                "primary identity confirmation failed; verify the configured pin: {err}"
            ))
        })?;
    match confirmation {
        NodeWsMessage::KeyConfirm { transcript_hash } if transcript_hash == hash => {}
        _ => return Err(fail("expected encrypted primary key confirmation")),
    }
    let ready = channel.encode_message(&NodeWsMessage::Ready {
        transcript_hash: hash,
    })?;
    tx.send(WsMessage::Binary(ready.into()))
        .await
        .map_err(|e| fail(&e.to_string()))?;
    let acknowledgement = channel
        .decode_message(&read_handshake_frame(&mut rx).await?)
        .map_err(|err| fail(&format!("encrypted Joined verification failed: {err}")))?;
    match acknowledgement {
        NodeWsMessage::Joined => Ok((tx, rx, channel)),
        NodeWsMessage::Error { message } => Err(fail(&format!("join rejected: {message}"))),
        _ => Err(fail("expected encrypted Joined")),
    }
}

async fn send_node_message(
    ws_tx: &mut futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        WsMessage,
    >,
    message: &NodeWsMessage,
    phase: &mut federation_crypto::Channel,
) -> Result<()> {
    let payload = phase.encode_message(message)?;
    tokio::time::timeout(
        NODE_SEND_TIMEOUT,
        ws_tx.send(WsMessage::Binary(payload.into())),
    )
    .await
    .map_err(|_| {
        crate::error::AppError::Protocol("primary WebSocket send timed out after 15s".into())
    })?
    .map_err(|err| {
        crate::error::AppError::Protocol(format!("primary WebSocket send failed: {err}"))
    })
}

fn decode_node_message(
    frame: WsMessage,
    phase: &mut federation_crypto::Channel,
) -> std::io::Result<NodeWsMessage> {
    match frame {
        WsMessage::Binary(data) => decode_node_message_bytes(&data, phase),
        _ => Err(std::io::Error::other("unsupported node connector frame")),
    }
}

fn decode_node_message_bytes(
    bytes: &[u8],
    phase: &mut federation_crypto::Channel,
) -> std::io::Result<NodeWsMessage> {
    phase.decode_message(bytes).map_err(std::io::Error::other)
}

fn is_supported_proxied_rpc(request: &RpcRequest) -> bool {
    matches!(
        request,
        RpcRequest::Health
            | RpcRequest::DaemonStatus
            | RpcRequest::List { .. }
            | RpcRequest::Start { .. }
            | RpcRequest::SessionInfo { .. }
            | RpcRequest::SessionMetadataSet { .. }
            | RpcRequest::NotifySet { .. }
            | RpcRequest::NotifySend { .. }
            | RpcRequest::AttachSubscribe { .. }
            | RpcRequest::AttachInput { .. }
            | RpcRequest::AttachMouse { .. }
            | RpcRequest::SessionCursor { .. }
            | RpcRequest::ObserveWindow { .. }
            | RpcRequest::AttachBusy { .. }
            | RpcRequest::UploadFile { .. }
            | RpcRequest::AttachResize { .. }
            | RpcRequest::AttachDetach { .. }
            | RpcRequest::Stop { .. }
            | RpcRequest::Restart { .. }
            | RpcRequest::Kill { .. }
            | RpcRequest::Remove { .. }
            | RpcRequest::LogsRead { .. }
            | RpcRequest::LogsTail { .. }
            | RpcRequest::LogsPagination { .. }
    )
}

/// Mid-stream client messages the relay forwards into an open attach
/// stream. Everything else is rejected: the stream channel is not
/// a general RPC tunnel.
fn is_stream_message_relayable(request: &RpcRequest) -> bool {
    matches!(
        request,
        RpcRequest::AttachInput { .. }
            | RpcRequest::AttachResize { .. }
            | RpcRequest::AttachAppliedCursor { .. }
            | RpcRequest::AttachDetach { .. }
    )
}

/// Serve a relayed streaming attach on the secondary: open a nested local
/// IPC connection to this daemon (single implementation — the same
/// streaming handler local clients use), forward its frames to the
/// primary, and write mid-stream client messages from the primary onto
/// the connection so attachment-scoped fencing, observe-mode gating, and
/// applied-cursor credits apply to remote clients.
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
    // The nested IPC attach stream sends its init (or an error) as a
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
                    Err(err) => {
                        // The nested IPC connection ended before a completion
                        // frame was forwarded (owning daemon stopped, nested
                        // stream aborted, read failure). Report the
                        // interruption to the primary; fabricating
                        // `AttachStreamDone { exit_code: None,
                        // final_offset: 0 }` would present an unknown cursor
                        // as a confirmed end.
                        warn!(
                            %err,
                            id = %rpc_id,
                            "relay stream interrupted before completion"
                        );
                        let _ = frame_tx
                            .send((
                                rpc_id.to_string(),
                                relay_stream_interrupted_response(&err),
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

/// Response forwarded to the primary when a relayed stream's nested IPC
/// connection ends before a completion frame. Never fabricate
/// `AttachStreamDone` with unknown values: an interruption must stay loud.
fn relay_stream_interrupted_response(err: &dyn std::fmt::Display) -> RpcResponse {
    RpcResponse::Error {
        message: format!("relay stream interrupted: {err}"),
    }
}

/// Response written to the local client when the owning node's stream
/// channel is dropped without a completion frame (node disconnected, relay
/// task ended abnormally). Never fabricate `AttachStreamDone` here either.
fn node_stream_interrupted_response(node: &str) -> RpcResponse {
    RpcResponse::Error {
        message: format!("node '{node}' stream closed before completion"),
    }
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

/// Handle a node-proxied streaming attach: open `proxy_rpc_stream()` to the
/// secondary node and relay all streaming frames back to the CLI via IPC.
/// Client messages (input/resize/credits/detach) read from the IPC
/// reader are forwarded as mid-stream messages on the same stream, so
/// the owning node's attachment-scoped fencing and credit gate apply to
/// remote clients exactly as to local ones.
pub(super) async fn handle_node_proxy_streaming(
    node: String,
    inner: RpcRequest,
    reader: BufReader<tokio::io::ReadHalf<Stream>>,
    mut writer: tokio::io::WriteHalf<Stream>,
    node_registry: &Arc<NodeRegistry>,
) -> Result<()> {
    let stream = match node_registry.proxy_rpc_stream(&node, &inner).await {
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
    let stream_rpc_id = stream.id;
    let mut stream_rx = stream.responses;
    let mut cancelled = stream.cancelled;

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

    // Local IPC attach streams switch to binary frames after the init
    // line (ADR-0004) — this relay is no exception.
    let mut stream_binary = false;
    let mut writing_frame = false;

    let relay = async {
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
                            writing_frame = true;
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
                            writing_frame = false;
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
                            writing_frame = true;
                            let _ = if stream_binary {
                                ipc::write_attach_control_frame(&mut writer, &resp).await
                            } else {
                                ipc::write_response_to_writer(&mut writer, resp).await
                            };
                            break;
                        }
                        None => {
                            // The node stream channel was dropped without a
                            // completion or error frame: the owning node
                            // disconnected or its relay task ended abnormally.
                            // Report the interruption instead of fabricating a
                            // completion with `final_offset: 0`.
                            let resp = node_stream_interrupted_response(&node);
                            writing_frame = true;
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
    };
    if interrupt_on_revocation(&mut cancelled, relay).await && !writing_frame {
        // Only send an interruption at a frame boundary. If cancellation
        // interrupted a write, close rather than append to a partial frame.
        // A stalled client gets at most one second to accept the final error.
        let response = node_stream_interrupted_response(&node);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            if stream_binary {
                ipc::write_attach_control_frame(&mut writer, &response).await
            } else {
                ipc::write_response_to_writer(&mut writer, response).await
            }
        })
        .await;
    }
    client_reader_task.abort();
    let _ = client_reader_task.await;
    // Clean up the pending entry so it doesn't linger if the secondary
    // hasn't sent a done frame yet, and ask the owning node to end the
    // stream (which detaches the remote attachment).
    node_registry.remove_pending(&node, &stream_rpc_id).await;
    let _ = tokio::time::timeout(
        NODE_SEND_TIMEOUT,
        node_registry.proxy_rpc_stream_message(
            &node,
            &stream_rpc_id,
            &RpcRequest::AttachDetach { id: session_id },
        ),
    )
    .await;

    Ok(())
}

/// The whole relay future is cancellable, including a blocked IPC write.
async fn interrupt_on_revocation(
    cancelled: &mut watch::Receiver<bool>,
    relay: impl std::future::Future<Output = ()>,
) -> bool {
    if *cancelled.borrow() {
        return true;
    }
    tokio::select! {
        biased;
        _ = cancelled.changed() => true,
        _ = relay => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_stream_message_relayable, is_supported_proxied_rpc, node_stream_interrupted_response,
        relay_stream_interrupted_response,
    };
    use crate::protocol::{ListQuery, ListSortField, RpcRequest, RpcResponse, SortOrder};

    #[test]
    fn interrupted_relay_streams_report_errors_never_fabricated_completions() {
        let relay = relay_stream_interrupted_response(&"daemon closed the connection");
        match relay {
            RpcResponse::Error { message } => {
                assert!(message.contains("relay stream interrupted"));
                assert!(message.contains("daemon closed the connection"));
            }
            other => panic!("relay interruption must be an error, got {other:?}"),
        }
        let node = node_stream_interrupted_response("worker1");
        match node {
            RpcResponse::Error { message } => {
                assert!(message.contains("worker1"));
                assert!(message.contains("closed before completion"));
            }
            other => panic!("node stream interruption must be an error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn revocation_interrupts_a_proven_stalled_attach_output_write() {
        use tokio::io::AsyncReadExt;
        let (mut writer, mut reader) = tokio::io::duplex(8);
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task_completed = completed.clone();
        let task = tokio::spawn(async move {
            super::interrupt_on_revocation(&mut stop_rx, async {
                crate::ipc::write_attach_output_frame(&mut writer, 0, &[b'x'; 4096])
                    .await
                    .unwrap();
                task_completed.store(true, std::sync::atomic::Ordering::Release);
            })
            .await
        });
        // Observe the actual output frame header, then stop consuming. An
        // 8-byte buffer cannot accept the remaining 4096-byte body.
        let mut header = [0; 5];
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            reader.read_exact(&mut header),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(header[4], 1);
        stop_tx.send(true).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
        );
        assert!(!completed.load(std::sync::atomic::Ordering::Acquire));
        let mut remaining = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            reader.read_to_end(&mut remaining),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            remaining.len() < 4096,
            "revoked output writer retained its socket"
        );
    }

    #[test]
    fn retry_backoff_caps_and_resets_after_a_successful_join() {
        let mut backoff = super::RetryBackoff::default();
        let waits = (0..9).map(|_| backoff.next_wait_secs()).collect::<Vec<_>>();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        backoff.reset();
        assert_eq!(backoff.next_wait_secs(), 1);
        assert_eq!(backoff.next_wait_secs(), 2);
    }

    #[tokio::test]
    async fn connector_stop_awaits_teardown_even_during_stalled_io() {
        struct Teardown(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Teardown {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ended_tx, mut ended_rx) = tokio::sync::oneshot::channel();
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut descendants = tokio::task::JoinSet::new();
            descendants.spawn(async move {
                let _teardown = Teardown(Some(ended_tx));
                let _ = ready_tx.send(());
                std::future::pending::<()>().await;
            });
            let _ = stop_rx.changed().await;
            super::drain_relay_tasks(&mut descendants).await;
        });
        let connector = super::NodeConnector {
            task,
            stop_tx,
            state: std::sync::Arc::new(std::sync::RwLock::new(super::ConnectorState::default())),
        };
        ready_rx.await.unwrap();
        connector.stop().await;
        ended_rx
            .try_recv()
            .expect("descendant relay resources released before stop returns");
    }

    #[tokio::test]
    async fn lagged_relays_emit_gap_markers_without_feeding_the_source_ring() {
        use crate::{protocol::NodeWsMessage, session::SessionEvent};
        use tokio::sync::broadcast;

        let (tx, mut first) = broadcast::channel(1);
        let mut second = tx.subscribe();
        for connected in [true, false] {
            tx.send(SessionEvent::NodeState {
                node: "worker".into(),
                connected,
                last_seen: None,
            })
            .unwrap();
        }
        let source_len = tx.len();
        for rx in [&mut first, &mut second] {
            let marker = super::next_relay_event(rx, "secondary").await.unwrap();
            assert!(matches!(
                &marker,
                SessionEvent::ResyncRequired { node: None, reason: Some(reason) }
                    if reason == "relay_lagged"
            ));
            assert!(matches!(
                NodeWsMessage::from_session_event(&marker, Some("secondary")),
                NodeWsMessage::SessionEvent {
                    payload: SessionEvent::ResyncRequired { node: Some(node), .. }
                } if node == "secondary"
            ));
            assert_eq!(
                tx.len(),
                source_len,
                "gap reporting must not publish source events"
            );
            assert!(matches!(
                super::next_relay_event(rx, "secondary").await,
                Some(SessionEvent::NodeState {
                    connected: false,
                    ..
                })
            ));
            assert!(rx.is_empty(), "the relay must drain, not generate more lag");
        }
        drop(tx);
        assert!(
            super::next_relay_event(&mut first, "secondary")
                .await
                .is_none()
        );
    }

    /// Only attach stream messages may ride the mid-stream channel —
    /// it is not a general RPC tunnel.
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
    fn mouse_is_proxyable_but_not_an_attach_stream_message() {
        let request = RpcRequest::AttachMouse {
            id: "s".into(),
            spec: "wheel+3".into(),
        };
        assert!(is_supported_proxied_rpc(&request));
        assert!(!is_stream_message_relayable(&request));
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

    /// Session-scoped commands exposed by the CLI and web API must reach
    /// the owning daemon, while daemon administration stays local.
    #[test]
    fn session_scoped_methods_are_proxyable() {
        for req in [
            RpcRequest::SessionInfo { id: "s".into() },
            RpcRequest::SessionMetadataSet {
                id: "s".into(),
                title: Some("new title".into()),
                tags: Some(vec!["project".into()]),
                notifications_enabled: None,
            },
            RpcRequest::SessionCursor { id: "s".into() },
            RpcRequest::ObserveWindow {
                id: "s".into(),
                from: 0,
                max_bytes: 1024,
            },
            RpcRequest::Restart {
                id: "s".into(),
                force: false,
            },
            RpcRequest::Remove {
                id: "s".into(),
                force: false,
            },
        ] {
            assert!(is_supported_proxied_rpc(&req), "{} must relay", req.name());
            assert!(
                !is_stream_message_relayable(&req),
                "{} must not ride an attach stream",
                req.name()
            );
        }
        assert!(!is_supported_proxied_rpc(&RpcRequest::DaemonStop {
            grace_seconds: 0,
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
