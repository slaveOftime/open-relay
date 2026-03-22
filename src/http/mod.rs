mod apps;
pub mod auth;
pub mod nodes;
pub mod sessions;
pub mod sse;
pub mod ws;

#[allow(unused_imports)]
use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
#[cfg(not(debug_assertions))]
use rust_embed::RustEmbed;
use std::{
    io,
    path::{Component, Path},
    sync::Arc,
};
use tokio::sync::broadcast;
use tower_http::{compression::CompressionLayer, cors::CorsLayer};
use tracing::{error, info};

pub use auth::AuthState;

use crate::{
    config::AppConfig,
    db::Database,
    node::NodeRegistry,
    notification::dispatcher::Notifier,
    session::{SessionEvent, SessionStore},
};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<SessionStore>,
    pub config: Arc<AppConfig>,
    pub db: Arc<Database>,
    pub notifier: Arc<Notifier>,
    pub event_tx: broadcast::Sender<SessionEvent>,
    /// None when `--no-auth` was specified; Some when password auth is active.
    pub auth: Option<Arc<AuthState>>,
    /// Registry of connected secondary nodes (only populated on a primary daemon).
    pub node_registry: Arc<NodeRegistry>,
}

// ── Release-only: embed the contents of web/dist into the binary ─────────────
// `build.rs` guarantees that `npm run build` has already run in release mode,
// so the folder is always present when this crate is compiled with --release.
#[cfg(not(debug_assertions))]
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

pub async fn serve(state: AppState) {
    let port = state.config.http_port;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    let wwwroot_dir = match apps::ensure_wwwroot(&state.config) {
        Ok(path) => path,
        Err(err) => {
            error!(
                %err,
                "failed to initialize HTTP wwwroot at {}",
                state.config.wwwroot_dir().display()
            );
            return;
        }
    };

    info!(
        path = %wwwroot_dir.display(),
        "serving custom HTTP static files from wwwroot"
    );

    tokio::spawn(sse::run_session_poller(
        state.store.clone(),
        state.event_tx.clone(),
    ));

    let protected_router = Router::new()
        .route("/api/auth/status", get(auth::status))
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/health", get(sessions::health))
        .route("/api/push/public-key", get(sessions::push_public_key))
        .route(
            "/api/push/subscriptions",
            post(sessions::subscribe_push).delete(sessions::unsubscribe_push),
        )
        .route("/api/sessions", get(sessions::list).post(sessions::create))
        .route("/api/sessions/events", get(sse::events_handler))
        .route("/api/sessions/{id}", get(sessions::get_session))
        .route("/api/sessions/{id}/stop", post(sessions::stop_session))
        .route("/api/sessions/{id}/kill", post(sessions::kill_session))
        .route("/api/sessions/{id}/input", post(sessions::send_input))
        .route("/api/sessions/{id}/logs", get(sessions::get_logs))
        .route("/api/sessions/{id}/attach", get(ws::attach_handler))
        .route("/api/nodes", get(nodes::list_nodes))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let router = Router::new()
        .route("/api/nodes/join", get(nodes::join_handler))
        .route("/api/static/apps", get(apps::list_static_apps))
        .merge(protected_router)
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .fallback(serve_static)
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(%err, "failed to bind HTTP server on port {}", port);
            return;
        }
    };

    info!("HTTP server listening at http://127.0.0.1:{port}");

    if let Err(err) = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        error!(%err, "HTTP server error");
    }
}

async fn serve_static(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }

    let wwwroot_dir = state.config.wwwroot_dir();
    let auth_token = auth::extract_request_token_parts(&headers, uri.query());
    let client_ip = Some(auth::effective_ip(&headers, peer.ip()).to_string());

    match apps::resolve_app_request(&wwwroot_dir, &uri) {
        Ok(Some(apps::AppRequestTarget::LocalFile(candidate))) => {
            if let Some(response) =
                auth::authorize_request(&state, uri.path(), auth_token.clone(), client_ip.clone())
                    .await
            {
                return response;
            }
            match try_read_local_asset(&wwwroot_dir, &candidate).await {
                Ok(Some(bytes)) => return build_bytes_response(&candidate, bytes),
                Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                Err(err) => {
                    error!(%err, path = %candidate, "failed to read static file from wwwroot");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        Ok(Some(apps::AppRequestTarget::Proxy(target_urls))) => {
            if let Some(response) =
                auth::authorize_request(&state, uri.path(), auth_token.clone(), client_ip.clone())
                    .await
            {
                return response;
            }
            return apps::proxy_app_request(&method, &headers, &target_urls).await;
        }
        Ok(None) => {}
        Err(err) => {
            error!(%err, path = %uri.path(), "failed to resolve app request from wwwroot");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let candidates = match static_request_candidates(&uri) {
        Ok(paths) => paths,
        Err(status) => return status.into_response(),
    };

    let local_candidate = match apps::find_existing_local_asset(&wwwroot_dir, &candidates) {
        Ok(candidate) => candidate,
        Err(err) => {
            error!(%err, path = %uri.path(), "failed to inspect static file in wwwroot");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if let Some(candidate) = local_candidate {
        if let Some(response) =
            auth::authorize_request(&state, uri.path(), auth_token, client_ip).await
        {
            return response;
        }
        match try_read_local_asset(&wwwroot_dir, &candidate).await {
            Ok(Some(bytes)) => return build_bytes_response(&candidate, bytes),
            Ok(None) => {}
            Err(err) => {
                error!(%err, path = %candidate, "failed to read static file from wwwroot");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    #[cfg(not(debug_assertions))]
    for candidate in &candidates {
        if let Some(asset) = WebAssets::get(candidate) {
            return build_bytes_response(candidate, asset.data.into_owned());
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

fn static_request_candidates(uri: &Uri) -> Result<Vec<String>, StatusCode> {
    let path = uri.path().trim_start_matches('/');
    let normalized = normalize_static_path(path).ok_or(StatusCode::NOT_FOUND)?;

    let mut candidates = Vec::with_capacity(3);
    if normalized.is_empty() {
        candidates.push("index.html".to_string());
        return Ok(candidates);
    }

    if path.ends_with('/') {
        candidates.push(format!("{normalized}/index.html"));
        return Ok(candidates);
    }

    candidates.push(normalized.clone());
    if Path::new(&normalized).extension().is_none() {
        candidates.push(format!("{normalized}.html"));
    }
    candidates.push(format!("{normalized}/index.html"));
    candidates.dedup();
    Ok(candidates)
}

fn normalize_static_path(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.join("/"))
}

async fn try_read_local_asset(wwwroot: &Path, relative_path: &str) -> io::Result<Option<Vec<u8>>> {
    let full_path = wwwroot.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    match tokio::fs::metadata(&full_path).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    }

    Ok(Some(tokio::fs::read(full_path).await?))
}

fn build_bytes_response(path: &str, bytes: Vec<u8>) -> axum::response::Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            mime.essence_str().to_owned(),
        )],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::static_request_candidates;
    use axum::http::Uri;

    #[test]
    fn static_request_candidates_reject_parent_segments() {
        let uri: Uri = "/../secret.txt".parse().expect("URI should parse");

        let result = static_request_candidates(&uri);

        assert!(result.is_err());
    }
}
