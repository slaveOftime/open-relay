use interprocess::local_socket::tokio::Stream;
use std::sync::Arc;
use tokio::{io::BufReader, sync::mpsc};
use tracing::{info, warn};

use crate::{
    client,
    config::AppConfig,
    db::Database,
    error::Result,
    http::auth,
    ipc,
    node::NodeRegistry,
    protocol::{ApiKeySummary, JoinSummary, ListQuery, RpcRequest, RpcResponse},
    session::{
        SessionStore, StartSpec,
        logs::{read_persisted_log_page, read_resize_events, render_log_file},
    },
};

use super::{JoinHandles, NotificationTx, NotifierHandle, SessionEventTx, SessionStoreHandle};
use super::{
    rpc_attach::{
        handle_attach_busy, handle_attach_detach, handle_attach_input, handle_attach_resize,
        handle_attach_subscribe,
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
    session_event_tx: SessionEventTx,
    notification_tx: NotificationTx,
    notifier: NotifierHandle,
) -> Result<()> {
    // Peek at the request without consuming the stream so we can decide whether
    // it needs the bidirectional streaming path or the simple req/resp path.
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let request = ipc::read_request_from_reader(&mut reader).await?;

    if let RpcRequest::AttachSubscribe {
        id,
        from_byte_offset,
        rows,
        cols,
    } = request
    {
        return handle_attach_subscribe(
            id,
            from_byte_offset,
            rows,
            cols,
            reader,
            write_half,
            &session_store,
        )
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
        &session_event_tx,
        &notification_tx,
        &notifier,
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
    session_event_tx: &SessionEventTx,
    notification_tx: &NotificationTx,
    notifier: &NotifierHandle,
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
            tags,
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
                tags,
                cmd,
                args,
                cwd,
                rows,
                cols,
                disable_notifications,
            )
            .await
        }
        RpcRequest::NotifySet { id, enabled } => {
            handle_notify_set(id, enabled, session_store).await
        }
        RpcRequest::NotifySend {
            source,
            title,
            description,
            body,
            url,
        } => {
            handle_notify_send(
                source,
                title,
                description,
                body,
                url,
                notifier,
                notification_tx,
                session_event_tx,
                session_store,
            )
            .await
        }
        RpcRequest::AttachSubscribe { .. } => {
            // Handled before dispatch in handle_client; should not reach here.
            RpcResponse::Error {
                message: "AttachSubscribe must be handled on the streaming path".into(),
            }
        }
        RpcRequest::AttachInput {
            id,
            data,
            wait_for_change,
        } => handle_attach_input(id, data, session_store, wait_for_change).await,
        RpcRequest::AttachBusy { id } => handle_attach_busy(id, session_store).await,
        RpcRequest::UploadFile {
            id,
            path,
            bytes,
            dedupe,
        } => handle_upload_file(config, id, path, bytes, dedupe).await,
        RpcRequest::AttachResize { id, rows, cols } => {
            handle_attach_resize(id, rows, cols, session_store).await
        }
        RpcRequest::AttachDetach { id } => handle_attach_detach(id, session_store).await,
        RpcRequest::Stop { id, grace_seconds } => {
            handle_stop(id, grace_seconds, session_store).await
        }
        RpcRequest::Kill { id } => handle_kill(id, session_store).await,
        RpcRequest::LogsTail {
            id,
            tail,
            term_cols,
            keep_color,
        } => handle_logs_tail(id, tail, term_cols, keep_color, db).await,
        RpcRequest::LogsPagination { id, offset, limit } => {
            handle_logs_pagination(id, offset, limit, db).await
        }
        RpcRequest::LogsWait { id, timeout_ms } => {
            handle_logs_wait(id, timeout_ms, session_store, notification_tx, db).await
        }
        RpcRequest::NodeProxy { node, inner } => {
            handle_node_proxy(node, *inner, node_registry).await
        }
        RpcRequest::ApiKeyAdd { name } => handle_api_key_add(name, db).await,
        RpcRequest::ApiKeyList => handle_api_key_list(db).await,
        RpcRequest::ApiKeyRemove { name } => handle_api_key_remove(name, db).await,
        RpcRequest::JoinStart { url, name, key } => {
            handle_join_start(config, join_handles, session_event_tx, url, name, key).await?
        }
        RpcRequest::JoinStop { name } => handle_join_stop(config, join_handles, name).await,
        RpcRequest::JoinList { primary } => handle_join_list(config, node_registry, primary).await,
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
    tags: Vec<String>,
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
            tags,
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

async fn handle_upload_file(
    config: &AppConfig,
    id: String,
    path: String,
    bytes: Vec<u8>,
    dedupe: bool,
) -> RpcResponse {
    match crate::session::file::write_session_upload(config, &id, &path, &bytes, dedupe) {
        Ok(saved_path) => RpcResponse::UploadFile {
            path: saved_path.to_string_lossy().to_string(),
            bytes: bytes.len(),
        },
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

async fn handle_notify_set(
    id: String,
    enabled: bool,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    match session_store.set_notifications_enabled(&id, enabled).await {
        Ok(()) => {
            info!(
                session_id = id,
                notifications_enabled = enabled,
                "session notification setting updated"
            );
            RpcResponse::Ack
        }
        Err(_) => RpcResponse::Error {
            message: format!("session not found or not running: {id}"),
        },
    }
}

async fn handle_notify_send(
    source: Option<String>,
    title: String,
    description: Option<String>,
    body: Option<String>,
    url: Option<String>,
    notifier: &NotifierHandle,
    notification_tx: &NotificationTx,
    session_event_tx: &SessionEventTx,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    if title.trim().is_empty() {
        return RpcResponse::Error {
            message: "notification title cannot be empty".to_string(),
        };
    }

    if let Some(source_id) = source.as_deref()
        && !session_store.is_running(source_id)
    {
        return RpcResponse::Error {
            message: format!("Cannot use non-running session as source: {source_id}"),
        };
    }

    let event = crate::notification::event::NotificationEvent::manual(
        source,
        title,
        description,
        body,
        url,
    );
    let outcome = notifier.dispatch(&event).await;

    if !outcome.any_delivered() {
        warn!(
            attempted = outcome.attempted,
            failed_channels = ?outcome.failed_channels,
            "manual notification delivery failed on all channels"
        );
    }

    let _ = notification_tx.send(event.clone());
    let _ = session_event_tx.send(event.into_session_event(0));
    RpcResponse::Ack
}

async fn handle_logs_tail(
    id: String,
    tail: usize,
    term_cols: u16,
    keep_color: bool,
    db: &Arc<Database>,
) -> RpcResponse {
    let session_dir = match db.get_session_dir(&id).await {
        Ok(Some(dir)) => dir,
        Ok(None) => {
            return RpcResponse::Error {
                message: format!("session not found: {id}"),
            };
        }
        Err(err) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
    };

    let log_path = session_dir.join("output.log");
    let lines = match render_log_file(&log_path, tail, keep_color, term_cols, None) {
        Ok(output) => output,
        Err(err) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
    };

    let resizes = match read_resize_events(&session_dir) {
        Ok(resizes) => resizes,
        Err(err) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
    };

    RpcResponse::LogsTail {
        output: lines,
        resizes,
    }
}

async fn handle_logs_pagination(
    id: String,
    offset: usize,
    limit: usize,
    db: &Arc<Database>,
) -> RpcResponse {
    let session_dir = match db.get_session_dir(&id).await {
        Ok(Some(dir)) => dir,
        Ok(None) => {
            return RpcResponse::Error {
                message: format!("session not found: {id}"),
            };
        }
        Err(err) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
    };

    match read_persisted_log_page(&session_dir, offset, limit) {
        Some((lines, total)) => {
            let resizes = read_resize_events(&session_dir).unwrap_or_default();
            RpcResponse::LogsPagination {
                lines,
                total,
                resizes,
            }
        }
        None => RpcResponse::Error {
            message: format!("session not found: {id}"),
        },
    }
}

async fn handle_logs_wait(
    id: String,
    timeout_ms: u64,
    session_store: &SessionStoreHandle,
    notification_tx: &NotificationTx,
    db: &Arc<Database>,
) -> RpcResponse {
    if let Err(err) = db.get_session_dir(&id).await {
        return RpcResponse::Error {
            message: err.to_string(),
        };
    }

    if timeout_ms == 0
        || !session_store.is_running(&id)
        || session_store.is_input_needed(&id)
        || session_store.is_silent_for(&id, std::time::Duration::from_secs(10))
    {
        return RpcResponse::Empty;
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut notify_rx = notification_tx.subscribe();
    let mut state_poll = tokio::time::interval(std::time::Duration::from_millis(100));
    let deadline_sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(deadline_sleep);

    'wait: loop {
        tokio::select! {
            biased;
            _ = &mut deadline_sleep => break 'wait,
            _ = state_poll.tick() => {
                if !session_store.is_running(&id) {
                    break 'wait;
                }
            }
            notif = notify_rx.recv() => {
                match notif {
                    Ok(event) => {
                        if matches!(event.kind, crate::notification::event::NotificationKind::InputNeeded)
                            && event.session_ids.iter().any(|s| s == &id)
                        {
                            break 'wait;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break 'wait,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break 'wait,
                }
            }
        }
    }

    RpcResponse::Empty
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
    session_event_tx: &SessionEventTx,
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
        spawn_join_connector(join, Arc::clone(config), session_event_tx.subscribe());
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

async fn handle_join_list(
    config: &AppConfig,
    node_registry: &NodeRegistry,
    primary: bool,
) -> RpcResponse {
    let joins = if primary {
        node_registry
            .connected_names()
            .await
            .iter()
            .map(|n| JoinSummary {
                name: n.clone(),
                primary_url: "".into(),
                connected: true,
            })
            .collect()
    } else {
        client::join::list_join_summaries(config)
    };

    RpcResponse::JoinList { joins }
}
