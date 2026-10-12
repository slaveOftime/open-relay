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
    federation_crypto, ipc,
    node::NodeRegistry,
    protocol::{
        FEDERATION_VERSION, MAX_NODE_HANDSHAKE_BYTES, NODE_HANDSHAKE_TIMEOUT, NodeWsMessage,
        RpcRequest, RpcResponse,
    },
    session::SessionEvent,
};

/// Outcome of the first attempt of a join connector. Reported exactly
/// once (via `oneshot`) to whatever spawned the connector; subsequent
/// retries/disconnects use the normal `Backoff` loop without further
/// notification through this channel.
#[derive(Debug)]
pub(crate) enum JoinAttempt {
    /// WS handshake completed and `NodeWsMessage::Joined` was received.
    Connected,
    /// WS handshake (or its prerequisites) failed before reaching
    /// `Joined`; carries the reason from the connector.
    Failed(String),
}

/// One-shot reporter for the first-attempt outcome. Holding this in the
/// connector's state means we never forget to fire (or double-fire) the
/// signal even across deep retry paths; `joined`/`fail` consume the
/// inner sender so a later disconnect cannot accidentally signal again.
struct AttemptReporter(Option<tokio::sync::oneshot::Sender<JoinAttempt>>);

impl AttemptReporter {
    fn empty() -> Self {
        Self(None)
    }
    fn new(tx: tokio::sync::oneshot::Sender<JoinAttempt>) -> Self {
        Self(Some(tx))
    }
    fn joined(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(JoinAttempt::Connected);
        }
    }
    fn fail(&mut self, msg: impl Into<String>) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(JoinAttempt::Failed(msg.into()));
        }
    }
}

pub(super) fn spawn_join_connector(
    join: JoinConfig,
    local_config: Arc<AppConfig>,
    session_event_tx: broadcast::Sender<SessionEvent>,
    on_attempt: Option<tokio::sync::oneshot::Sender<JoinAttempt>>,
) -> (tokio::task::AbortHandle, watch::Sender<bool>) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let reporter = match on_attempt {
        Some(tx) => AttemptReporter::new(tx),
        None => AttemptReporter::empty(),
    };
    let session_event_rx = session_event_tx.subscribe();
    let task = tokio::spawn(async move {
        run_join_connector(
            join,
            local_config,
            session_event_tx,
            session_event_rx,
            stop_rx,
            reporter,
        )
        .await;
    });
    (task.abort_handle(), stop_tx)
}

async fn run_join_connector(
    join: JoinConfig,
    local_config: Arc<AppConfig>,
    session_event_tx: broadcast::Sender<SessionEvent>,
    mut session_event_rx: broadcast::Receiver<SessionEvent>,
    mut stop_rx: watch::Receiver<bool>,
    mut attempt_report: AttemptReporter,
) {
    const BACKOFF: &[u64] = &[1, 2, 4, 8, 16, 32, 60];
    let mut attempt = 0usize;

    loop {
        match connect_and_relay(
            &join,
            &local_config,
            &session_event_tx,
            &mut session_event_rx,
            &mut stop_rx,
            &mut attempt_report,
        )
        .await
        {
            Ok(true) => {
                info!(node = %join.name, "join connector stopped");
                return;
            }
            Ok(false) => {
                warn!(node = %join.name, "join connector disconnected");
            }
            Err(err) => {
                // connect_and_relay already converts the reason into a
                // `Failed(...)` signal when appropriate, but the helper
                // can't reach every path; firefall here for any escape.
                attempt_report.fail(err.to_string());
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
    session_event_tx: &broadcast::Sender<SessionEvent>,
    session_event_rx: &mut broadcast::Receiver<SessionEvent>,
    stop_rx: &mut watch::Receiver<bool>,
    attempt_report: &mut AttemptReporter,
) -> Result<bool> {
    let base = join.primary_url.trim_end_matches('/');
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
        Ok(Err(err)) => {
            attempt_report.fail(err.to_string());
            return Err(err);
        }
        Err(_) => {
            let msg = "node identity handshake timed out before encrypted Joined";
            attempt_report.fail(msg);
            return Err(crate::error::AppError::Protocol(msg.into()));
        }
    };
    info!(node = %join.name, primary = %join.primary_url, "joined primary after encrypted confirmation");
    attempt_report.joined();

    let (stream_frame_tx, mut stream_frame_rx) = mpsc::channel::<(String, RpcResponse, bool)>(256);
    // Open streaming RPCs: rpc_id -> inbound client-message channel.
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
                if !send_node_message(&mut ws_tx, &reply, &mut phase).await {
                    break;
                }
            }
            incoming = ws_rx.next() => {
                let Some(msg_result) = incoming else { break };

                let frame = match msg_result {
                    Ok(WsMessage::Close(_)) | Err(_) => break,
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

                match node_msg {
                    NodeWsMessage::Rpc { id, request } => {
                        let response = match serde_json::from_value::<RpcRequest>(request) {
                            Ok(req) if is_supported_proxied_rpc(&req) => {
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

                                match ipc::send_request(local_config, req).await {
                                    Ok(r) => r,
                                    Err(err) => RpcResponse::Error {
                                        message: err.to_string(),
                                    },
                                }
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
                        if !send_node_message(&mut ws_tx, &reply, &mut phase).await {
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
                        if !send_node_message(&mut ws_tx, &NodeWsMessage::Pong, &mut phase).await { break; }
                    }
                    NodeWsMessage::Pong => {},
                    _ => break,
                }
            }
            session_event = session_event_rx.recv() => {
                match session_event {
                    Ok(event) => {
                        let relay = NodeWsMessage::from_session_event(&event, Some(join.name.as_str()));
                        debug!(node = %join.name, event = ?event, "relaying session event to primary");
                        if !send_node_message(&mut ws_tx, &relay, &mut phase).await {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(node = %join.name, skipped, "session event relay lagged");
                        // CR-4: surface `resync_required` so the primary's
                        // SSE listener knows the relay's view diverged and
                        // that any subsequent activity byte counters lost
                        // their basis. Continue this loop on lag; the
                        // encoding pipeline below will resume on the
                        // next delivered frame.
                        let _ = session_event_tx.send(SessionEvent::ResyncRequired {
                            // This daemon owns the dropped events. The primary
                            // applies our registered secondary name on delivery.
                            node: None,
                            reason: Some("relay_lagged".to_string()),
                        });
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(false)
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
    join: &JoinConfig,
    config: &AppConfig,
) -> Result<(NodeSink, NodeSource, federation_crypto::Channel)> {
    use crate::error::AppError;
    let fail = |message: &str| AppError::Protocol(message.to_string());
    let seed =
        federation_crypto::load_seed(&federation_crypto::seed_path(&config.paths.state_dir))?;
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let primary = federation_crypto::parse_public_key(&join.primary_identity_pub_key)?;
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
        name: join.name.clone(),
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
        &join.name,
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
) -> bool {
    let payload = match phase.encode_message(message) {
        Ok(p) => p,
        Err(err) => {
            warn!(%err, "failed to encode node connector frame");
            return false;
        }
    };
    ws_tx.send(WsMessage::Binary(payload.into())).await.is_ok()
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
            | RpcRequest::List { .. }
            | RpcRequest::Start { .. }
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

pub(super) async fn handle_node_accept(
    name: String,
    identity_pub_key: String,
    db: &Arc<Database>,
) -> RpcResponse {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return RpcResponse::Error {
            message: "invalid node name".into(),
        };
    }
    // Validate and normalize the key so lookups at join time (which use the
    // canonical form) always match, regardless of how the key was pasted in.
    let canonical = match crate::federation_crypto::parse_public_key(&identity_pub_key) {
        Ok(key) => key.to_line(),
        Err(e) => {
            return RpcResponse::Error {
                message: e.to_string(),
            };
        }
    };
    if let Err(e) = db.register_node_identity(&name, &canonical).await {
        return RpcResponse::Error {
            message: format!("failed to register identity pubkey: {e}"),
        };
    }
    info!(node = %name, "registered accepted identity pubkey");
    RpcResponse::Empty
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

    // Local IPC attach streams switch to binary frames after the init
    // line (ADR-0004) — this relay is no exception.
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

    // ── AttemptReporter — first-attempt sync join feedback ─────────────────────
    //
    // The reporter is the channel `JoinStart` uses to tell the CLI whether
    // the first handshake completed, failed, or is still in progress; the
    // tests below pin its contract: `joined` consumes the inner sender
    // exactly once, `fail` does the same, and an empty reporter stays
    // silent (which is what lifecycle boot relies on when nobody is
    // waiting on the IPC path).

    #[test]
    fn attempt_reporter_empty_silently_drops_reports() {
        let mut reporter = super::AttemptReporter::empty();
        // Both report methods must be safe no-ops without a sender.
        reporter.joined();
        reporter.fail("ignored");
    }

    #[test]
    fn attempt_reporter_joined_delivers_connected_exactly_once() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut reporter = super::AttemptReporter::new(tx);
        reporter.joined();
        reporter.joined(); // second call must not panic (sender already taken).
        let outcome = rx.blocking_recv().expect("oneshot deliver");
        assert!(
            matches!(outcome, super::JoinAttempt::Connected),
            "expected Connected, got {outcome:?}"
        );
    }

    #[test]
    fn attempt_reporter_fail_delivers_failed_with_message() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut reporter = super::AttemptReporter::new(tx);
        reporter.fail("join rejected: unauthorized");
        // Subsequent joined() must be a no-op even after a failure report.
        reporter.joined();
        let outcome = rx.blocking_recv().expect("oneshot deliver");
        match outcome {
            super::JoinAttempt::Failed(msg) => {
                assert_eq!(msg, "join rejected: unauthorized");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
