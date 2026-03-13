use interprocess::local_socket::tokio::Stream;
use std::sync::Arc;
use tokio::{io::BufReader, sync::mpsc};
use tracing::info;

use crate::{
    client,
    config::AppConfig,
    db::Database,
    error::Result,
    http::auth,
    ipc,
    node::NodeRegistry,
    protocol::{ApiKeySummary, ListQuery, RpcRequest, RpcResponse},
    session::{SessionStore, StartSpec},
};

use super::{JoinHandles, NotificationTx, SessionStoreHandle};
use super::{
    rpc_attach::{
        handle_attach_detach, handle_attach_input, handle_attach_resize, handle_attach_subscribe,
    },
    rpc_nodes::{
        handle_node_list, handle_node_proxy, handle_node_proxy_streaming, spawn_join_connector,
    },
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client(
    stream: Stream,
    config: Arc<AppConfig>,
    session_store: SessionStoreHandle,
    shutdown_tx: mpsc::UnboundedSender<()>,
    node_registry: Arc<NodeRegistry>,
    db: Arc<Database>,
    join_handles: JoinHandles,
    notification_tx: NotificationTx,
) -> Result<()> {
    // Peek at the request without consuming the stream so we can decide whether
    // it needs the bidirectional streaming path or the simple req/resp path.
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let request = ipc::read_request_from_reader(&mut reader).await?;

    if let RpcRequest::AttachSubscribe {
        id,
        from_byte_offset,
    } = request
    {
        return handle_attach_subscribe(id, from_byte_offset, reader, write_half, &session_store)
            .await;
    }

    // Node-proxied streaming attach: unwrap the proxy envelope and relay
    // streaming frames from the secondary node back to the CLI.
    if matches!(
        &request,
        RpcRequest::NodeProxy { inner, .. }
            if matches!(inner.as_ref(), RpcRequest::AttachSubscribe { .. })
    ) {
        if let RpcRequest::NodeProxy { node, inner } = request {
            return handle_node_proxy_streaming(node, *inner, reader, write_half, &node_registry)
                .await;
        }
    }

    // Non-streaming path: dispatch and write single response.
    let response = dispatch_request(
        request,
        &config,
        &session_store,
        &shutdown_tx,
        &node_registry,
        &db,
        &join_handles,
        &notification_tx,
    )
    .await?;
    ipc::write_response_to_writer(&mut write_half, response).await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_request(
    request: RpcRequest,
    config: &Arc<AppConfig>,
    session_store: &SessionStoreHandle,
    shutdown_tx: &mpsc::UnboundedSender<()>,
    node_registry: &Arc<NodeRegistry>,
    db: &Arc<Database>,
    join_handles: &JoinHandles,
    notification_tx: &NotificationTx,
) -> Result<RpcResponse> {
    let response = match request {
        RpcRequest::Health => RpcResponse::Health {
            daemon_pid: std::process::id(),
        },
        RpcRequest::DaemonStop { grace_seconds } => {
            handle_daemon_stop(grace_seconds, session_store, shutdown_tx).await
        }
        RpcRequest::List { query } => handle_list(query, session_store, db).await?,
        RpcRequest::Start {
            title,
            cmd,
            args,
            cwd,
            rows,
            cols,
            disable_notifications,
        } => {
            handle_start(
                config,
                session_store,
                title,
                cmd,
                args,
                cwd,
                rows,
                cols,
                disable_notifications,
            )
            .await
        }
        RpcRequest::AttachSubscribe { .. } => {
            // Handled before dispatch in handle_client; should not reach here.
            RpcResponse::Error {
                message: "AttachSubscribe must be handled on the streaming path".into(),
            }
        }
        RpcRequest::AttachInput { id, data } => handle_attach_input(id, data, session_store).await,
        RpcRequest::AttachResize { id, rows, cols } => {
            handle_attach_resize(id, rows, cols, session_store).await
        }
        RpcRequest::AttachDetach { id } => handle_attach_detach(id, session_store).await,
        RpcRequest::Stop { id, grace_seconds } => {
            handle_stop(id, grace_seconds, session_store).await
        }
        RpcRequest::Kill { id } => handle_kill(id, session_store).await,
        RpcRequest::LogsSnapshot { id, tail } => {
            handle_logs_snapshot(id, tail, session_store).await
        }
        RpcRequest::LogsPoll { id, cursor } => handle_logs_poll(id, cursor, session_store).await,
        RpcRequest::LogsWait {
            id,
            tail,
            timeout_secs,
        } => handle_logs_wait(id, tail, timeout_secs, session_store, notification_tx).await,
        RpcRequest::NodeProxy { node, inner } => {
            handle_node_proxy(node, *inner, node_registry).await
        }
        RpcRequest::ApiKeyAdd { name } => handle_api_key_add(name, db).await,
        RpcRequest::ApiKeyList => handle_api_key_list(db).await,
        RpcRequest::ApiKeyRemove { name } => handle_api_key_remove(name, db).await,
        RpcRequest::JoinStart { url, name, key } => {
            handle_join_start(config, join_handles, notification_tx, url, name, key).await?
        }
        RpcRequest::JoinStop { name } => handle_join_stop(config, join_handles, name).await,
        RpcRequest::JoinList => handle_join_list(config),
        RpcRequest::NodeList => handle_node_list(node_registry).await,
    };

    Ok(response)
}

async fn handle_daemon_stop(
    grace_seconds: u64,
    session_store: &SessionStoreHandle,
    shutdown_tx: &mpsc::UnboundedSender<()>,
) -> RpcResponse {
    let stopped = session_store.stop_all_sessions(grace_seconds).await;
    let _ = shutdown_tx.send(());
    RpcResponse::DaemonStop { stopped }
}

async fn handle_list(
    query: ListQuery,
    session_store: &SessionStoreHandle,
    db: &Arc<Database>,
) -> Result<RpcResponse> {
    let total = db.count_summaries(&query).await?;
    let sessions = session_store.list_summaries(&query).await?;
    Ok(RpcResponse::List { total, sessions })
}

#[allow(clippy::too_many_arguments)]
async fn handle_start(
    config: &AppConfig,
    session_store: &SessionStoreHandle,
    title: Option<String>,
    cmd: String,
    args: Vec<String>,
    cwd: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    disable_notifications: bool,
) -> RpcResponse {
    match SessionStore::start_session_via_handle(
        session_store,
        config,
        StartSpec {
            title: title.clone(),
            cmd: cmd.clone(),
            args: args.clone(),
            cwd: cwd.clone(),
            rows,
            cols,
            notifications_enabled: !disable_notifications,
        },
    )
    .await
    {
        Ok(session_id) => {
            info!(session_id, cmd, "session started");
            RpcResponse::Start { session_id }
        }
        Err(err) => RpcResponse::Error {
            message: err.to_string(),
        },
    }
}

async fn handle_stop(
    id: String,
    grace_seconds: u64,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    if session_store.stop_session(&id, grace_seconds).await {
        info!(session_id = id, "session stopped");
        RpcResponse::Stop { stopped: true }
    } else {
        RpcResponse::Error {
            message: format!("session not found or failed to stop: {id}"),
        }
    }
}

async fn handle_kill(id: String, session_store: &SessionStoreHandle) -> RpcResponse {
    if session_store.kill_session(&id).await {
        info!(session_id = id, "session killed");
        RpcResponse::Kill { killed: true }
    } else {
        RpcResponse::Error {
            message: format!("session not found or failed to kill: {id}"),
        }
    }
}

async fn handle_logs_snapshot(
    id: String,
    tail: usize,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    match session_store.logs_snapshot(&id, tail).await {
        Some((lines, cursor, running)) => RpcResponse::LogsSnapshot {
            lines,
            cursor,
            running,
        },
        None => RpcResponse::Error {
            message: format!("session not found: {id}"),
        },
    }
}

async fn handle_logs_poll(
    id: String,
    cursor: u64,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    match session_store.logs_poll(&id, cursor).await {
        Some((lines, cursor, running)) => RpcResponse::LogsPoll {
            lines,
            cursor,
            running,
        },
        None => RpcResponse::Error {
            message: format!("session not found: {id}"),
        },
    }
}

async fn handle_logs_wait(
    id: String,
    tail: usize,
    timeout_secs: u64,
    session_store: &SessionStoreHandle,
    notification_tx: &NotificationTx,
) -> RpcResponse {
    use crate::notification::event::NotificationKind;

    let mut notify_rx = notification_tx.subscribe();
    let initial = session_store.logs_snapshot(&id, tail).await;

    match initial {
        None => RpcResponse::Error {
            message: format!("session not found: {id}"),
        },
        Some((mut lines, mut cursor, mut running)) => {
            if !running {
                return RpcResponse::LogsSnapshot {
                    lines,
                    cursor,
                    running,
                };
            }

            let deadline = if timeout_secs == 0 {
                None
            } else {
                Some(tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs))
            };

            'wait: loop {
                if let Some(dl) = deadline {
                    if tokio::time::Instant::now() >= dl {
                        break 'wait;
                    }
                }

                tokio::select! {
                    biased;
                    notif = notify_rx.recv() => {
                        match notif {
                            Ok(event) => {
                                if matches!(event.kind, NotificationKind::InputNeeded)
                                    && event.session_ids.iter().any(|s| s == &id)
                                {
                                    break 'wait;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break 'wait,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break 'wait,
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        let updated = session_store.logs_snapshot(&id, tail).await;
                        if let Some((l, c, r)) = updated {
                            lines = l;
                            cursor = c;
                            running = r;
                        }
                        if !running {
                            break 'wait;
                        }
                        if let Some(dl) = deadline {
                            if tokio::time::Instant::now() >= dl {
                                break 'wait;
                            }
                        }
                    }
                }
            }

            if let Some((l, c, r)) = session_store.logs_snapshot(&id, tail).await {
                lines = l;
                cursor = c;
                running = r;
            }

            RpcResponse::LogsSnapshot {
                lines,
                cursor,
                running,
            }
        }
    }
}

async fn handle_api_key_add(name: String, db: &Arc<Database>) -> RpcResponse {
    use rand::RngCore;
    let mut key_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key_bytes);
    let plaintext: String = key_bytes.iter().map(|b| format!("{b:02x}")).collect();

    match auth::hash_password(&plaintext) {
        Ok(hash) => match db.add_api_key(&name, &hash).await {
            Ok(()) => {
                info!(name, "api key registered");
                RpcResponse::ApiKeyAdd {
                    plaintext_key: plaintext,
                }
            }
            Err(e) => RpcResponse::Error {
                message: e.to_string(),
            },
        },
        Err(e) => RpcResponse::Error {
            message: e.to_string(),
        },
    }
}

async fn handle_api_key_list(db: &Arc<Database>) -> RpcResponse {
    match db.list_api_keys().await {
        Ok(records) => RpcResponse::ApiKeyList {
            keys: records
                .into_iter()
                .map(|r| ApiKeySummary {
                    name: r.name,
                    created_at: r.created_at,
                })
                .collect(),
        },
        Err(e) => RpcResponse::Error {
            message: e.to_string(),
        },
    }
}

async fn handle_api_key_remove(name: String, db: &Arc<Database>) -> RpcResponse {
    match db.delete_api_key(&name).await {
        Ok(removed) => {
            info!(name, removed, "api key removed");
            RpcResponse::ApiKeyRemove { removed }
        }
        Err(e) => RpcResponse::Error {
            message: e.to_string(),
        },
    }
}

async fn handle_join_start(
    config: &Arc<AppConfig>,
    join_handles: &JoinHandles,
    notification_tx: &NotificationTx,
    url: String,
    name: String,
    key: String,
) -> Result<RpcResponse> {
    let join = client::join::JoinConfig {
        name: name.clone(),
        primary_url: url,
        api_key: key,
    };
    client::join::save_join_config(config, &join)?;
    let (abort, stop_tx) =
        spawn_join_connector(join, Arc::clone(config), notification_tx.subscribe());
    join_handles.lock().await.insert(name, (abort, stop_tx));
    Ok(RpcResponse::Ack)
}

async fn handle_join_stop(
    config: &AppConfig,
    join_handles: &JoinHandles,
    name: String,
) -> RpcResponse {
    client::join::remove_join_config(config, &name);
    if let Some((abort, stop_tx)) = join_handles.lock().await.remove(&name) {
        let _ = stop_tx.send(true);
        drop(abort);
    }
    RpcResponse::Ack
}

fn handle_join_list(config: &AppConfig) -> RpcResponse {
    let joins = client::join::list_join_summaries(config);
    RpcResponse::JoinList { joins }
}
