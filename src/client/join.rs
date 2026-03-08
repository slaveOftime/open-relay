use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{info, warn};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    notification::event::NotificationEvent,
    protocol::{JoinSummary, NodeWsMessage, RpcResponse},
};

// ---------------------------------------------------------------------------
// Persisted join configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinConfig {
    pub name: String,
    pub primary_url: String,
    /// Plaintext API key — stored with user-private permissions on the secondary.
    pub api_key: String,
}

fn joins_path(config: &AppConfig) -> std::path::PathBuf {
    config.state_dir.join("joins.json")
}

pub fn load_join_configs(config: &AppConfig) -> Vec<JoinConfig> {
    let path = joins_path(config);
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_join_config(config: &AppConfig, join: &JoinConfig) -> Result<()> {
    let path = joins_path(config);
    let mut joins = load_join_configs(config);
    joins.retain(|j| j.name != join.name);
    joins.push(join.clone());
    let data = serde_json::to_string_pretty(&joins)?;
    std::fs::write(&path, data)?;
    // Set file permissions to user-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn remove_join_config(config: &AppConfig, name: &str) -> bool {
    let path = joins_path(config);
    let mut joins = load_join_configs(config);
    let before = joins.len();
    joins.retain(|j| j.name != name);
    if joins.len() < before {
        let data = serde_json::to_string_pretty(&joins).unwrap_or_default();
        let _ = std::fs::write(&path, data);
        true
    } else {
        false
    }
}

pub fn list_join_summaries(config: &AppConfig) -> Vec<JoinSummary> {
    load_join_configs(config)
        .into_iter()
        .map(|j| JoinSummary {
            name: j.name,
            primary_url: j.primary_url,
            connected: false, // live status is known only inside the daemon
        })
        .collect()
}

// ---------------------------------------------------------------------------
// CLI handlers
// ---------------------------------------------------------------------------

/// `oly join start` — persist config and signal the local daemon to connect.
pub async fn run_join(config: &AppConfig, url: String, name: String, key: String) -> Result<()> {
    let join = JoinConfig {
        name: name.clone(),
        primary_url: url.clone(),
        api_key: key.clone(),
    };
    save_join_config(config, &join)?;

    match ipc::send_request(
        config,
        crate::protocol::RpcRequest::JoinStart {
            url,
            name: name.clone(),
            key,
        },
    )
    .await
    {
        Ok(RpcResponse::Ack) => {
            println!(
                "Joining primary as \"{name}\". Use `oly join stop --name {name}` to disconnect."
            );
            Ok(())
        }
        Ok(RpcResponse::Error { message }) => Err(AppError::DaemonUnavailable(message)),
        Err(AppError::DaemonUnavailable(_)) => {
            // Daemon not running — config is saved; will connect on next daemon start.
            println!(
                "Saved join config for \"{name}\". \
                 Start the daemon with `oly daemon start` to connect automatically."
            );
            Ok(())
        }
        _ => Err(AppError::Protocol("unexpected response".into())),
    }
}

/// `oly join stop` — remove persisted config and signal the local daemon to disconnect.
pub async fn run_join_stop(config: &AppConfig, name: String) -> Result<()> {
    let removed = remove_join_config(config, &name);
    if !removed {
        eprintln!("warning: no saved join config found for \"{name}\"");
    }

    match ipc::send_request(
        config,
        crate::protocol::RpcRequest::JoinStop { name: name.clone() },
    )
    .await
    {
        Ok(RpcResponse::Ack) | Ok(RpcResponse::Error { .. }) => {}
        Err(_) => {} // Daemon not running is fine — config already removed.
        _ => {}
    }

    println!("Stopped join for \"{name}\".");
    Ok(())
}

// ---------------------------------------------------------------------------
// Background connector (runs inside the secondary daemon)
// ---------------------------------------------------------------------------

/// Spawn a persistent outbound connector to the primary for `join`.
/// Returns an abort handle and a watch sender.  Send `true` on the sender
/// to request a clean stop; the connector will exit without retrying.
pub fn spawn_join_connector(
    join: JoinConfig,
    local_config: AppConfig,
    notification_rx: broadcast::Receiver<NotificationEvent>,
) -> (tokio::task::AbortHandle, watch::Sender<bool>) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        run_join_connector(join, local_config, notification_rx, stop_rx).await;
    });
    (task.abort_handle(), stop_tx)
}

/// Persistent outbound connector loop for a secondary node.
async fn run_join_connector(
    join: JoinConfig,
    local_config: AppConfig,
    mut notification_rx: broadcast::Receiver<NotificationEvent>,
    mut stop_rx: watch::Receiver<bool>,
) {
    const BACKOFF: &[u64] = &[1, 2, 4, 8, 16, 32, 60];
    let mut attempt = 0usize;

    loop {
        match connect_and_relay(&join, &local_config, &mut notification_rx, &mut stop_rx).await {
            Ok(true) => {
                // Stop was explicitly requested — do not retry.
                info!(node = %join.name, "join connector stopped");
                return;
            }
            Ok(false) => {
                // Server closed / network drop — retry below.
                warn!(node = %join.name, "join connector disconnected");
            }
            Err(err) => {
                warn!(
                    node = %join.name,
                    %err,
                    "join connector disconnected"
                );
            }
        }

        // Check stop signal before entering backoff sleep.
        if *stop_rx.borrow() {
            info!(node = %join.name, "join connector stopped");
            return;
        }

        let wait = BACKOFF[attempt.min(BACKOFF.len() - 1)];
        warn!(
            node = %join.name,
            wait_secs = wait,
            "join connector retrying"
        );
        // Interrupt backoff sleep early if stop is requested.
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

/// Returns `Ok(true)` when a stop was requested, `Ok(false)` on a clean
/// server-side disconnect, and `Err(_)` on handshake / network failures.
async fn connect_and_relay(
    join: &JoinConfig,
    local_config: &AppConfig,
    notification_rx: &mut broadcast::Receiver<NotificationEvent>,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<bool> {
    // Build the WebSocket URL (http → ws, https → wss).
    let base = join.primary_url.trim_end_matches('/');
    let ws_url = if base.starts_with("https://") {
        format!("{}/api/nodes/join", base.replacen("https://", "wss://", 1))
    } else {
        format!("{}/api/nodes/join", base.replacen("http://", "ws://", 1))
    };

    let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| AppError::Protocol(format!("WebSocket connect failed: {e}")))?;

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // ── Handshake ────────────────────────────────────────────────────────────
    let handshake = NodeWsMessage::Join {
        name: join.name.clone(),
        key: join.api_key.clone(),
    };
    let text = serde_json::to_string(&handshake)?;
    ws_tx
        .send(WsMessage::Text(text.into()))
        .await
        .map_err(|e| AppError::Protocol(e.to_string()))?;

    // ── Wait for Joined ──────────────────────────────────────────────────────
    match ws_rx.next().await {
        Some(Ok(WsMessage::Text(t))) => {
            match serde_json::from_str::<NodeWsMessage>(&t)
                .map_err(|e| AppError::Protocol(e.to_string()))?
            {
                NodeWsMessage::Joined => {
                    info!(node = %join.name, primary = %join.primary_url, "joined primary");
                }
                NodeWsMessage::Error { message } => {
                    return Err(AppError::Protocol(format!("join rejected: {message}")));
                }
                _ => {
                    return Err(AppError::Protocol("unexpected response to join".into()));
                }
            }
        }
        _ => return Err(AppError::Protocol("no response to join handshake".into())),
    }

    // ── Relay loop ───────────────────────────────────────────────────────────
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    let _ = ws_tx.send(WsMessage::Close(None)).await;
                    return Ok(true);
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
                        let req = match serde_json::from_value::<crate::protocol::RpcRequest>(request) {
                            Ok(r) => r,
                            Err(err) => {
                                warn!(%err, rpc_id = %id, "failed to deserialise proxied RPC");
                                continue;
                            }
                        };

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
                        if ws_tx
                            .send(WsMessage::Text(reply_text.into()))
                            .await
                            .is_err()
                        {
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
