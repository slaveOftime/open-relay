use interprocess::local_socket::tokio::Stream;
use tokio::{io::BufReader, sync::mpsc};
use tracing::{debug, info, trace, warn};

use crate::{
    error::Result,
    ipc,
    protocol::{RpcRequest, RpcResponse},
    session::pty::EscapeFilter,
};

use super::SessionStoreHandle;

fn collect_filtered_chunks(chunks: &[(u64, bytes::Bytes)], filter: &mut EscapeFilter) -> Vec<u8> {
    let mut filtered = Vec::with_capacity(chunks.iter().map(|(_, chunk)| chunk.len()).sum());
    for (_, chunk) in chunks {
        filtered.extend(filter.filter(chunk));
    }
    filtered
}

pub(super) async fn handle_attach_subscribe(
    id: String,
    from_byte_offset: Option<u64>,
    mut reader: BufReader<tokio::io::ReadHalf<Stream>>,
    mut writer: tokio::io::WriteHalf<Stream>,
    session_store: &SessionStoreHandle,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;

    debug!(
        session_id = %id,
        from_byte_offset,
        "starting IPC streaming session relay"
    );

    let (replay_chunks, end_offset, mut broadcast_rx, bracketed_paste_mode, app_cursor_keys) = {
        match session_store
            .attach_subscribe_init(&id, from_byte_offset)
            .await
        {
            Ok(t) => t,
            Err(err) => {
                debug!(session_id = %id, error = err.message(&id), "IPC stream init failed");
                let resp = RpcResponse::Error {
                    message: err.message(&id),
                };
                return ipc::write_response_to_writer(&mut writer, resp).await;
            }
        }
    };

    let mut init_filter = EscapeFilter::new();
    let data = collect_filtered_chunks(&replay_chunks, &mut init_filter);

    let running = session_store.is_running(&id);
    debug!(
        session_id = %id,
        replay_chunks = replay_chunks.len(),
        replay_bytes = data.len(),
        end_offset,
        running,
        app_cursor_keys,
        bracketed_paste_mode,
        "IPC stream init prepared"
    );
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

    session_store.register_attach_client(&id).await;
    debug!(session_id = %id, "IPC stream client registered");

    let (client_msg_tx, mut client_msg_rx) = mpsc::channel(64);
    let client_reader_task = tokio::spawn(async move {
        loop {
            let msg = ipc::read_request_from_reader(&mut reader).await;
            let done = msg.is_err();
            if client_msg_tx.send(msg).await.is_err() {
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

    let mut last_modes = crate::session::mode_tracker::ModeSnapshot {
        app_cursor_keys,
        bracketed_paste_mode,
    };

    let result = async {
        loop {
            tokio::select! {
                biased;

                _ = completion_check.tick() => {
                    let (running, _output_closed, exit_code) = {
                        match session_store.attach_stream_status(&id).await {
                            Ok(state) => state,
                            Err(err) => {
                                warn!(session_id = %id, error = err.message(&id), "IPC stream status lookup failed");
                                break;
                            }
                        }
                    };

                    if !running {
                        let (chunks, new_end) = {
                            match session_store.attach_subscribe_init(&id, Some(current_offset)).await {
                                Ok((chunks, end, _rx, _bpm, _ack)) => (chunks, end),
                                Err(_) => (Vec::new(), current_offset),
                            }
                        };

                        if !chunks.is_empty() {
                            let mut resync_filter = EscapeFilter::new();
                            let raw = collect_filtered_chunks(&chunks, &mut resync_filter);
                            if !raw.is_empty() {
                                debug!(
                                    session_id = %id,
                                    resync_chunks = chunks.len(),
                                    resync_bytes = raw.len(),
                                    from_offset = current_offset,
                                    to_offset = new_end,
                                    "IPC stream flushing final buffered output before completion"
                                );
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
                        current_offset = new_end;

                        info!(session_id = %id, ?exit_code, final_offset = current_offset, "IPC stream completed");
                        let _ = ipc::write_response_to_writer(
                            &mut writer,
                            RpcResponse::AttachStreamDone { exit_code },
                        )
                        .await;
                        break;
                    }
                }

                client_msg = client_msg_rx.recv() => {
                    match client_msg {
                        None => {
                            debug!(session_id = %id, "IPC client reader channel closed");
                            break;
                        }
                        Some(Err(err)) => {
                            warn!(session_id = %id, %err, "IPC client request read failed");
                            break;
                        }
                        Some(Ok(RpcRequest::AttachInput { id: req_id, data })) if req_id == id => {
                            trace!(session_id = %id, bytes = data.len(), "IPC client input received");
                            if session_store.attach_input(&req_id, &data).await.is_err() {
                                warn!(session_id = %id, "IPC client input forwarding failed");
                                break;
                            }
                        }
                        Some(Ok(RpcRequest::AttachResize { id: req_id, rows, cols })) if req_id == id => {
                            debug!(session_id = %id, rows, cols, "IPC client resize received");
                            if session_store.attach_resize(&req_id, rows, cols).await.is_err() {
                                warn!(session_id = %id, rows, cols, "IPC client resize forwarding failed");
                                break;
                            }
                        }
                        Some(Ok(RpcRequest::AttachDetach { id: req_id })) if req_id == id => {
                            debug!(session_id = %id, "IPC client requested detach");
                            break;
                        }
                        Some(Ok(other)) => {
                            trace!(session_id = %id, request_type = other.name(), "ignoring unrelated IPC stream request");
                        }
                    }
                }

                chunk = broadcast_rx.recv() => {
                    match chunk {
                        Ok(raw_arc) => {
                            let filtered = chunk_filter.filter(&raw_arc);
                            let filtered_len = filtered.len();
                            if !filtered.is_empty() {
                                ipc::write_response_to_writer(
                                    &mut writer,
                                    RpcResponse::AttachStreamChunk {
                                        offset: current_offset,
                                        data: filtered,
                                    },
                                )
                                .await?;
                            }
                            current_offset += raw_arc.len() as u64;
                            trace!(
                                session_id = %id,
                                raw_bytes = raw_arc.len(),
                                filtered_bytes = filtered_len,
                                current_offset,
                                "forwarded live PTY output over IPC stream"
                            );

                            if let Some(modes) = session_store.get_mode_snapshot(&id) {
                                if modes != last_modes {
                                    debug!(
                                        session_id = %id,
                                        app_cursor_keys = modes.app_cursor_keys,
                                        bracketed_paste_mode = modes.bracketed_paste_mode,
                                        "IPC stream terminal mode changed"
                                    );
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
                        Err(RecvError::Lagged(skipped)) => {
                            warn!(
                                session_id = %id,
                                skipped,
                                from_offset = current_offset,
                                "IPC stream lagged behind broadcast output; resyncing from ring"
                            );
                            let (chunks, new_end) = {
                                match session_store.attach_subscribe_init(&id, Some(current_offset)).await {
                                    Ok((c, e, rx, _bpm, _ack)) => {
                                        broadcast_rx = rx;
                                        (c, e)
                                    }
                                    Err(err) => {
                                        warn!(session_id = %id, error = err.message(&id), "IPC stream resync failed");
                                        break;
                                    }
                                }
                            };
                            let mut resync_filter = EscapeFilter::new();
                            let raw = collect_filtered_chunks(&chunks, &mut resync_filter);
                            if !raw.is_empty() {
                                debug!(
                                    session_id = %id,
                                    resync_chunks = chunks.len(),
                                    resync_bytes = raw.len(),
                                    from_offset = current_offset,
                                    to_offset = new_end,
                                    "IPC stream replayed buffered output after lag"
                                );
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
                            let exit_code = session_store.get_exit_code(&id);
                            info!(session_id = %id, ?exit_code, "IPC broadcast channel closed");
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

        Ok(())
    }
    .await;

    client_reader_task.abort();
    let _ = session_store.attach_detach(&id).await;
    debug!(session_id = %id, "IPC streaming session relay stopped");
    result
}

pub(super) async fn handle_attach_input(
    id: String,
    data: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    debug!(session_id = %id, bytes = data.len(), "handling one-shot IPC input request");
    match session_store.attach_input(&id, &data).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

pub(super) async fn handle_attach_resize(
    id: String,
    rows: u16,
    cols: u16,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    debug!(session_id = %id, rows, cols, "handling one-shot IPC resize request");
    match session_store.attach_resize(&id, rows, cols).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

pub(super) async fn handle_attach_detach(
    id: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    debug!(session_id = %id, "handling one-shot IPC detach request");
    match session_store.attach_detach(&id).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}
