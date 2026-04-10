use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path as FsPath};
use tracing::{debug, error, info, warn};

use crate::{
    db::meta_to_summary,
    protocol::{
        ListQuery, ListSortField, PushSubscriptionInput, RpcRequest, RpcResponse, SortOrder,
    },
    session::{
        SessionError, SessionStore, StartSpec,
        file::{normalize_session_upload_relative_path, write_session_upload},
        logs::{read_persisted_log_page, read_resize_events, render_log_file},
        persist::current_output_offset_by_id,
    },
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
    /// Comma-separated tags; all listed tags must be present.
    pub tag: Option<String>,
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
    let tags: Vec<String> = params
        .tag
        .map(|value| {
            value
                .split(',')
                .map(|part| part.trim().to_string())
                .filter(|tag| !tag.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let page_limit = params.limit.unwrap_or(20).max(1).min(200);
    let offset = params.offset.unwrap_or(0);

    let query = ListQuery {
        search: params.search,
        tags,
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
    #[serde(default)]
    pub tags: Vec<String>,
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
            tags: body.tags.clone(),
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
        tags: body.tags.clone(),
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
                tags: vec![],
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
            let total_bytes = current_output_offset_by_id(&state.config.sessions_dir, &id);
            Json(meta_to_summary(&meta, false, total_bytes)).into_response()
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

#[derive(Debug, Deserialize)]
pub struct SessionNotificationsBody {
    pub enabled: bool,
}

pub async fn set_session_notifications(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<NodeParams>,
    Json(body): Json<SessionNotificationsBody>,
) -> impl IntoResponse {
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::NotifySet {
            id: id.clone(),
            enabled: body.enabled,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::Ack) => Json(serde_json::json!({
                "ok": true,
                "notifications_enabled": body.enabled,
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

    match state
        .store
        .set_notifications_enabled(&id, body.enabled)
        .await
    {
        Ok(()) => {
            info!(
                session_id = %id,
                notifications_enabled = body.enabled,
                "session notification setting updated"
            );
            Json(serde_json::json!({
                "ok": true,
                "notifications_enabled": body.enabled,
            }))
            .into_response()
        }
        Err(SessionError::NotRunning) => {
            warn!(session_id = %id, "notification toggle: session not running");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session not running: {id}") })),
            )
                .into_response()
        }
        Err(SessionError::Evicted) => {
            warn!(session_id = %id, "notification toggle: session evicted");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session evicted from memory: {id}") })),
            )
                .into_response()
        }
        Err(SessionError::Busy) => {
            warn!(session_id = %id, "notification toggle: session busy");
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": format!("session busy: {id}") })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct InputBody {
    pub data: String,
    pub wait_for_change: bool,
}

#[derive(Debug, Serialize)]
pub struct UploadFileResponse {
    pub ok: bool,
    pub path: String,
    pub bytes: usize,
}

fn sanitize_uploaded_filename(file_name: &str) -> Option<String> {
    let file_name = FsPath::new(file_name).file_name()?;
    let trimmed = file_name.to_string_lossy().trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub async fn upload_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<NodeParams>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut requested_path: Option<String> = None;
    let mut uploaded_name: Option<String> = None;
    let mut uploaded_bytes = None;

    loop {
        let next_field = match multipart.next_field().await {
            Ok(field) => field,
            Err(err) => {
                warn!(session_id = %id, %err, "invalid multipart upload payload");
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": err.to_string() })),
                )
                    .into_response();
            }
        };

        let Some(field) = next_field else {
            break;
        };

        match field.name() {
            Some("path") => match field.text().await {
                Ok(value) => requested_path = Some(value.trim().to_string()),
                Err(err) => {
                    warn!(session_id = %id, %err, "invalid upload path field");
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": err.to_string() })),
                    )
                        .into_response();
                }
            },
            Some("file") => {
                uploaded_name = field.file_name().and_then(sanitize_uploaded_filename);
                match field.bytes().await {
                    Ok(bytes) => uploaded_bytes = Some(bytes),
                    Err(err) => {
                        warn!(session_id = %id, %err, "failed to read uploaded file bytes");
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": err.to_string() })),
                        )
                            .into_response();
                    }
                }
            }
            _ => {}
        }
    }

    let Some(bytes) = uploaded_bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "missing file field" })),
        )
            .into_response();
    };

    let raw_target = requested_path
        .filter(|value| !value.is_empty())
        .or(uploaded_name)
        .unwrap_or_default();

    let Some(relative_path) = normalize_session_upload_relative_path(&raw_target) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid upload path" })),
        )
            .into_response();
    };

    if let Some(ref node) = params.node {
        let rpc = RpcRequest::UploadFile {
            id: id.clone(),
            path: pathbuf_to_rpc_path(&relative_path),
            bytes: bytes.to_vec(),
            dedupe: false,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::UploadFile { path, bytes }) => Json(UploadFileResponse {
                ok: true,
                path,
                bytes,
            })
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

    match write_session_upload(&state.config, &id, &raw_target, &bytes, false) {
        Ok(target_path) => Json(UploadFileResponse {
            ok: true,
            path: target_path.to_string_lossy().to_string(),
            bytes: bytes.len(),
        })
        .into_response(),
        Err(err) => {
            error!(session_id = %id, %err, path = %relative_path.display(), "failed to write uploaded file");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err.to_string() })),
            )
                .into_response()
        }
    }
}

fn pathbuf_to_rpc_path(path: &FsPath) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
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
            wait_for_change: body.wait_for_change,
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

    match state
        .store
        .attach_input(&id, &body.data, body.wait_for_change)
        .await
    {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::pathbuf_to_rpc_path;

    #[test]
    fn accepts_nested_relative_upload_path() {
        let path = crate::session::file::normalize_session_upload_relative_path("subdir/file.txt")
            .expect("path should parse");
        assert_eq!(path.to_string_lossy().replace('\\', "/"), "subdir/file.txt");
    }

    #[test]
    fn rejects_parent_segments_in_upload_path() {
        assert!(
            crate::session::file::normalize_session_upload_relative_path("../file.txt").is_none()
        );
    }

    #[test]
    fn rpc_upload_paths_use_forward_slashes() {
        let path = PathBuf::from("subdir").join("file.txt");
        assert_eq!(pathbuf_to_rpc_path(&path), "subdir/file.txt");
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

#[derive(Debug, Serialize)]
struct LogsResponseBody {
    offset: usize,
    chunks: Vec<String>,
    total: usize,
    resizes: Vec<crate::protocol::LogResize>,
}

fn logs_response(body: LogsResponseBody) -> axum::response::Response {
    Json(body).into_response()
}

pub async fn get_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LogsParams>,
) -> impl IntoResponse {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(200).clamp(1, 5000);

    // Proxy to remote node via the paginated logs RPC.
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::LogsPagination {
            id: id.clone(),
            offset: Some(offset),
            limit,
            tail: false,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::LogsPagination {
                offset,
                lines,
                total,
                resizes,
            }) => logs_response(LogsResponseBody {
                offset,
                chunks: lines,
                total,
                resizes,
            }),
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
        Some((lines, mut total)) => {
            if let Ok(live_total) = state.store.read_live_log_chunk_count(&id).await {
                total += live_total;
            }
            let resizes = read_resize_events(&session_dir).unwrap_or_default();
            logs_response(LogsResponseBody {
                offset,
                chunks: lines,
                total,
                resizes,
            })
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

// ---------------------------------------------------------------------------
// Logs tail (raw bytes, same output as CLI)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LogsTailParams {
    pub tail: Option<usize>,
    pub cols: Option<u16>,
    pub node: Option<String>,
}

pub async fn get_logs_tail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<LogsTailParams>,
) -> impl IntoResponse {
    let tail = params.tail.unwrap_or(40).clamp(1, 5000);
    let term_cols = params.cols.unwrap_or(80).max(1);

    // Proxy to remote node via RPC LogsTail.
    if let Some(ref node) = params.node {
        let rpc = RpcRequest::LogsTail {
            id: id.clone(),
            tail,
            keep_color: true,
            term_cols,
            from_file: false,
        };
        return match state.node_registry.proxy_rpc(node, &rpc).await {
            Ok(RpcResponse::LogsTail { output, resizes }) => {
                logs_tail_binary_response(output, &resizes)
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
            debug!(session_id = %id, "session not found for logs tail");
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

    // Try live render first (running sessions).
    if let Ok(output) = state
        .store
        .render_live_logs(&id, tail, true, term_cols)
        .await
    {
        return logs_tail_binary_response(output.0, &output.1);
    }

    // Fall back to persisted log file.
    let log_path = session_dir.join("output.log");
    match render_log_file(&log_path, tail, true, term_cols, None) {
        Ok(output) => {
            let resizes = read_resize_events(&session_dir).unwrap_or_default();
            logs_tail_binary_response(output, &resizes)
        }
        Err(err) => {
            debug!(session_id = %id, %err, "failed to render log file for tail");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("log not available: {id}") })),
            )
                .into_response()
        }
    }
}

fn logs_tail_binary_response(
    output: Vec<u8>,
    resizes: &[crate::protocol::LogResize],
) -> axum::response::Response {
    let resizes_json = serde_json::to_string(resizes).unwrap_or_else(|_| "[]".to_string());
    let mut response = (StatusCode::OK, output).into_response();
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(val) = HeaderValue::from_str(&resizes_json) {
        response.headers_mut().insert("x-log-resizes", val);
    }
    response
}
