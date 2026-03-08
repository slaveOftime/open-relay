use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::io::BufRead;
use tracing::{debug, error, info, warn};

use crate::{
    protocol::{ListQuery, PushSubscriptionInput, RpcRequest, RpcResponse},
    session::StartSpec,
};

use super::{AppState, SessionEvent};

// ---------------------------------------------------------------------------
// Shared param for optional node routing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct NodeParams {
    pub node: Option<String>,
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

pub async fn health() -> impl IntoResponse {
    Json(serde_json::json!({
        "ok": true,
        "daemon_pid": std::process::id()
    }))
}

// ---------------------------------------------------------------------------
// Push notifications
// ---------------------------------------------------------------------------

pub async fn push_public_key(State(state): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "public_key": state.config.web_push_vapid_public_key.clone()
    }))
}

pub async fn subscribe_push(
    State(state): State<AppState>,
    Json(body): Json<PushSubscriptionInput>,
) -> impl IntoResponse {
    let endpoint = body.endpoint.trim();
    let p256dh = body.keys.p256dh.trim();
    let auth = body.keys.auth.trim();

    if endpoint.is_empty() || p256dh.is_empty() || auth.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid push subscription payload" })),
        )
            .into_response();
    }

    match state.db.upsert_push_subscription(&body).await {
        Ok(()) => {
            info!(endpoint = %crate::utils::get_base_url(endpoint), "push subscription upserted");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(err) => {
            error!(%err, "failed to upsert push subscription");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UnsubscribePushBody {
    pub endpoint: String,
}

pub async fn unsubscribe_push(
    State(state): State<AppState>,
    Json(body): Json<UnsubscribePushBody>,
) -> impl IntoResponse {
    let endpoint = body.endpoint.trim();
    if endpoint.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "endpoint is required" })),
        )
            .into_response();
    }

    match state.db.delete_push_subscription(endpoint).await {
        Ok(deleted) => Json(serde_json::json!({ "ok": true, "deleted": deleted })).into_response(),
        Err(err) => {
            error!(%err, "failed to delete push subscription");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// List sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub search: Option<String>,
    /// Comma-separated status values: running,stopped,failed,created,stopping
    pub status: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Field to sort by: created_at | status | title
    pub sort: Option<String>,
    /// asc | desc
    pub order: Option<String>,
    /// If set, proxy the list request to this connected secondary node.
    pub node: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> impl IntoResponse {
    let statuses: Vec<String> = params
        .status
        .map(|s| {
            s.split(',')
                .map(|p| p.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let page_limit = params.limit.unwrap_or(20).max(1).min(200);
    let offset = params.offset.unwrap_or(0);

    // Use usize::MAX so apply() doesn't truncate — we do pagination ourselves
    let query = ListQuery {
        search: params.search,
        statuses,
        since: None,
        until: None,
        limit: usize::MAX,
    };

    // If a node is specified, proxy the request to that secondary node.
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::List { query };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::List {
                sessions: mut remote_sessions,
            }) => {
                if let Some(sort_field) = &params.sort {
                    let desc = params.order.as_deref() == Some("desc");
                    remote_sessions.sort_by(|a, b| {
                        let ord = match sort_field.as_str() {
                            "status" => a.status.cmp(&b.status),
                            "title" => a
                                .title
                                .as_deref()
                                .unwrap_or("")
                                .cmp(b.title.as_deref().unwrap_or("")),
                            _ => a.created_at.cmp(&b.created_at),
                        };
                        if desc { ord.reverse() } else { ord }
                    });
                }
                let total = remote_sessions.len();
                let items: Vec<_> = remote_sessions
                    .into_iter()
                    .skip(offset)
                    .take(page_limit)
                    .collect();
                Json(serde_json::json!({
                    "items": items,
                    "total": total,
                    "offset": offset,
                    "limit": page_limit,
                }))
                .into_response()
            }
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    let mut store = state.store.lock().await;
    let mut sessions = store.list_summaries(&query).await;

    if let Some(sort_field) = &params.sort {
        let desc = params.order.as_deref() == Some("desc");
        sessions.sort_by(|a, b| {
            let ord = match sort_field.as_str() {
                "status" => a.status.cmp(&b.status),
                "title" => a
                    .title
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.title.as_deref().unwrap_or("")),
                _ => a.created_at.cmp(&b.created_at),
            };
            if desc { ord.reverse() } else { ord }
        });
    }

    let total = sessions.len();
    let items: Vec<_> = sessions.into_iter().skip(offset).take(page_limit).collect();

    Json(serde_json::json!({
        "items": items,
        "total": total,
        "offset": offset,
        "limit": page_limit,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Create session
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSessionBody {
    pub cmd: String,
    pub args: Option<Vec<String>>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    /// If set, create the session on this connected secondary node instead of locally.
    pub node: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateSessionBody>,
) -> impl IntoResponse {
    // Proxy to a secondary node if requested.
    if let Some(ref node) = body.node {
        let rpc = RpcRequest::Start {
            title: body.title.clone(),
            cmd: body.cmd.clone(),
            args: body.args.clone().unwrap_or_default(),
            cwd: body.cwd.clone(),
            rows: body.rows,
            cols: body.cols,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::Start { session_id }) => {
                info!(
                    session_id,
                    cmd = body.cmd,
                    node = node,
                    "remote session created"
                );
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({ "session_id": session_id })),
                )
                    .into_response()
            }
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    let spec = StartSpec {
        title: body.title.clone(),
        cmd: body.cmd.clone(),
        args: body.args.clone().unwrap_or_default(),
        cwd: body.cwd.clone(),
        rows: body.rows,
        cols: body.cols,
    };

    let result = {
        let mut store = state.store.lock().await;
        store
            .start_session(&state.config, spec)
            .await
            .and_then(|id| {
                let summary = store.get_summary(&id);
                Ok((id, summary))
            })
    };

    match result {
        Ok((session_id, summary)) => {
            info!(
                session_id,
                cmd = body.cmd,
                args = ?body.args,
                title = ?body.title,
                "session created"
            );
            if let Some(s) = summary {
                let _ = state.event_tx.send(SessionEvent::SessionCreated(s));
            }
            (
                StatusCode::CREATED,
                Json(serde_json::json!({ "session_id": session_id })),
            )
                .into_response()
        }
        Err(err) => {
            error!(%err, "failed to create session");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Get single session
// ---------------------------------------------------------------------------

pub async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<NodeParams>,
) -> impl IntoResponse {
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::List {
            query: ListQuery {
                search: Some(id.clone()),
                statuses: vec![],
                since: None,
                until: None,
                limit: 1000,
            },
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::List { sessions }) => {
                if let Some(s) = sessions.into_iter().find(|s| s.id == id) {
                    Json(s).into_response()
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "error": format!("session not found: {id}") })),
                    )
                        .into_response()
                }
            }
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    match state.db.get_session(&id).await {
        Ok(Some(meta)) => {
            use crate::db::meta_to_summary;
            Json(meta_to_summary(&meta, false)).into_response()
        }
        Ok(None) => {
            debug!(session_id = %id, "session not found");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session not found: {id}") })),
            )
                .into_response()
        }
        Err(err) => {
            error!(session_id = %id, %err, "failed to read session from DB");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Stop / Kill
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct StopBody {
    pub grace_seconds: Option<u64>,
}

pub async fn stop_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<NodeParams>,
    body: Option<Json<StopBody>>,
) -> impl IntoResponse {
    let grace = body
        .and_then(|b| b.grace_seconds)
        .unwrap_or(state.config.stop_grace_seconds);

    if let Some(ref node) = params.node {
        let rpc = RpcRequest::Stop {
            id: id.clone(),
            grace_seconds: grace,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::Stop { stopped }) => {
                if stopped {
                    Json(serde_json::json!({ "stopped": true })).into_response()
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "error": format!("session not found: {id}") })),
                    )
                        .into_response()
                }
            }
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    let (stopped, summary) = {
        let mut store = state.store.lock().await;
        let stopped = store.stop_session(&id, grace).await;
        let summary = store.get_summary(&id);
        (stopped, summary)
    };

    if stopped {
        info!(session_id = %id, grace_seconds = grace, "stop requested");
        if let Some(s) = summary {
            let _ = state.event_tx.send(SessionEvent::SessionUpdated(s));
        }
        Json(serde_json::json!({ "stopped": true })).into_response()
    } else {
        warn!(session_id = %id, "stop: session not found");
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("session not found: {id}") })),
        )
            .into_response()
    }
}

pub async fn kill_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<NodeParams>,
) -> impl IntoResponse {
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::Stop {
            id: id.clone(),
            grace_seconds: 0,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::Stop { stopped }) => {
                if stopped {
                    Json(serde_json::json!({ "killed": true })).into_response()
                } else {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "error": format!("session not found: {id}") })),
                    )
                        .into_response()
                }
            }
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    let (stopped, summary) = {
        let mut store = state.store.lock().await;
        let stopped = store.stop_session(&id, 0).await;
        let summary = store.get_summary(&id);
        (stopped, summary)
    };

    if stopped {
        info!(session_id = %id, "kill requested");
        if let Some(s) = summary {
            let _ = state.event_tx.send(SessionEvent::SessionUpdated(s));
        }
        Json(serde_json::json!({ "killed": true })).into_response()
    } else {
        warn!(session_id = %id, "kill: session not found");
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("session not found: {id}") })),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct InputBody {
    pub data: String,
}

pub async fn send_input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<NodeParams>,
    Json(body): Json<InputBody>,
) -> impl IntoResponse {
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::AttachInput {
            id: id.clone(),
            data: body.data.clone(),
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::Ack) => Json(serde_json::json!({ "ok": true })).into_response(),
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    let mut store = state.store.lock().await;
    match store.attach_input(&id, &body.data).await {
        Ok(()) => {
            debug!(session_id = %id, bytes = body.data.len(), "input forwarded");
            Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(err) => {
            warn!(session_id = %id, error = err.message(&id), "input failed");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": err.message(&id) })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LogsParams {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    /// If set, proxy the logs request to this connected secondary node.
    pub node: Option<String>,
}

fn read_persisted_log_page(
    session_dir: &std::path::Path,
    offset: usize,
    limit: usize,
) -> Option<(Vec<String>, usize)> {
    let file = std::fs::File::open(session_dir.join("output.log")).ok()?;
    let mut reader = std::io::BufReader::new(file);

    let end = offset.saturating_add(limit);
    let mut total = 0usize;
    let mut lines = Vec::with_capacity(limit);

    loop {
        let mut buf = String::new();
        let bytes_read = reader.read_line(&mut buf).ok()?;
        if bytes_read == 0 {
            break;
        }

        if total >= offset && total < end {
            lines.push(buf);
        }
        total += 1;
    }

    Some((lines, total))
}

pub async fn get_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LogsParams>,
) -> impl IntoResponse {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(200).clamp(1, 5000);

    // Proxy to remote node: use LogsSnapshot to get in-memory output buffer.
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::LogsSnapshot {
            id: id.clone(),
            tail: usize::MAX,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::LogsSnapshot {
                lines,
                cursor: _,
                running,
            }) => {
                let total = lines.len();
                let page: Vec<_> = lines.into_iter().skip(offset).take(limit).collect();
                let next_offset = (offset + page.len()).min(total);
                Json(serde_json::json!({
                    "lines": page,
                    "offset": offset,
                    "limit": limit,
                    "total": total,
                    "has_more": next_offset < total,
                    "next_offset": next_offset,
                    "running": running,
                }))
                .into_response()
            }
            Ok(RpcResponse::Error { message }) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "unexpected response from node" })),
            )
                .into_response(),
        };
    }

    let session_dir = match state.db.get_session_dir(&id).await {
        Ok(Some(dir)) => dir,
        Ok(None) => {
            debug!(session_id = %id, "session not found for logs");
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session not found: {id}") })),
            )
                .into_response();
        }
        Err(err) => {
            error!(session_id = %id, %err, "failed to resolve session dir from DB");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    match read_persisted_log_page(&session_dir, offset, limit) {
        Some((lines, total)) => {
            let next_offset = offset.saturating_add(lines.len()).min(total);
            Json(serde_json::json!({
                "lines": lines,
                "offset": offset,
                "limit": limit,
                "total": total,
                "has_more": next_offset < total,
                "next_offset": next_offset,
                "running": false,
            }))
            .into_response()
        }
        None => {
            debug!(session_id = %id, "session not found for logs (disk)");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session not found: {id}") })),
            )
                .into_response()
        }
    }
}
