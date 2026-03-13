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
    ipc,
    node::NodeRegistry,
    notification::event::NotificationEvent,
    protocol::{NodeWsMessage, RpcRequest, RpcResponse},
};

pub(super) fn spawn_join_connector(
    join: JoinConfig,
    local_config: Arc<AppConfig>,
    notification_rx: broadcast::Receiver<NotificationEvent>,
) -> (tokio::task::AbortHandle, watch::Sender<bool>) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        run_join_connector(join, local_config, notification_rx, stop_rx).await;
    });
    (task.abort_handle(), stop_tx)
}

async fn run_join_connector(
    join: JoinConfig,
    local_config: Arc<AppConfig>,
    mut notification_rx: broadcast::Receiver<NotificationEvent>,
    mut stop_rx: watch::Receiver<bool>,
) {
    const BACKOFF: &[u64] = &[1, 2, 4, 8, 16, 32, 60];
    let mut attempt = 0usize;

    loop {
        match connect_and_relay(&join, &local_config, &mut notification_rx, &mut stop_rx).await {
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
    notification_rx: &mut broadcast::Receiver<NotificationEvent>,
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
    let text = serde_json::to_string(&handshake)?;
    ws_tx
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(|e| crate::error::AppError::Protocol(e.to_string()))?;

    match ws_rx.next().await {
        Some(Ok(WsMessage::Text(t))) => {
            match serde_json::from_str::<NodeWsMessage>(&t)
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

    let (stream_frame_tx, mut stream_frame_rx) =
        mpsc::unbounded_channel::<(String, RpcResponse, bool)>();
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
                let reply_text = match serde_json::to_string(&reply) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if ws_tx.send(WsMessage::Text(reply_text.into())).await.is_err() {
                    break;
                }
            }
            incoming = ws_rx.next() => {
                let Some(msg_result) = incoming else { break };

                let text = match msg_result {
                    Ok(WsMessage::Text(t)) => t,
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                    _ => continue,
                };

                let node_msg: NodeWsMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                match node_msg {
                    NodeWsMessage::Rpc { id, request } => {
                        let req = match serde_json::from_value::<RpcRequest>(request) {
                            Ok(r) => {
                                match r {
                                    RpcRequest::Health { .. }
                                    | RpcRequest::List { .. }
                                    | RpcRequest::Start { .. }
                                    | RpcRequest::AttachSubscribe { .. }
                                    | RpcRequest::AttachInput { .. }
                                    | RpcRequest::AttachResize { .. }
                                    | RpcRequest::Stop { .. }
                                    | RpcRequest::Kill { .. }
                                    | RpcRequest::LogsPoll { .. }
                                    | RpcRequest::LogsSnapshot { .. }
                                    | RpcRequest::LogsWait { .. } => r,
                                    _ => {
                                        warn!(%id, request_type = r.name(), "unsupported proxied RPC method");
                                        continue;
                                    }
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
                                    let _ = frame_tx.send((rpc_id, resp, true));
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
                        let reply_text = match serde_json::to_string(&reply) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        if ws_tx.send(WsMessage::Text(reply_text.into())).await.is_err() {
                            break;
                        }
                    }
                    NodeWsMessage::Ping => {
                        if let Ok(pong) = serde_json::to_string(&NodeWsMessage::Pong) {
                            let _ = ws_tx.send(WsMessage::Text(pong.into())).await;
                        }
                    }
                    _ => {}
                }
            }
            notif = notification_rx.recv() => {
                match notif {
                    Ok(event) => {
                        let relay = NodeWsMessage::Notification {
                            kind: event.kind.as_str().to_string(),
                            summary: event.summary,
                            body: event.body,
                            session_ids: event.session_ids,
                            trigger_rule: event.trigger_rule.map(|rule| rule.as_str().to_string()),
                            trigger_detail: event.trigger_detail,
                        };
                        let relay_text = match serde_json::to_string(&relay) {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        if ws_tx.send(WsMessage::Text(relay_text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(node = %join.name, skipped, "notification relay lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(false)
}

async fn relay_streaming_rpc(
    config: &AppConfig,
    request: RpcRequest,
    rpc_id: &str,
    frame_tx: &mpsc::UnboundedSender<(String, RpcResponse, bool)>,
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
                if frame_tx.send((rpc_id.to_string(), resp, is_done)).is_err() {
                    break;
                }
                if is_done {
                    break;
                }
            }
            Err(_) => {
                let _ = frame_tx.send((
                    rpc_id.to_string(),
                    RpcResponse::AttachStreamDone { exit_code: None },
                    true,
                ));
                break;
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
    let mut stream_rx = match node_registry.proxy_rpc_stream(&node, &inner).await {
        Ok(rx) => rx,
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

    let (client_msg_tx, mut client_msg_rx) = mpsc::unbounded_channel::<Result<RpcRequest>>();
    let client_reader_task = tokio::spawn(async move {
        let mut reader = reader;
        loop {
            match ipc::read_request_from_reader(&mut reader).await {
                Ok(req) => {
                    if client_msg_tx.send(Ok(req)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = client_msg_tx.send(Err(e));
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
    let _ = node_registry
        .proxy_rpc(&node, &RpcRequest::AttachDetach { id: session_id })
        .await;

    Ok(())
}
