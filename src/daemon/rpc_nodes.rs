use interprocess::local_socket::tokio::Stream;
use std::sync::Arc;
use tokio::{
    io::BufReader,
    sync::{broadcast, mpsc, watch},
};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::{
    client::join::JoinConfig,
    config::AppConfig,
    error::Result,
    http::sse::session_event_for_delivery,
    ipc,
    node::NodeRegistry,
    protocol::{
        NodeWsMessage, RpcRequest, RpcResponse, decode_node_ws_payload, encode_node_ws_payload,
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

    let handshake = NodeWsMessage::Join {
        name: join.name.clone(),
        key: join.api_key.clone(),
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
                            let local_cfg = Arc::clone(&local_config);
                            let rpc_id = id.clone();
                            let frame_tx = stream_frame_tx.clone();
                            tokio::spawn(async move {
                                if let Err(err) =
                                    relay_streaming_rpc(&local_cfg, req, &rpc_id, &frame_tx).await
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
                    NodeWsMessage::Ping => {
                        let _ = send_node_message(&mut ws_tx, &NodeWsMessage::Pong).await;
                    }
                    _ => {}
                }
            }
            session_event = session_event_rx.recv() => {
                match session_event {
                    Ok(event) => {
                        let relay = NodeWsMessage::SessionEvent {
                            payload: session_event_for_delivery(&event, Some(join.name.as_str())),
                        };
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
        RpcRequest::Health { .. }
            | RpcRequest::List { .. }
            | RpcRequest::Start { .. }
            | RpcRequest::NotifySet { .. }
            | RpcRequest::NotifySend { .. }
            | RpcRequest::AttachSubscribe { .. }
            | RpcRequest::AttachInput { .. }
            | RpcRequest::AttachResize { .. }
            | RpcRequest::AttachDetach { .. }
            | RpcRequest::Stop { .. }
            | RpcRequest::Kill { .. }
            | RpcRequest::LogsWait { .. }
            | RpcRequest::LogsTail { .. }
            | RpcRequest::LogsPagination { .. }
    )
}

async fn relay_streaming_rpc(
    config: &AppConfig,
    request: RpcRequest,
    rpc_id: &str,
    frame_tx: &mpsc::Sender<(String, RpcResponse, bool)>,
) -> Result<()> {
    let stream = ipc::connect(config).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    ipc::write_request_to_writer(&mut write_half, request).await?;

    loop {
        let response = ipc::read_response_from_reader(&mut reader).await;
        match response {
            Ok(resp) => {
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
                        RpcResponse::AttachStreamDone { exit_code: None },
                        true,
                    ))
                    .await;
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_supported_proxied_rpc;
    use crate::protocol::{ListQuery, ListSortField, RpcRequest, SortOrder};

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
        }));
    }

    #[test]
    fn nested_node_proxy_is_rejected() {
        assert!(!is_supported_proxied_rpc(&RpcRequest::NodeProxy {
            node: "secondary-a".to_string(),
            inner: Box::new(RpcRequest::List {
                query: ListQuery {
                    search: None,
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

/// Handle a node-proxied streaming attach: open `proxy_rpc_stream()` to the
/// secondary node and relay all streaming frames back to the CLI via IPC.
/// Also reads client messages (input/resize/detach) from the IPC reader and
/// proxies them to the secondary node as one-shot RPCs.
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
                        if ipc::write_response_to_writer(&mut writer, resp).await.is_err() {
                            break;
                        }
                        if is_done {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        let _ = ipc::write_response_to_writer(
                            &mut writer,
                            RpcResponse::Error {
                                message: e.to_string(),
                            },
                        )
                        .await;
                        break;
                    }
                    None => {
                        let _ = ipc::write_response_to_writer(
                            &mut writer,
                            RpcResponse::AttachStreamDone { exit_code: None },
                        )
                        .await;
                        break;
                    }
                }
            }

            client_msg = client_msg_rx.recv() => {
                match client_msg {
                    Some(Ok(req)) => {
                        let is_detach = matches!(req, RpcRequest::AttachDetach { .. });
                        let _ = node_registry.proxy_rpc(&node, &req).await;
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
    // hasn't sent a done frame yet.
    node_registry.remove_pending(&node, &stream_rpc_id).await;
    let _ = node_registry
        .proxy_rpc(&node, &RpcRequest::AttachDetach { id: session_id })
        .await;

    Ok(())
}
