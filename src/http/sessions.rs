use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use crate::{
    protocol::{
        ListQuery, ListSortField, PushSubscriptionInput, RpcRequest, RpcResponse, SortOrder,
    },
    session::{SessionStore, StartSpec, logs::read_persisted_log_page},
};

use super::AppState;

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
    /// Comma-separated status values: running,stopped,killed,failed,created,stopping
    pub status: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// Field to sort by: created_at | status | title | id | command | cwd | pid
    pub sort: Option<ListSortField>,
    /// asc | desc
    pub order: Option<SortOrder>,
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

    let query = ListQuery {
        search: params.search,
        statuses,
        since: None,
        until: None,
        limit: page_limit,
        offset: offset,
        sort: params.sort.unwrap_or_default(),
        order: params.order.unwrap_or_default(),
    };

    // If a node is specified, proxy the request to that secondary node.
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::List {
            query: query.clone(),
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::List { total, sessions }) => Json(serde_json::json!({
                "items": sessions,
                "total": total,
                "offset": offset,
                "limit": page_limit,
            }))
            .into_response(),
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

    let total = match state.db.count_summaries(&query).await {
        Ok(total) => total,
        Err(err) => {
            error!(%err, "failed to count sessions from DB");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };

    let sessions = match state.store.list_summaries(&query).await {
        Ok(sessions) => sessions,
        Err(err) => {
            error!(%err, "failed to list sessions from store");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response();
        }
    };
    Json(serde_json::json!({
        "items": sessions,
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
    #[serde(default)]
    pub disable_notifications: bool,
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
            disable_notifications: body.disable_notifications,
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
        notifications_enabled: !body.disable_notifications,
    };

    let result =
        match SessionStore::start_session_via_handle(&state.store, &state.config, spec).await {
            Ok(id) => {
                let summary = state.store.get_summary(&id);
                Ok((id, summary))
            }
            Err(err) => Err(err),
        };

    match result {
        Ok((session_id, _summary)) => {
            info!(
                session_id,
                cmd = body.cmd,
                args = ?body.args,
                title = ?body.title,
                "session created"
            );
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
                limit: 1,
                offset: 0,
                sort: ListSortField::CreatedAt,
                order: SortOrder::Desc,
            },
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::List { sessions, .. }) => {
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

    let stopped = {
        let stopped = state.store.stop_session(&id, grace).await;
        let _ = state.store.get_summary(&id);
        stopped
    };

    if stopped {
        info!(session_id = %id, grace_seconds = grace, "stop requested");
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
        let rpc = RpcRequest::Kill { id: id.clone() };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::Kill { killed }) => {
                if killed {
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

    let killed = {
        let killed = state.store.kill_session(&id).await;
        let _ = state.store.get_summary(&id);
        killed
    };

    if killed {
        info!(session_id = %id, "kill requested");
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

    match state.store.attach_input(&id, &body.data).await {
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
