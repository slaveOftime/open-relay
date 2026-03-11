use interprocess::local_socket::tokio::Stream;
use tokio::{io::BufReader, sync::mpsc};

use crate::{
    error::Result,
    ipc,
    protocol::{RpcRequest, RpcResponse},
    session::pty::EscapeFilter,
};

use super::SessionStoreHandle;

pub(super) async fn handle_attach_subscribe(
    id: String,
    from_byte_offset: Option<u64>,
    mut reader: BufReader<tokio::io::ReadHalf<Stream>>,
    mut writer: tokio::io::WriteHalf<Stream>,
    session_store: &SessionStoreHandle,
) -> Result<()> {
    use tokio::sync::broadcast::error::RecvError;

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

    let mut init_filter = EscapeFilter::new();
    let data: Vec<u8> = replay_chunks
        .iter()
        .flat_map(|(_, b)| init_filter.filter(b))
        .collect();

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

    {
        let mut store = session_store.lock().await;
        let _ = store.mark_attach_presence(&id).await;
    }

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

            client_msg = client_msg_rx.recv() => {
                match client_msg {
                    None => break,
                    Some(Err(_)) => break,
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
                    Some(Ok(_)) => {}
                }
            }

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

    client_reader_task.abort();
    let mut store = session_store.lock().await;
    let _ = store.attach_detach(&id).await;
    Ok(())
}

pub(super) async fn handle_attach_input(
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

pub(super) async fn handle_attach_resize(
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

pub(super) async fn handle_attach_detach(
    id: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    let mut store = session_store.lock().await;
    match store.attach_detach(&id).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}
