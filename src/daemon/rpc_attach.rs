use interprocess::local_socket::tokio::Stream;
use tokio::{io::BufReader, sync::mpsc};
use tracing::{debug, trace, warn};

use crate::{
    error::Result,
    ipc,
    protocol::{RpcRequest, RpcResponse},
    session::{
        AttachEvent, AttachPump,
        registry::{AttachKind, AttachRole, ControlRequest},
        resize::ResizeSubscriber,
    },
};

use super::SessionStoreHandle;

/// Stream one session's output to an IPC client.
///
/// M3-2: the output state machine (follow, coalesce, lag resync, completion
/// flush, mode tracking) lives in [`AttachPump`]; this handler is the thin
/// IPC adapter that frames pump events as [`RpcResponse`]s and forwards
/// client input/resize/detach requests.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_attach_subscribe(
    id: String,
    from_byte_offset: Option<u64>,
    incarnation: Option<u64>,
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
    role: Option<String>,
    credited: bool,
    mut reader: BufReader<tokio::io::ReadHalf<Stream>>,
    mut writer: tokio::io::WriteHalf<Stream>,
    session_store: &SessionStoreHandle,
) -> Result<()> {
    debug!(
        session_id = %id,
        from_byte_offset,
        initial_rows,
        initial_cols,
        "starting IPC streaming session relay"
    );

    // M3-4: register the attachment (control lease + viewport) *before*
    // taking the snapshot, so a controller's authorized initial geometry is
    // applied through the sequencer and is already reflected in the init.
    let request = match ControlRequest::parse(role.as_deref()) {
        Ok(request) => request,
        Err(message) => {
            return ipc::write_response_to_writer(&mut writer, RpcResponse::Error { message })
                .await;
        }
    };
    let viewport = match (from_byte_offset, initial_rows, initial_cols) {
        (None, Some(rows), Some(cols)) if rows > 0 && cols > 0 => Some((rows, cols)),
        _ => None,
    };
    let registration = match session_store
        .attach_register(&id, AttachKind::Cli, request, viewport)
        .await
    {
        Ok(registration) => registration,
        Err(err) => {
            let resp = RpcResponse::Error {
                message: err.message(&id),
            };
            return ipc::write_response_to_writer(&mut writer, resp).await;
        }
    };
    let attachment_id = registration.attachment_id;
    let mut current_role = registration.role;

    // M5-1: local IPC clients ack applied cursors, so their stream is
    // credit-gated; node-relayed subscriptions arrive with `credited:
    // false` and run ungated (the relay cannot forward mid-stream credits
    // — documented limitation until direct remote streams, M5-2).
    let credit = if credited {
        crate::session::PumpCredit::Credited { attachment_id }
    } else {
        crate::session::PumpCredit::Uncredited
    };
    let (mut pump, init) = match AttachPump::subscribe(
        session_store,
        &id,
        from_byte_offset,
        incarnation,
        credit,
    )
    .await
    {
        Ok(pair) => pair,
        Err(err) => {
            debug!(session_id = %id, error = err.message(&id), "IPC attach init failed");
            let _ = session_store.attach_detach(&id, attachment_id).await;
            let resp = RpcResponse::Error {
                message: err.message(&id),
            };
            return ipc::write_response_to_writer(&mut writer, resp).await;
        }
    };

    // Seed scrollback only for fresh interactive attaches: offset-based
    // resumes already have their terminal history, and piped attaches have no
    // terminal at all. The client only sends dimensions when interactive; the
    // seed depth is a fixed floor (see `attach_scrollback_seed`), not the
    // client's screen height, so reattach keeps a deep scrollback.
    let scrollback = match (from_byte_offset, initial_rows, initial_cols) {
        (None, Some(rows), Some(_)) if rows > 0 => session_store
            .attach_scrollback_seed(&id, rows)
            .await
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    debug!(
        session_id = %id,
        snapshot_bytes = init.data.len(),
        scrollback_bytes = scrollback.len(),
        end_offset = init.end_offset,
        running = init.running,
        app_cursor_keys = init.modes.app_cursor_keys,
        bracketed_paste_mode = init.modes.bracketed_paste_mode,
        "IPC stream init prepared"
    );
    ipc::write_response_to_writer(
        &mut writer,
        RpcResponse::AttachStreamInit {
            data: init.data,
            end_offset: init.end_offset,
            running: init.running,
            bracketed_paste_mode: init.modes.bracketed_paste_mode,
            app_cursor_keys: init.modes.app_cursor_keys,
            scrollback,
            incarnation: init.incarnation,
            attachment_id,
            role: current_role.as_str().to_owned(),
        },
    )
    .await?;

    // Control-handoff notices: every send carries the current controller id.
    let mut control_rx = session_store.subscribe_control(&id);
    debug!(session_id = %id, attachment_id, role = current_role.as_str(), "IPC stream client registered");

    // Subscribe to resize broadcasts so we can notify this client when
    // another attached client changes the PTY size.
    let mut resize_sub = ResizeSubscriber::new(session_store.subscribe_resize(&id), id.clone());

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

    let result = async {
        loop {
            tokio::select! {
                biased;

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
                        Some(Ok(RpcRequest::AttachInput { id: req_id, data, wait_for_change, .. })) if req_id == id => {
                            trace!(session_id = %id, bytes = data.len(), "IPC client input received");
                            if let Err(err) = session_store.attach_input(&req_id, Some(attachment_id), &data, wait_for_change).await {
                                // Control-gate violations are client-visible,
                                // transport failures end the stream.
                                if matches!(err, crate::session::SessionError::NotController | crate::session::SessionError::StaleAttachment) {
                                    let message = err.message(&id);
                                    if ipc::write_attach_control_frame(&mut writer, &RpcResponse::Error { message }).await.is_err() {
                                        break;
                                    }
                                } else {
                                    warn!(session_id = %id, "IPC client input forwarding failed");
                                    break;
                                }
                            }
                        }
                        Some(Ok(RpcRequest::AttachResize { id: req_id, rows, cols })) if req_id == id => {
                            debug!(session_id = %id, rows, cols, "IPC client resize received");
                            resize_sub.mark_sent(rows, cols);
                            if let Err(err) = session_store.attach_resize(&req_id, Some(attachment_id), rows, cols).await {
                                // Observers never resize the shared PTY; their
                                // declared size is recorded as a viewport only.
                                if matches!(err, crate::session::SessionError::NotController | crate::session::SessionError::StaleAttachment) {
                                    resize_sub.mark_sent(0, 0);
                                    continue;
                                }
                                warn!(session_id = %id, rows, cols, "IPC client resize forwarding failed");
                                break;
                            }
                        }
                        Some(Ok(RpcRequest::AttachAcquireControl { id: req_id })) if req_id == id => {
                            debug!(session_id = %id, attachment_id, "IPC client requested control takeover");
                            match session_store.attach_acquire_control(&req_id, attachment_id).await {
                                Ok(outcome) => current_role = outcome.role,
                                Err(err) => warn!(session_id = %id, error = err.message(&id), "control takeover failed"),
                            }
                        }
                        Some(Ok(RpcRequest::AttachAppliedCursor { id: req_id, cursor })) if req_id == id => {
                            session_store
                                .attach_report_applied(&req_id, attachment_id, cursor)
                                .await;
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

                event = pump.next() => {
                    match event {
                        AttachEvent::Chunk { offset, data } => {
                            // M6-3: raw binary frame, no base64 (ADR-0004).
                            ipc::write_attach_output_frame(&mut writer, offset, &data).await?;
                        }
                        AttachEvent::Modes(modes) => {
                            ipc::write_attach_control_frame(
                                &mut writer,
                                &RpcResponse::AttachModeChanged {
                                    app_cursor_keys: modes.app_cursor_keys,
                                    bracketed_paste_mode: modes.bracketed_paste_mode,
                                },
                            )
                            .await?;
                        }
                        AttachEvent::Done {
                            exit_code,
                            final_offset,
                        } => {
                            let _ = ipc::write_attach_control_frame(
                                &mut writer,
                                &RpcResponse::AttachStreamDone {
                                    exit_code,
                                    final_offset,
                                },
                            )
                            .await;
                            break;
                        }
                        AttachEvent::Closed => {
                            break;
                        }
                    }
                }

                // Control handoffs: every notice carries the current
                // controller id; derive this attachment's role from it.
                notice = async {
                    match control_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match notice {
                        Ok(controller) => {
                            current_role = if controller == Some(attachment_id) {
                                AttachRole::Controller
                            } else {
                                AttachRole::Observer
                            };
                            let resp = RpcResponse::AttachControlChanged {
                                role: current_role.as_str().to_owned(),
                            };
                            if ipc::write_attach_control_frame(&mut writer, &resp).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                    }
                }

                // Resize notifications from other attached clients.
                Some((rows, cols)) = resize_sub.recv_foreign() => {
                    debug!(
                        session_id = %id,
                        rows, cols,
                        "forwarding resize notification to IPC client"
                    );
                    ipc::write_attach_control_frame(
                        &mut writer,
                        &RpcResponse::AttachResized { rows, cols },
                    )
                    .await?;
                }
            }
        }

        Ok(())
    }
    .await;

    client_reader_task.abort();
    let _ = session_store.attach_detach(&id, attachment_id).await;
    debug!(session_id = %id, "IPC streaming session relay stopped");
    result
}

pub(super) async fn handle_attach_input(
    id: String,
    data: String,
    session_store: &SessionStoreHandle,
    wait_for_change: bool,
    attachment_id: Option<u64>,
) -> RpcResponse {
    debug!(session_id = %id, bytes = data.len(), "handling one-shot IPC input request");
    // `attachment_id = None` is the ungated operator one-shot (`oly send`);
    // `Some(lease)` is an agent send gated on a held control lease.
    match session_store
        .attach_input(&id, attachment_id, &data, wait_for_change)
        .await
    {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

pub(super) async fn handle_attach_busy(
    id: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    debug!(session_id = %id, "handling one-shot IPC attach busy request");
    match session_store.attach_busy(&id).await {
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
    // One-shot operator resize: not an attachment, ungated.
    match session_store.attach_resize(&id, None, rows, cols).await {
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
    // The streaming handler unregisters its own attachment on exit, so a
    // one-shot detach has nothing anonymous to remove.
    let _ = session_store;
    RpcResponse::Ack
}

// ---------------------------------------------------------------------------
// M4 agent surfaces: cursor, bounded observe windows, parked control leases
// ---------------------------------------------------------------------------

/// Machine-readable session cursor: liveness + canonical filtered offset.
pub(super) async fn handle_session_cursor(
    id: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    let offset = match session_store.attach_filtered_len(&id).await {
        Some(offset) => offset,
        None => {
            return RpcResponse::Error {
                message: format!("session not found or no output recorded: {id}"),
            };
        }
    };
    let (running, _, exit_code) = session_store
        .attach_stream_status(&id)
        .await
        .unwrap_or((false, true, None));
    RpcResponse::SessionCursor {
        running,
        exit_code,
        offset,
        incarnation: session_store.journal_incarnation(&id),
    }
}

/// One bounded filtered-stream window (agent `observe`/`history`).
pub(super) async fn handle_observe_window(
    id: String,
    from: u64,
    max_bytes: u32,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    // Hard cap regardless of what the client asked for: reads stay
    // memory-bounded (I7).
    let max_bytes = (max_bytes as usize).clamp(1, 8 * 1024 * 1024);
    let data = match session_store
        .attach_resync_window(&id, from, max_bytes)
        .await
    {
        Ok(data) => data,
        Err(err) => {
            return RpcResponse::Error {
                message: err.message(&id),
            };
        }
    };
    let next_offset = from + data.len() as u64;
    let (running, _, exit_code) = session_store
        .attach_stream_status(&id)
        .await
        .unwrap_or((false, true, None));
    RpcResponse::ObserveWindow {
        data,
        next_offset,
        running,
        exit_code,
        incarnation: session_store.journal_incarnation(&id),
    }
}

/// Acquire the control lease without a streaming attach: a parked
/// controller attachment whose id is the lease token for gated agent
/// sends. The lease carries a TTL (post-review corrective increment): an
/// agent that crashes without releasing stops gating the session after
/// the TTL instead of leaking the lease forever.
pub(super) async fn handle_control_acquire(
    id: String,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    match session_store
        .attach_register_parked(&id, AttachKind::Cli)
        .await
    {
        Ok(registration) => RpcResponse::ControlAcquired {
            lease: registration.attachment_id,
        },
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}

/// Release a parked control lease.
pub(super) async fn handle_control_release(
    id: String,
    lease: u64,
    session_store: &SessionStoreHandle,
) -> RpcResponse {
    match session_store.attach_detach(&id, lease).await {
        Ok(()) => RpcResponse::Ack,
        Err(err) => RpcResponse::Error {
            message: err.message(&id),
        },
    }
}
