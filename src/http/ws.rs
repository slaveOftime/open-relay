use axum::extract::ws::{Message, WebSocket};
use axum::{
    extract::{Path, Query, State, WebSocketUpgrade},
    response::IntoResponse,
};
use futures_util::FutureExt;
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, error, info, trace, warn};

use crate::protocol::RpcRequest;
use crate::session::registry::ControlRequest;

use crate::session::{ModeSnapshot, SessionError};

use super::AppState;
use super::attach_source::{
    AttachSource, AttachStreamEvent, InitFrame, LocalSource, LocalSourceOutput, RelayedSource,
    RelayedSourceOutput,
};

#[derive(Debug, Deserialize)]
pub struct AttachParams {
    pub node: Option<String>,
    /// Initial terminal width (cols) reported by the browser xterm instance.
    pub cols: Option<u16>,
    /// Initial terminal height (rows) reported by the browser xterm instance.
    pub rows: Option<u16>,
    /// Requested control role: observer | controller (default) | takeover.
    pub role: Option<String>,
}

// ---------------------------------------------------------------------------
// Protocol types — unified for both local and proxied sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerMessage {
    /// Initial terminal snapshot. `data` contains filtered stream bytes
    /// covering the stream up to `end_offset` in journal `incarnation`.
    Init {
        data: Vec<u8>,
        /// Stream offset immediately after the snapshot (C in ADR-0004).
        end_offset: u64,
        /// Journal incarnation the snapshot/cursor belongs to (0 = legacy).
        incarnation: u64,
        /// Whether the session was still running at attach time.
        running: bool,
        /// Authoritative child input modes at attach time. The browser
        /// terminal mirrors them with DECSET (the web equivalent of the
        /// native client's `sync_local_terminal_modes`), because the
        /// snapshot stream deliberately omits mode sequences — without
        /// this, a fresh page load into an already-mouse-enabled program
        /// (vim, htop, …) leaves xterm.js capturing nothing.
        modes: WsModes,
        /// This attachment's fencing token and granted role (M3-4).
        attachment_id: u64,
        role: &'static str,
    },
    /// Incremental PTY output chunk; `offset` is the stream offset of the
    /// first byte so clients can verify contiguity (I2).
    Data {
        offset: u64,
        data: Vec<u8>,
    },
    /// Terminal mode changed mid-stream (all input-affecting modes).
    ModeChanged {
        modes: WsModes,
    },
    /// Another attached client resized the PTY.
    Resized {
        rows: u16,
        cols: u16,
    },
    /// Session ended. `final_offset` is the end-of-stream cursor (I2
    /// completion check); 0 when unknown (proxy errors).
    SessionEnded {
        exit_code: Option<i32>,
        final_offset: u64,
    },
    Error {
        message: String,
    },
    /// Control handoff notice: this attachment's role after the change.
    Control {
        role: &'static str,
    },
    Pong,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ClientMessage {
    Input {
        data: String,
        #[serde(rename = "waitForChange")]
        wait_for_change: bool,
    },
    Busy,
    Resize {
        rows: u16,
        cols: u16,
    },
    /// Take over the control lease from an observer position.
    AcquireControl,
    /// Applied-cursor credit (M3-5, I7): highest stream offset the client
    /// has rendered.
    Ack {
        offset: u64,
    },
    Detach,
    Ping,
}

const WS_FRAME_INIT: u8 = 1;
const WS_FRAME_DATA: u8 = 2;
const WS_FRAME_MODE_CHANGED: u8 = 3;
const WS_FRAME_RESIZED: u8 = 4;
const WS_FRAME_SESSION_ENDED: u8 = 5;
const WS_FRAME_ERROR: u8 = 6;
const WS_FRAME_PONG: u8 = 7;
const WS_FRAME_CONTROL: u8 = 8;
const WS_FLAG_APP_CURSOR_KEYS: u8 = 1 << 0;
const WS_FLAG_BRACKETED_PASTE_MODE: u8 = 1 << 1;
const WS_FLAG_MOUSE_REPORT: u8 = 1 << 2;
const WS_FLAG_SGR_MOUSE: u8 = 1 << 3;
const WS_FLAG_FOCUS_EVENTS: u8 = 1 << 4;

/// Input-affecting terminal modes carried by the Init and ModeChanged
/// frames. The daemon tracks them in the engine (`ModeSnapshot`); the
/// browser needs all of them because xterm.js only captures mouse/focus
/// events while its own parsed mode state says so.
#[derive(Debug, Clone, Copy, Serialize, Default)]
pub(crate) struct WsModes {
    #[serde(rename = "appCursorKeys")]
    pub(crate) app_cursor_keys: bool,
    #[serde(rename = "bracketedPasteMode")]
    pub(crate) bracketed_paste_mode: bool,
    /// Child has mouse reporting enabled (any of 1000/1002/1003).
    #[serde(rename = "mouseReport")]
    pub(crate) mouse_report: bool,
    /// Child negotiated SGR (1006) mouse encoding.
    #[serde(rename = "sgrMouse")]
    pub(crate) sgr_mouse: bool,
    /// Child has focus in/out reporting (1004) enabled.
    #[serde(rename = "focusEvents")]
    pub(crate) focus_events: bool,
}

impl From<ModeSnapshot> for WsModes {
    fn from(modes: ModeSnapshot) -> Self {
        WsModes {
            app_cursor_keys: modes.app_cursor_keys,
            bracketed_paste_mode: modes.bracketed_paste_mode,
            mouse_report: modes.mouse_report,
            sgr_mouse: modes.sgr_mouse,
            focus_events: modes.focus_events,
        }
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub async fn attach_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<AttachParams>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // ADR-0007 (M5-4): browser cross-site WebSockets must not ride the
    // ambient auth cookie — Origin host must match the request Host.
    if !crate::http::auth::ws_origin_allowed(&headers) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            "websocket origin rejected",
        )
            .into_response();
    }
    debug!(session_id = %id, "WebSocket upgrade requested");
    ws.on_upgrade(move |socket| async move {
        let panic_session_id = id.clone();
        let panic_node = params.node.clone();
        let result = std::panic::AssertUnwindSafe(handle_ws(
            socket,
            state,
            id,
            params.node,
            AttachConnectionParams {
                initial_rows: params.rows,
                initial_cols: params.cols,
                role: params.role,
                headers,
            },
        ))
        .catch_unwind()
        .await;

        if let Err(payload) = result {
            error!(
                session_id = %panic_session_id,
                node = ?panic_node,
                panic = %panic_payload_message(payload.as_ref()),
                "attach WebSocket handler panicked"
            );
        }
    })
    .into_response()
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return message;
    }

    if let Some(message) = payload.downcast_ref::<String>() {
        return message.as_str();
    }

    "non-string panic payload"
}

fn mode_flags(modes: &WsModes) -> u8 {
    let mut flags = 0;
    if modes.app_cursor_keys {
        flags |= WS_FLAG_APP_CURSOR_KEYS;
    }
    if modes.bracketed_paste_mode {
        flags |= WS_FLAG_BRACKETED_PASTE_MODE;
    }
    if modes.mouse_report {
        flags |= WS_FLAG_MOUSE_REPORT;
    }
    if modes.sgr_mouse {
        flags |= WS_FLAG_SGR_MOUSE;
    }
    if modes.focus_events {
        flags |= WS_FLAG_FOCUS_EVENTS;
    }
    flags
}

/// Encode a server message into its binary wire frame. Pure so the layout
/// is unit-testable against the TypeScript decoder (web/src/api/client.ts).
fn encode_server_message(msg: &ServerMessage) -> Vec<u8> {
    match msg {
        ServerMessage::Init {
            data,
            end_offset,
            incarnation,
            running,
            modes,
            attachment_id,
            role,
        } => {
            let mut payload = Vec::with_capacity(28 + data.len());
            payload.push(WS_FRAME_INIT);
            payload.push(mode_flags(modes));
            payload.extend_from_slice(&end_offset.to_be_bytes());
            payload.extend_from_slice(&incarnation.to_be_bytes());
            payload.push(u8::from(*running));
            payload.extend_from_slice(&attachment_id.to_be_bytes());
            payload.push(u8::from(*role == "controller"));
            payload.extend_from_slice(data);
            payload
        }
        ServerMessage::Data { offset, data } => {
            let mut payload = Vec::with_capacity(9 + data.len());
            payload.push(WS_FRAME_DATA);
            payload.extend_from_slice(&offset.to_be_bytes());
            payload.extend_from_slice(data);
            payload
        }
        ServerMessage::ModeChanged { modes } => {
            vec![WS_FRAME_MODE_CHANGED, mode_flags(modes)]
        }
        ServerMessage::Resized { rows, cols } => {
            let mut payload = Vec::with_capacity(5);
            payload.push(WS_FRAME_RESIZED);
            payload.extend_from_slice(&rows.to_be_bytes());
            payload.extend_from_slice(&cols.to_be_bytes());
            payload
        }
        ServerMessage::SessionEnded {
            exit_code,
            final_offset,
        } => {
            let mut payload = Vec::with_capacity(14);
            payload.push(WS_FRAME_SESSION_ENDED);
            match exit_code {
                Some(code) => {
                    payload.push(1);
                    payload.extend_from_slice(&code.to_be_bytes());
                }
                None => payload.push(0),
            }
            payload.extend_from_slice(&final_offset.to_be_bytes());
            payload
        }
        ServerMessage::Error { message } => {
            let mut payload = Vec::with_capacity(1 + message.len());
            payload.push(WS_FRAME_ERROR);
            payload.extend_from_slice(message.as_bytes());
            payload
        }
        ServerMessage::Control { role } => vec![WS_FRAME_CONTROL, u8::from(*role == "controller")],
        ServerMessage::Pong => vec![WS_FRAME_PONG],
    }
}

async fn send_server_message(socket: &mut WebSocket, msg: &ServerMessage) -> bool {
    socket
        .send(Message::Binary(encode_server_message(msg).into()))
        .await
        .is_ok()
}

/// Per-connection attach parameters bundled to keep handler signatures
/// under clippy's argument limit.
struct AttachConnectionParams {
    initial_rows: Option<u16>,
    initial_cols: Option<u16>,
    role: Option<String>,
    /// Upgrade-time headers retained so a future revision can incorporate
    /// per-connection surfaces (CSRF cookies, Auth-Bearer tokens,
    /// per-message auth evidence) without changing the upgrade plumbing.
    #[allow(dead_code)]
    headers: axum::http::HeaderMap,
}

async fn handle_ws(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    node: Option<String>,
    params: AttachConnectionParams,
) {
    debug!(session_id = %id, node = ?node, "WebSocket connected");

    // Parse the role before opening either source so the dispatch error
    // path can wrap a clean ServerMessage::Error without side effects.
    let request = match ControlRequest::parse(params.role.as_deref()) {
        Ok(request) => request,
        Err(message) => {
            let _ = send_server_message(&mut socket, &ServerMessage::Error { message }).await;
            return;
        }
    };

    // ADR-0007 (M5-4): logout/revocation closes live control streams not
    // just future requests — watch the revocation epoch and re-validate the
    // connection's token before tearing the stream down.
    let reconnect_token =
        crate::http::auth::extract_request_token_parts(&params.headers, None).unwrap_or_default();

    if let Some(node_name) = node.clone() {
        match AttachSource::relayed(
            state.node_registry.clone(),
            id.clone(),
            node_name.clone(),
            params.initial_rows,
            params.initial_cols,
            params.role.clone(),
        )
        .await
        {
            Ok(RelayedSourceOutput {
                source,
                init_frame,
                tti_start,
            }) => {
                if !send_init_frame(&mut socket, init_frame, tti_start, &id, "proxied").await {
                    return;
                }
                let auth = state.auth.clone();
                let revocation_rx = auth.as_ref().map(|a| a.revocation_watch());
                serve_attach(
                    socket,
                    state,
                    id,
                    Some(node_name),
                    "proxied",
                    revocation_rx,
                    auth,
                    reconnect_token,
                    AttachSource::Relayed(source),
                )
                .await;
            }
            Err(err) => {
                let _ = send_server_message(&mut socket, &err.into_message()).await;
            }
        }
        return;
    }

    match AttachSource::local(
        state.store.clone(),
        id.clone(),
        params.initial_rows,
        params.initial_cols,
        request,
    )
    .await
    {
        Ok(LocalSourceOutput {
            source,
            init_frame,
            tti_start,
        }) => {
            if !send_init_frame(&mut socket, init_frame, tti_start, &id, "local").await {
                return;
            }
            let auth = state.auth.clone();
            let revocation_rx = auth.as_ref().map(|a| a.revocation_watch());
            serve_attach(
                socket,
                state,
                id,
                None,
                "local",
                revocation_rx,
                auth,
                String::new(),
                AttachSource::Local(source),
            )
            .await;
        }
        Err(err) => {
            let _ = send_server_message(&mut socket, &err.into_message()).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Shared serve loop — local + relayed attach via `AttachSource` (PLAN2 S2)
// ---------------------------------------------------------------------------

/// Single canonical WebSocket attach loop for both arms. The init frame
/// has already been written to `socket` by the caller's preamble (either
/// the local constructor or the relayed constructor), and a metrics label
/// has been observed; this function waits for chunks/modes/resizes/etc.
/// and dispatches client messages back into the source.
///
/// The `init_metrics_label` must be exactly one of `"local"` or
/// `"proxied"` -- the same labels pre-S2 observers expect.
async fn serve_attach(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    node: Option<String>,
    init_metrics_label: &'static str,
    mut revocation_rx: Option<tokio::sync::watch::Receiver<u64>>,
    auth: Option<Arc<crate::http::auth::AuthState>>,
    reconnect_token: String,
    mut source: AttachSource,
) {
    debug!(
        session_id = %id,
        attach_path = init_metrics_label,
        "WebSocket attach entered shared serve loop"
    );
    loop {
        // Revocation check armed only while auth is enabled and the
        // connection carried a token.
        let revoked = async {
            match (revocation_rx.as_mut(), reconnect_token.is_empty()) {
                (Some(rx), false) => {
                    let _ = rx.changed().await;
                }
                _ => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(revoked);

        tokio::select! {
            biased;

            _ = &mut revoked => {
                if let Some(ref auth_state) = auth
                    && !reconnect_token.is_empty()
                    && !auth_state.validate_token(&reconnect_token).await
                {
                    info!(
                        session_id = %id,
                        attach_path = init_metrics_label,
                        "WebSocket closed -- session token revoked"
                    );
                    let _ = send_server_message(
                        &mut socket,
                        &ServerMessage::Error {
                            message: "session revoked (logout)".to_string(),
                        },
                    )
                    .await;
                    cleanup_source(&state, &id, &node, &source).await;
                    return;
                }
            }

            // Stream events from the source.
            event = source_source(&mut source) => {
                match event {
                    Some(AttachStreamEvent::Chunk { offset, data }) => {
                        if !send_server_message(&mut socket, &ServerMessage::Data { offset, data }).await {
                            cleanup_source(&state, &id, &node, &source).await;
                            return;
                        }
                    }
                    Some(AttachStreamEvent::Modes(modes)) => {
                        if !send_server_message(&mut socket, &ServerMessage::ModeChanged { modes }).await {
                            cleanup_source(&state, &id, &node, &source).await;
                            return;
                        }
                    }
                    Some(AttachStreamEvent::Resized { rows, cols }) => {
                        if !send_server_message(&mut socket, &ServerMessage::Resized { rows, cols }).await {
                            cleanup_source(&state, &id, &node, &source).await;
                            return;
                        }
                    }
                    Some(AttachStreamEvent::Control { role }) => {
                        if !send_server_message(&mut socket, &ServerMessage::Control { role }).await {
                            cleanup_source(&state, &id, &node, &source).await;
                            return;
                        }
                    }
                    Some(AttachStreamEvent::Done { exit_code, final_offset }) => {
                        info!(
                            session_id = %id,
                            attach_path = init_metrics_label,
                            ?exit_code,
                            final_offset,
                            "WebSocket session ended"
                        );
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded { exit_code, final_offset }).await;
                        cleanup_source(&state, &id, &node, &source).await;
                        return;
                    }
                    Some(AttachStreamEvent::Closed) | None => {
                        let final_offset = pump_current_offset(&source);
                        let _ = send_server_message(&mut socket, &ServerMessage::SessionEnded {
                            exit_code: None,
                            final_offset,
                        }).await;
                        cleanup_source(&state, &id, &node, &source).await;
                        return;
                    }
                }
            }

            // Client messages.
            msg = socket.recv() => {
                let keep = handle_client_message(
                    msg,
                    &state,
                    &id,
                    &node,
                    &mut source,
                    &mut socket,
                    init_metrics_label,
                )
                .await;
                if !keep {
                    cleanup_source(&state, &id, &node, &source).await;
                    return;
                }
            }
        }
    }
}

/// Drain the source's next event: `AttachPump::next` for local, the
/// relayed mpsc for proxied. Returns `None` if the relevant primitive has
/// been dropped (transport-level closure).
async fn source_source(source: &mut AttachSource) -> Option<AttachStreamEvent> {
    match source {
        AttachSource::Local(local) => local.next_event().await,
        AttachSource::Relayed(relayed) => relayed.next_event().await,
    }
}

/// Resolves the pump's `current_offset` for the local arm; the relayed arm
/// has no concept of a live offset, so this returns `0` for it.
fn pump_current_offset(source: &AttachSource) -> u64 {
    match source {
        AttachSource::Local(local) => local.pump.current_offset(),
        AttachSource::Relayed(_) => 0,
    }
}

/// Source-side cleanup on every drop path: detach local, remove_pending
/// relay stream entry. Idempotent.
async fn cleanup_source(state: &AppState, id: &str, node: &Option<String>, source: &AttachSource) {
    match source {
        AttachSource::Local(local) => {
            let _ = state.store.attach_detach(id, local.attachment_id).await;
            debug!(
                session_id = %id,
                attachment_id = local.attachment_id,
                "local attach detached at serve-loop exit"
            );
        }
        AttachSource::Relayed(relayed) => {
            let node_name = node.as_deref().unwrap_or(&relayed.node);
            state
                .node_registry
                .remove_pending(node_name, &relayed.stream_rpc_id)
                .await;
            debug!(
                session_id = %id,
                node = %node_name,
                "relayed attach pending entry removed at serve-loop exit"
            );
        }
    }
}

/// Handle one inbound WS message. Returns `false` if the loop should exit.
#[allow(clippy::too_many_arguments)]
async fn handle_client_message(
    msg: Option<std::result::Result<Message, axum::Error>>,
    state: &AppState,
    id: &str,
    node: &Option<String>,
    source: &mut AttachSource,
    socket: &mut WebSocket,
    init_metrics_label: &'static str,
) -> bool {
    let message = match msg {
        Some(Ok(message)) => message,
        Some(Err(err)) => {
            warn!(
                session_id = %id,
                attach_path = init_metrics_label,
                %err,
                "WebSocket receive error"
            );
            return false;
        }
        None => {
            debug!(
                session_id = %id,
                attach_path = init_metrics_label,
                "WebSocket client disconnected"
            );
            return false;
        }
    };

    let text = match message {
        Message::Text(text) => text,
        Message::Close(_) => {
            debug!(
                session_id = %id,
                attach_path = init_metrics_label,
                "WebSocket Close frame received"
            );
            return false;
        }
        Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => return true,
    };

    let client_message: ClientMessage = match serde_json::from_str(&text) {
        Ok(message) => message,
        Err(err) => {
            warn!(
                session_id = %id,
                attach_path = init_metrics_label,
                %err,
                "failed to parse WebSocket client message"
            );
            return true;
        }
    };

    apply_client_message(
        client_message,
        state,
        id,
        node,
        source,
        socket,
        init_metrics_label,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn apply_client_message(
    msg: ClientMessage,
    state: &AppState,
    id: &str,
    node: &Option<String>,
    source: &mut AttachSource,
    socket: &mut WebSocket,
    init_metrics_label: &'static str,
) -> bool {
    match source {
        AttachSource::Local(local) => {
            apply_client_message_local(msg, state, id, local, socket, init_metrics_label).await
        }
        AttachSource::Relayed(_) => {
            let (relayed, node_name) = match source {
                AttachSource::Relayed(relayed) => {
                    let node_name = match node.as_deref() {
                        Some(name) => name.to_string(),
                        None => relayed.node.clone(),
                    };
                    (relayed, node_name)
                }
                AttachSource::Local(_) => unreachable!(),
            };
            apply_client_message_relayed(
                msg,
                state,
                id,
                &node_name,
                relayed,
                socket,
                init_metrics_label,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_client_message_local(
    msg: ClientMessage,
    state: &AppState,
    id: &str,
    source: &mut LocalSource,
    socket: &mut WebSocket,
    init_metrics_label: &'static str,
) -> bool {
    match msg {
        ClientMessage::Input {
            data,
            wait_for_change,
        } => {
            debug!(
                session_id = %id,
                attach_path = init_metrics_label,
                bytes = data.len(),
                "WS input received"
            );
            let outcome = state
                .store
                .attach_input(
                    id,
                    Some(source.attachment_id),
                    data.as_bytes(),
                    wait_for_change,
                )
                .await;
            if let Err(err) = outcome {
                let gated = matches!(
                    err,
                    SessionError::NotController | SessionError::StaleAttachment
                );
                if !send_server_message(
                    socket,
                    &ServerMessage::Error {
                        message: err.message(id),
                    },
                )
                .await
                {
                    return false;
                }
                if !gated {
                    return false;
                }
            }
            true
        }
        ClientMessage::Busy => {
            trace!(
                session_id = %id,
                attach_path = init_metrics_label,
                "WS busy received"
            );
            if let Err(err) = state.store.attach_busy(id).await
                && !send_server_message(
                    socket,
                    &ServerMessage::Error {
                        message: err.message(id),
                    },
                )
                .await
            {
                return false;
            }
            true
        }
        ClientMessage::Resize { rows, cols } => {
            debug!(
                session_id = %id,
                attach_path = init_metrics_label,
                rows,
                cols,
                "WS resize received"
            );
            source.resize_sub.mark_sent(rows, cols);
            match state
                .store
                .attach_resize(id, Some(source.attachment_id), rows, cols)
                .await
            {
                Ok(()) => true,
                Err(SessionError::NotController | SessionError::StaleAttachment) => {
                    source.resize_sub.mark_sent(0, 0);
                    true
                }
                Err(err) => {
                    let _ = send_server_message(
                        socket,
                        &ServerMessage::Error {
                            message: err.message(id),
                        },
                    )
                    .await;
                    false
                }
            }
        }
        ClientMessage::AcquireControl => {
            debug!(
                session_id = %id,
                attach_path = init_metrics_label,
                attachment_id = source.attachment_id,
                "WS control takeover requested"
            );
            if let Err(err) = state
                .store
                .attach_acquire_control(id, source.attachment_id)
                .await
            {
                warn!(
                    session_id = %id,
                    attach_path = init_metrics_label,
                    error = err.message(id),
                    "control takeover failed"
                );
            }
            true
        }
        ClientMessage::Ack { offset } => {
            state
                .store
                .attach_report_applied(id, source.attachment_id, offset)
                .await;
            true
        }
        ClientMessage::Detach => {
            debug!(
                session_id = %id,
                attach_path = init_metrics_label,
                "WS client detached"
            );
            let _ = state.store.attach_detach(id, source.attachment_id).await;
            false
        }
        ClientMessage::Ping => {
            let _ = send_server_message(socket, &ServerMessage::Pong).await;
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_client_message_relayed(
    msg: ClientMessage,
    state: &AppState,
    id: &str,
    node: &str,
    source: &mut RelayedSource,
    socket: &mut WebSocket,
    init_metrics_label: &'static str,
) -> bool {
    match msg {
        ClientMessage::Input {
            data,
            wait_for_change,
        } => {
            debug!(
                session_id = %id,
                node = %node,
                attach_path = init_metrics_label,
                bytes = data.len(),
                "relayed WS input received"
            );
            let rpc = RpcRequest::AttachInput {
                id: id.to_string(),
                data: data.into_bytes(),
                wait_for_change,
                attachment_id: None,
            };
            if let Err(err) = state
                .node_registry
                .proxy_rpc_stream_message(node, &source.stream_rpc_id, &rpc)
                .await
            {
                warn!(
                    session_id = %id,
                    node = %node,
                    attach_path = init_metrics_label,
                    %err,
                    "failed to proxy WebSocket input"
                );
            }
            true
        }
        ClientMessage::Busy => {
            trace!(
                session_id = %id,
                node = %node,
                attach_path = init_metrics_label,
                "relayed WS busy received"
            );
            let rpc = RpcRequest::AttachBusy { id: id.to_string() };
            if let Err(err) = state.node_registry.proxy_rpc(node, &rpc).await {
                warn!(
                    session_id = %id,
                    node = %node,
                    attach_path = init_metrics_label,
                    %err,
                    "failed to proxy WebSocket busy"
                );
            }
            true
        }
        ClientMessage::Resize { rows, cols } => {
            debug!(
                session_id = %id,
                node = %node,
                attach_path = init_metrics_label,
                rows,
                cols,
                "relayed WS resize received"
            );
            let rpc = RpcRequest::AttachResize {
                id: id.to_string(),
                rows,
                cols,
            };
            if let Err(err) = state
                .node_registry
                .proxy_rpc_stream_message(node, &source.stream_rpc_id, &rpc)
                .await
            {
                warn!(
                    session_id = %id,
                    node = %node,
                    attach_path = init_metrics_label,
                    %err,
                    rows,
                    cols,
                    "failed to proxy WebSocket resize"
                );
            }
            true
        }
        ClientMessage::AcquireControl => {
            debug!(
                session_id = %id,
                node = %node,
                attach_path = init_metrics_label,
                "relayed WS control takeover requested"
            );
            let rpc = RpcRequest::AttachAcquireControl { id: id.to_string() };
            if let Err(err) = state
                .node_registry
                .proxy_rpc_stream_message(node, &source.stream_rpc_id, &rpc)
                .await
            {
                warn!(
                    session_id = %id,
                    node = %node,
                    attach_path = init_metrics_label,
                    %err,
                    "failed to proxy WebSocket control takeover"
                );
            }
            true
        }
        ClientMessage::Ack { offset } => {
            let rpc = RpcRequest::AttachAppliedCursor {
                id: id.to_string(),
                cursor: offset,
            };
            if let Err(err) = state
                .node_registry
                .proxy_rpc_stream_message(node, &source.stream_rpc_id, &rpc)
                .await
            {
                warn!(
                    session_id = %id,
                    node = %node,
                    attach_path = init_metrics_label,
                    %err,
                    "failed to proxy WebSocket applied-cursor credit"
                );
            }
            true
        }
        ClientMessage::Detach => {
            debug!(
                session_id = %id,
                node = %node,
                attach_path = init_metrics_label,
                "relayed WS detach requested"
            );
            let rpc = RpcRequest::AttachDetach { id: id.to_string() };
            if let Err(err) = state
                .node_registry
                .proxy_rpc_stream_message(node, &source.stream_rpc_id, &rpc)
                .await
            {
                warn!(
                    session_id = %id,
                    node = %node,
                    attach_path = init_metrics_label,
                    %err,
                    "failed to proxy WebSocket detach"
                );
            }
            false
        }
        ClientMessage::Ping => {
            let _ = send_server_message(socket, &ServerMessage::Pong).await;
            true
        }
    }
}

/// Bridge: the caller's preamble has already produced an `InitFrame` and
/// sent it on the wire. This wrapper hides that detail so the dispatch in
/// `handle_ws` reads naturally.
async fn send_init_frame(
    socket: &mut WebSocket,
    init_frame: InitFrame,
    tti_start: Instant,
    id: &str,
    init_metrics_label: &'static str,
) -> bool {
    let data_len = init_frame.data.len();
    let init_msg = init_frame.into_message();
    if !send_server_message(socket, &init_msg).await {
        debug!(
            session_id = %id,
            attach_path = init_metrics_label,
            "WebSocket closed before init frame could be sent"
        );
        return false;
    }
    crate::metrics::observe("attach_init", Some(init_metrics_label), tti_start.elapsed());
    debug!(
        session_id = %id,
        attach_path = init_metrics_label,
        snapshot_bytes = data_len,
        "WebSocket stream initialized"
    );
    true
}

/// Merge a scrollback seed into the attach init payload the way the native
/// client writes it: seed bytes (CRLF-normalized, guaranteed scrolled off)
/// first, then the screen snapshot. Requires a declared viewport — without
/// the client's row count the scroll-off padding cannot be computed, so the
/// snapshot goes out unseeded (same rule as the native attach).
pub(crate) fn seed_web_init_data(
    seed: Option<Vec<u8>>,
    rows: Option<u16>,
    snapshot: Vec<u8>,
) -> Vec<u8> {
    match (seed, rows) {
        (Some(seed), Some(rows)) if rows > 0 && !seed.is_empty() => {
            let mut data = crate::session::scrollback_seed_bytes(&seed, rows);
            data.extend_from_slice(&snapshot);
            data
        }
        _ => snapshot,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ServerMessage, WS_FLAG_FOCUS_EVENTS, WS_FLAG_MOUSE_REPORT, WS_FLAG_SGR_MOUSE,
        WS_FRAME_CONTROL, WS_FRAME_DATA, WS_FRAME_INIT, WS_FRAME_SESSION_ENDED, WsModes,
        encode_server_message, panic_payload_message, seed_web_init_data,
    };

    #[test]
    fn web_init_data_prepends_formatted_scrollback_seed() {
        // Seed rows scroll into history, then the snapshot repaints the
        // screen — the same byte stream the native attach writes.
        let data = seed_web_init_data(Some(b"old row\n".to_vec()), Some(2), b"SNAP".to_vec());
        assert_eq!(data, b"old row\r\n\n\n\x1b[HSNAP".as_slice());
    }

    #[test]
    fn web_init_data_skips_seed_without_viewport_or_history() {
        // No declared rows: scroll-off padding cannot be computed.
        let data = seed_web_init_data(Some(b"old row\n".to_vec()), None, b"SNAP".to_vec());
        assert_eq!(data, b"SNAP".as_slice());
        // No scrolled-off history (e.g. alternate-screen sessions).
        let data = seed_web_init_data(None, Some(24), b"SNAP".to_vec());
        assert_eq!(data, b"SNAP".as_slice());
        let data = seed_web_init_data(Some(Vec::new()), Some(24), b"SNAP".to_vec());
        assert_eq!(data, b"SNAP".as_slice());
    }

    #[test]
    fn panic_payload_message_formats_static_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("attach panic");
        assert_eq!(panic_payload_message(payload.as_ref()), "attach panic");
    }

    #[test]
    fn panic_payload_message_formats_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("attach panic"));
        assert_eq!(panic_payload_message(payload.as_ref()), "attach panic");
    }

    /// Protocol evidence (post-review corrective increment, W4): the shared
    /// fixture tests/fixtures/ws_frames.json pins the wire format for BOTH
    /// endpoints. Here: encoding each `expect` description reproduces the
    /// pinned bytes exactly. The web decoder test decodes the same bytes
    /// back to the same description — encoder and decoder can never drift
    /// from each other without one side failing.
    #[test]
    fn ws_frame_fixture_matches_the_encoder() {
        fn decode_hex(hex: &str) -> Vec<u8> {
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex byte"))
                .collect()
        }
        fn to_hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ws_frames.json");
        let fixture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("read ws_frames.json"))
                .expect("parse ws_frames.json");
        let frames = fixture["frames"].as_array().expect("frames array");
        assert!(frames.len() >= 10, "the fixture covers every frame kind");

        for frame in frames {
            let name = frame["name"].as_str().expect("name");
            let pinned = frame["hex"].as_str().expect("hex");
            let expect = &frame["expect"];
            let kind = expect["type"].as_str().expect("expect.type");
            let role = match expect["role"].as_str() {
                Some("controller") => "controller",
                _ => "observer",
            };
            let message = match kind {
                "init" => ServerMessage::Init {
                    data: decode_hex(expect["data_hex"].as_str().unwrap_or("")),
                    end_offset: expect["end_offset"].as_u64().expect("end_offset"),
                    incarnation: expect["incarnation"].as_u64().expect("incarnation"),
                    running: expect["running"].as_bool().expect("running"),
                    modes: WsModes {
                        app_cursor_keys: expect["app_cursor_keys"].as_bool().expect("ack"),
                        bracketed_paste_mode: expect["bracketed_paste_mode"]
                            .as_bool()
                            .expect("bpm"),
                        mouse_report: expect["mouse_report"].as_bool().expect("mouse_report"),
                        sgr_mouse: expect["sgr_mouse"].as_bool().expect("sgr_mouse"),
                        focus_events: expect["focus_events"].as_bool().expect("focus_events"),
                    },
                    attachment_id: expect["attachment_id"].as_u64().expect("attachment_id"),
                    role,
                },
                "data" => ServerMessage::Data {
                    offset: expect["offset"].as_u64().expect("offset"),
                    data: decode_hex(expect["data_hex"].as_str().unwrap_or("")),
                },
                "mode_changed" => ServerMessage::ModeChanged {
                    modes: WsModes {
                        app_cursor_keys: expect["app_cursor_keys"].as_bool().expect("ack"),
                        bracketed_paste_mode: expect["bracketed_paste_mode"]
                            .as_bool()
                            .expect("bpm"),
                        mouse_report: expect["mouse_report"].as_bool().expect("mouse_report"),
                        sgr_mouse: expect["sgr_mouse"].as_bool().expect("sgr_mouse"),
                        focus_events: expect["focus_events"].as_bool().expect("focus_events"),
                    },
                },
                "resized" => ServerMessage::Resized {
                    rows: expect["rows"].as_u64().expect("rows") as u16,
                    cols: expect["cols"].as_u64().expect("cols") as u16,
                },
                "session_ended" => ServerMessage::SessionEnded {
                    exit_code: expect["exit_code"].as_i64().map(|c| c as i32),
                    final_offset: expect["final_offset"].as_u64().expect("final_offset"),
                },
                "error" => ServerMessage::Error {
                    message: expect["message"].as_str().expect("message").to_string(),
                },
                "control" => ServerMessage::Control { role },
                "pong" => ServerMessage::Pong,
                other => panic!("{name}: unknown frame type {other}"),
            };
            let encoded = to_hex(&encode_server_message(&message));
            assert_eq!(encoded, pinned, "frame {name} diverged from the fixture");
        }
    }

    /// Frame-layout golden checks: the TypeScript decoder
    /// (web/src/api/client.ts) must read exactly these bytes.
    #[test]
    fn encode_init_frame_carries_cursor_and_incarnation() {
        let payload = encode_server_message(&ServerMessage::Init {
            data: b"hi".to_vec(),
            end_offset: 0x0102_0304_0506_0708,
            incarnation: 9,
            running: true,
            modes: WsModes {
                app_cursor_keys: true,
                ..WsModes::default()
            },
            attachment_id: 42,
            role: "controller",
        });
        assert_eq!(payload[0], WS_FRAME_INIT);
        assert_eq!(payload[1], 1); // app-cursor-keys flag only
        assert_eq!(payload[2..10], 0x0102_0304_0506_0708_u64.to_be_bytes());
        assert_eq!(payload[10..18], 9_u64.to_be_bytes());
        assert_eq!(payload[18], 1); // running
        assert_eq!(payload[19..27], 42_u64.to_be_bytes()); // attachment id
        assert_eq!(payload[27], 1); // role: controller
        assert_eq!(&payload[28..], b"hi");
    }

    /// Mouse/focus mode bits must reach the browser: xterm.js only
    /// captures mouse and focus events while the client mirrors them,
    /// so the flags ride the INIT and MODE_CHANGED flag byte.
    #[test]
    fn encode_mode_frames_carries_mouse_and_focus_flags() {
        let modes = WsModes {
            app_cursor_keys: false,
            bracketed_paste_mode: false,
            mouse_report: true,
            sgr_mouse: true,
            focus_events: true,
        };
        assert_eq!(
            encode_server_message(&ServerMessage::ModeChanged { modes })[1],
            WS_FLAG_MOUSE_REPORT | WS_FLAG_SGR_MOUSE | WS_FLAG_FOCUS_EVENTS
        );
        let payload = encode_server_message(&ServerMessage::Init {
            data: Vec::new(),
            end_offset: 0,
            incarnation: 0,
            running: false,
            modes,
            attachment_id: 0,
            role: "observer",
        });
        assert_eq!(
            payload[1],
            WS_FLAG_MOUSE_REPORT | WS_FLAG_SGR_MOUSE | WS_FLAG_FOCUS_EVENTS
        );
    }

    #[test]
    fn encode_control_frame_layout() {
        assert_eq!(
            encode_server_message(&ServerMessage::Control { role: "controller" }),
            vec![WS_FRAME_CONTROL, 1]
        );
        assert_eq!(
            encode_server_message(&ServerMessage::Control { role: "observer" }),
            vec![WS_FRAME_CONTROL, 0]
        );
    }

    #[test]
    fn encode_data_frame_carries_chunk_offset() {
        let payload = encode_server_message(&ServerMessage::Data {
            offset: 42,
            data: b"xy".to_vec(),
        });
        assert_eq!(payload[0], WS_FRAME_DATA);
        assert_eq!(payload[1..9], 42_u64.to_be_bytes());
        assert_eq!(&payload[9..], b"xy");
    }

    #[test]
    fn encode_session_ended_frame_carries_final_offset() {
        let payload = encode_server_message(&ServerMessage::SessionEnded {
            exit_code: Some(3),
            final_offset: 99,
        });
        assert_eq!(payload[0], WS_FRAME_SESSION_ENDED);
        assert_eq!(payload[1], 1);
        assert_eq!(payload[2..6], 3_i32.to_be_bytes());
        assert_eq!(payload[6..14], 99_u64.to_be_bytes());
    }

    #[test]
    fn panic_payload_message_handles_unknown_payloads() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_u8);
        assert_eq!(
            panic_payload_message(payload.as_ref()),
            "non-string panic payload"
        );
    }
}
