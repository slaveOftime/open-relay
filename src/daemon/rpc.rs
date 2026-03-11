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
    session::{StartSpec, pty::EscapeFilter},
};

use super::{JoinHandles, NotificationTx, SessionStoreHandle};

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client(
    stream: Stream,
    config: &AppConfig,
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
        config,
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
    config: &AppConfig,
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
        RpcRequest::DaemonStop => handle_daemon_stop(config, session_store, shutdown_tx).await,
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
    config: &AppConfig,
    session_store: &SessionStoreHandle,
    shutdown_tx: &mpsc::UnboundedSender<()>,
) -> RpcResponse {
    let mut store = session_store.lock().await;
    let stopped = store.stop_all_sessions(config.stop_grace_seconds).await;
    let _ = shutdown_tx.send(());
    RpcResponse::DaemonStop { stopped }
}

async fn handle_list(
    query: ListQuery,
    session_store: &SessionStoreHandle,
    db: &Arc<Database>,
) -> Result<RpcResponse> {
    let total = db.count_summaries(&query).await?;
    let mut store = session_store.lock().await;
    let sessions = store.list_summaries(&query).await?;
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
    let mut store = session_store.lock().await;
    match store
        .start_session(
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

async fn handle_attach_subscribe(
    id: String,
    from_byte_offset: Option<u64>,
    mut reader: BufReader<tokio::io::ReadHalf<Stream>>,
    mut writer: tokio::io::WriteHalf<Stream>,
    session_store: &SessionStoreHandle,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;

    // Acquire snapshot + subscribe to broadcast.
    let (replay_chunks, end_offset, mut broadcast_rx, bracketed_paste_mode, app_cursor_keys) = {
        let mut store = session_store.lock().await;
        match store.attach_subscribe_init(&id, from_byte_offset).await {
            Ok(t) => t,
            Err(err) => {
                let resp = RpcResponse::Error {
                    message: err.message(&id),
                };
                return ipc::write_response_to_writer(&mut writer, resp).await;
            }
        }
    };

    // Filter CPR/DSR responses from the replay bytes (same filter as the
    // live stream) and send the raw filtered bytes directly to the client.
    let mut init_filter = EscapeFilter::new();
    let data: Vec<u8> = replay_chunks
        .iter()
        .flat_map(|(_, b)| init_filter.filter(b))
        .collect();

    // Send init frame.
    let running = {
        let store = session_store.lock().await;
        store.is_running(&id)
    };
    ipc::write_response_to_writer(
        &mut writer,
        RpcResponse::AttachStreamInit {
            data,
            end_offset,
            running,
            bracketed_paste_mode,
            app_cursor_keys,
        },
    )
    .await?;

    // Mark attach presence.
    {
        let mut store = session_store.lock().await;
        let _ = store.mark_attach_presence(&id).await;
    }

    // `read_request_from_reader` uses `read_line`, which is not safe to keep
    // cancelling inside `tokio::select!`. Read client messages in a dedicated
    // task so each frame is consumed to completion.
    let (client_msg_tx, mut client_msg_rx) = mpsc::unbounded_channel();
    let client_reader_task = tokio::spawn(async move {
        loop {
            let msg = ipc::read_request_from_reader(&mut reader).await;
            let done = msg.is_err();
            if client_msg_tx.send(msg).is_err() {
                break;
            }
            if done {
                break;
            }
        }
    });

    let mut chunk_filter = EscapeFilter::new();
    let mut current_offset = end_offset;
    let mut completion_check = tokio::time::interval(std::time::Duration::from_millis(100));
    completion_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Track mode state for change detection.
    let mut last_modes = crate::session::mode_tracker::ModeSnapshot {
        app_cursor_keys,
        bracketed_paste_mode,
    };

    loop {
        tokio::select! {
            biased;

            _ = completion_check.tick() => {
                let (running, _output_closed, exit_code) = {
                    let mut store = session_store.lock().await;
                    match store.attach_stream_status(&id).await {
                        Ok(state) => state,
                        Err(_) => break,
                    }
                };

                if !running {
                    let chunks = {
                        let mut store = session_store.lock().await;
                        match store.attach_subscribe_init(&id, Some(current_offset)).await {
                            Ok((chunks, _end, _rx, _bpm, _ack)) => chunks,
                            Err(_) => Vec::new(),
                        }
                    };

                    if !chunks.is_empty() {
                        let mut resync_filter = EscapeFilter::new();
                        let raw: Vec<u8> = chunks
                            .iter()
                            .flat_map(|(_, b)| resync_filter.filter(b))
                            .collect();
                        if !raw.is_empty() {
                            ipc::write_response_to_writer(
                                &mut writer,
                                RpcResponse::AttachStreamChunk {
                                    offset: current_offset,
                                    data: raw,
                                },
                            )
                            .await?;
                        }
                    }

                    let _ = ipc::write_response_to_writer(
                        &mut writer,
                        RpcResponse::AttachStreamDone { exit_code },
                    )
                    .await;
                    break;
                }
            }

            // Incoming client message (input / resize / detach).
            client_msg = client_msg_rx.recv() => {
                match client_msg {
                    None => break,
                    Some(Err(_)) => break, // client disconnected
                    Some(Ok(RpcRequest::AttachInput { id: req_id, data })) if req_id == id => {
                        let mut store = session_store.lock().await;
                        if store.attach_input(&req_id, &data).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(RpcRequest::AttachResize { id: req_id, rows, cols })) if req_id == id => {
                        let mut store = session_store.lock().await;
                        let _ = store.attach_resize(&req_id, rows, cols).await;
                    }
                    Some(Ok(RpcRequest::AttachDetach { id: req_id })) if req_id == id => {
                        break;
                    }
                    Some(Ok(_)) => {} // ignore unrecognised messages
                }
            }

            // Outgoing chunk from the PTY broadcast.
            chunk = broadcast_rx.recv() => {
                match chunk {
                    Ok(raw_arc) => {
                        let filtered = chunk_filter.filter(&raw_arc);
                        if !filtered.is_empty() {
                            ipc::write_response_to_writer(
                                &mut writer,
                                RpcResponse::AttachStreamChunk {
                                    offset: current_offset,
                                    data: filtered,
                                },
                            )
                            .await?;
                            current_offset += raw_arc.len() as u64;
                        }

                        // Check for terminal mode changes after each chunk.
                        let current_modes = {
                            let store = session_store.lock().await;
                            store.get_mode_snapshot(&id)
                        };
                        if let Some(modes) = current_modes {
                            if modes != last_modes {
                                ipc::write_response_to_writer(
                                    &mut writer,
                                    RpcResponse::AttachModeChanged {
                                        app_cursor_keys: modes.app_cursor_keys,
                                        bracketed_paste_mode: modes.bracketed_paste_mode,
                                    },
                                )
                                .await?;
                                last_modes = modes;
                            }
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        // Re-sync from ring from current_offset.
                        let (chunks, new_end) = {
                            let mut store = session_store.lock().await;
                            match store.attach_subscribe_init(&id, Some(current_offset)).await {
                                Ok((c, e, rx, _bpm, _ack)) => {
                                    broadcast_rx = rx;
                                    (c, e)
                                }
                                Err(_) => break,
                            }
                        };
                        let mut resync_filter = EscapeFilter::new();
                        let raw: Vec<u8> = chunks
                            .iter()
                            .flat_map(|(_, b)| resync_filter.filter(b))
                            .collect();
                        if !raw.is_empty() {
                            ipc::write_response_to_writer(
                                &mut writer,
                                RpcResponse::AttachStreamChunk {
                                    offset: current_offset,
                                    data: raw,
                                },
                            )
                            .await?;
                        }
                        current_offset = new_end;
                    }
                    Err(RecvError::Closed) => {
                        // Session ended.
                        let exit_code = {
                            let store = session_store.lock().await;
                            store.get_exit_code(&id)
                        };
                        let _ = ipc::write_response_to_writer(
                            &mut writer,
                            RpcResponse::AttachStreamDone { exit_code },
                        )
                        .await;
                        break;
                    }
                }
            }
        }
    }

    // Clean up attach presence on exit.
    client_reader_task.abort();
    let mut store = session_store.lock().await;
    let _ = store.attach_detach(&id).await;
    Ok(())
}

async fn handle_attach_input(
    id: String,
    data: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    let mut store = session_store.lock().await;
    match store.attach_input(&id, &data).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

async fn handle_attach_resize(
    id: String,
    rows: u16,
    cols: u16,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    let mut store = session_store.lock().await;
    match store.attach_resize(&id, rows, cols).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

async fn handle_attach_detach(id: String, session_store: &SessionStoreHandle) -> RpcResponse {
    let mut store = session_store.lock().await;
    match store.attach_detach(&id).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

async fn handle_stop(
    id: String,
    grace_seconds: u64,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    let mut store = session_store.lock().await;
    if store.stop_session(&id, grace_seconds).await {
        info!(session_id = id, "session stopped");
        RpcResponse::Stop { stopped: true }
    } else {
        RpcResponse::Error {
            message: format!("session not found or failed to stop: {id}"),
        }
    }
}

async fn handle_logs_snapshot(
    id: String,
    tail: usize,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    let mut store = session_store.lock().await;
    match store.logs_snapshot(&id, tail).await {
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
    let mut store = session_store.lock().await;
    match store.logs_poll(&id, cursor).await {
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
    let initial = {
        let mut store = session_store.lock().await;
        store.logs_snapshot(&id, tail).await
    };

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
                        let updated = {
                            let mut store = session_store.lock().await;
                            store.logs_snapshot(&id, tail).await
                        };
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

            if let Some((l, c, r)) = {
                let mut store = session_store.lock().await;
                store.logs_snapshot(&id, tail).await
            } {
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

async fn handle_node_proxy(
    node: String,
    inner: crate::protocol::RpcRequest,
    node_registry: &Arc<NodeRegistry>,
) -> RpcResponse {
    match node_registry.proxy_rpc(&node, &inner).await {
        Ok(r) => r,
        Err(e) => RpcResponse::Error {
            message: e.to_string(),
        },
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
    config: &AppConfig,
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
        client::spawn_join_connector(join, config.clone(), notification_tx.subscribe());
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

async fn handle_node_list(node_registry: &Arc<NodeRegistry>) -> RpcResponse {
    let nodes = node_registry.connected_names().await;
    RpcResponse::NodeList { nodes }
}

/// Handle a node-proxied streaming attach: open `proxy_rpc_stream()` to the
/// secondary node and relay all streaming frames back to the CLI via IPC.
/// Also reads client messages (input/resize/detach) from the IPC reader and
/// proxies them to the secondary node as one-shot RPCs.
async fn handle_node_proxy_streaming(
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

    // Spawn a task to read client messages (input/resize/detach) from IPC.
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

    // Extract session ID from the inner request for proxying client messages.
    let session_id = match &inner {
        RpcRequest::AttachSubscribe { id, .. } => id.clone(),
        _ => String::new(),
    };

    loop {
        tokio::select! {
            biased;

            // Streaming frames from the secondary node.
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
                        // Stream ended without a done frame.
                        let _ = ipc::write_response_to_writer(
                            &mut writer,
                            RpcResponse::AttachStreamDone { exit_code: None },
                        )
                        .await;
                        break;
                    }
                }
            }

            // Client messages (input/resize/detach) from CLI via IPC.
            client_msg = client_msg_rx.recv() => {
                match client_msg {
                    Some(Ok(req)) => {
                        let is_detach = matches!(req, RpcRequest::AttachDetach { .. });
                        // Proxy to secondary node as one-shot RPC.
                        let _ = node_registry.proxy_rpc(&node, &req).await;
                        if is_detach {
                            break;
                        }
                    }
                    _ => break, // client disconnected
                }
            }
        }
    }

    client_reader_task.abort();
    // Send detach to clean up attach presence on the secondary node.
    let _ = node_registry
        .proxy_rpc(&node, &RpcRequest::AttachDetach { id: session_id })
        .await;

    Ok(())
}
