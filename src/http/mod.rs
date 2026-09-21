mod apps;
mod attach_source;
pub mod auth;
pub mod nodes;
mod reverse_proxy;
pub mod sessions;
pub mod sse;
pub mod ws;

use axum::{
    Router,
    extract::ws::rejection::WebSocketUpgradeRejection,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State, WebSocketUpgrade},
    http::{HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
};
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
    config::LiveConfig,
    db::Database,
    node::NodeRegistry,
    notification::dispatcher::SharedNotifier,
    session::{SessionEvent, SessionStore},
    utils::format_http_url,
};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<SessionStore>,
    /// Live, hot-reloadable configuration view: read via `config.get()` so
    /// edits to config.json apply without a daemon restart.
    pub config: LiveConfig,
    pub db: Arc<Database>,
    pub notifier: SharedNotifier,
    pub event_tx: broadcast::Sender<SessionEvent>,
    /// None when `--no-auth` was specified; Some when password auth is active.
    pub auth: Option<Arc<AuthState>>,
    /// Registry of connected secondary nodes (only populated on a primary daemon).
    pub node_registry: Arc<NodeRegistry>,
    /// SSH host key pair for node join authentication (primary side).
    /// The public key is served at GET /api/nodes/host-key.
    pub ssh_host_key: SshHostKey,
}

/// SSH host key pair: the primary proves ownership of this Ed25519 key on
/// every node-join handshake (host-key challenge), so secondaries can
/// authenticate the primary and pin its key via known_hosts.
#[derive(Clone)]
pub struct SshHostKey {
    /// Canonical public key line: "ssh-ed25519 <base64(raw32)>".
    public_key: String,
    /// 32-byte Ed25519 private key seed (empty when disabled).
    seed: Vec<u8>,
}

impl SshHostKey {
    /// A host key that cannot sign anything — used when HTTP is disabled or
    /// key generation failed; SSH-key joins are then rejected.
    pub fn disabled() -> Self {
        SshHostKey {
            public_key: String::new(),
            seed: Vec::new(),
        }
    }

    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// The host signing key, if this host key is usable.
    pub fn signing_key(&self) -> Option<ed25519_dalek::SigningKey> {
        let seed = <[u8; 32]>::try_from(self.seed.as_slice()).ok()?;
        Some(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    /// Generate a new Ed25519 host key or load an existing one.
    /// The seed is stored as `ssh_host_key` (0600) and the canonical public
    /// key line as `ssh_host_key.pub`.
    pub async fn create_or_load(state_dir: &std::path::Path) -> std::io::Result<Self> {
        let key_path = state_dir.join("ssh_host_key");
        let pub_path = state_dir.join("ssh_host_key.pub");

        let seed = if key_path.exists() {
            let seed = tokio::fs::read(&key_path).await?;
            if seed.len() != 32 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "host key {} has unexpected length {}",
                        key_path.display(),
                        seed.len()
                    ),
                ));
            }
            seed
        } else {
            // Generate new Ed25519 key pair (raw 32-byte seed).
            let mut seed_bytes = [0u8; 32];
            rand::Rng::fill_bytes(&mut rand::rng(), &mut seed_bytes);
            let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed_bytes);
            let seed = signing_key.to_bytes().to_vec();

            // Write private seed (user-only) and public key line.
            tokio::fs::write(&key_path, &seed).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
            }
            seed
        };

        // Always derive the public key from the seed so the in-memory value
        // and the .pub file stay consistent with the private key.
        let arr = <[u8; 32]>::try_from(seed.as_slice())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad seed"))?;
        let public_key =
            crate::sshauth::public_key_line(&ed25519_dalek::SigningKey::from_bytes(&arr));
        tokio::fs::write(&pub_path, format!("{public_key}\n")).await?;

        Ok(SshHostKey { public_key, seed })
    }
}

// ── Release-only: embed the contents of web/dist into the binary ─────────────
// `build.rs` guarantees that `npm run build` has already run in release mode,
// so the folder is always present when this crate is compiled with --release.
#[derive(RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

pub async fn serve(state: AppState) {
    let bind = state.config.get().http_bind.clone();
    let port = state.config.get().http_port;
    let ip = match bind.parse::<std::net::IpAddr>() {
        Ok(ip) => ip,
        Err(err) => {
            error!(%err, bind = %bind, "failed to parse HTTP bind address");
            return;
        }
    };
    let addr = std::net::SocketAddr::new(ip, port);

    let wwwroot_dir = match apps::ensure_wwwroot(&state.config.get()) {
        Ok(path) => path,
        Err(err) => {
            error!(
                %err,
                "failed to initialize HTTP wwwroot at {}",
                state.config.get().wwwroot_dir().display()
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
        .route(
            "/api/sessions/{id}/metadata",
            post(sessions::set_session_metadata),
        )
        .route(
            "/api/sessions/{id}/notifications",
            post(sessions::set_session_notifications),
        )
        .route("/api/sessions/{id}/stop", post(sessions::stop_session))
        .route("/api/sessions/{id}/kill", post(sessions::kill_session))
        .route("/api/sessions/{id}/input", post(sessions::send_input))
        .route(
            "/api/sessions/{id}/upload",
            post(sessions::upload_file).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/sessions/{id}/logs", get(sessions::get_logs))
        .route("/api/sessions/{id}/logs/tail", get(sessions::get_logs_tail))
        .route("/api/sessions/{id}/attach", get(ws::attach_handler))
        .route("/api/nodes", get(nodes::list_nodes))
        .route("/api/nodes/host-key", get(nodes::get_host_key))
        .route("/api/metrics", get(metrics_endpoint))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        // Outermost layer on the API: measures full handled time including
        // auth, per matched route (PERFORMANCE.md data source).
        .layer(axum::middleware::from_fn(metrics_middleware));

    let cors = CorsLayer::new()
        .allow_origin([
            format!("http://127.0.0.1:{port}")
                .parse::<HeaderValue>()
                .unwrap(),
            format!("http://localhost:{port}")
                .parse::<HeaderValue>()
                .unwrap(),
        ])
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ACCEPT,
        ])
        .allow_credentials(true);

    let router = Router::new()
        .route("/api/nodes/join", get(nodes::join_handler))
        .route("/api/static/apps", get(apps::list_static_apps))
        .merge(protected_router)
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(axum::middleware::from_fn(security_headers))
        .fallback(serve_static_or_proxy)
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(err) => {
            error!(%err, "failed to bind HTTP server on port {}", port);
            return;
        }
    };

    info!("HTTP server listening at {}", format_http_url(&bind, port));

    if let Err(err) = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        error!(%err, "HTTP server error");
    }
}

async fn serve_static_or_proxy(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    ws_upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let uri = parts.uri.clone();
    let headers = parts.headers.clone();
    let method = parts.method.clone();
    let wwwroot_dir = state.config.get().wwwroot_dir();
    let auth_token = auth::extract_request_token_parts(&headers, uri.query());
    let bearer = auth::extract_bearer_token(&headers);
    let client_ip = Some(auth::effective_ip(&headers, peer.ip()).to_string());

    match apps::resolve_app_request(&wwwroot_dir, &uri) {
        Ok(Some(apps::AppRequestTarget::LocalFile(candidate))) => {
            if let Some(response) = auth::authorize_request(
                &state,
                &method,
                uri.path(),
                auth_token.clone(),
                bearer.clone(),
                client_ip.clone(),
            )
            .await
            {
                return response;
            }
            match try_read_static_file(&candidate).await {
                Ok(Some(bytes)) => return build_bytes_response(&candidate, bytes),
                Ok(None) => return StatusCode::NOT_FOUND.into_response(),
                Err(err) => {
                    error!(%err, path = %candidate.display(), "failed to read app static file");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            }
        }
        Ok(Some(apps::AppRequestTarget::Proxy(target_urls))) => {
            if let Some(response) = auth::authorize_request(
                &state,
                &method,
                uri.path(),
                auth_token.clone(),
                bearer.clone(),
                client_ip.clone(),
            )
            .await
            {
                return response;
            }
            return reverse_proxy::proxy(
                Request::from_parts(parts, body),
                ws_upgrade.ok(),
                &target_urls,
            )
            .await;
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
            auth::authorize_request(&state, &method, uri.path(), auth_token, bearer, client_ip)
                .await
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

    for candidate in &candidates {
        if let Some(asset) = WebAssets::get(candidate) {
            return build_bytes_response(candidate, asset.data.into_owned());
        }
    }

    let default_asset_name = "index.html";
    if let Some(asset) = WebAssets::get(default_asset_name) {
        return build_bytes_response(default_asset_name, asset.data.into_owned());
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
    try_read_static_file(&full_path).await
}

async fn try_read_static_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let full_path = path;
    match tokio::fs::metadata(&full_path).await {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    }

    Ok(Some(tokio::fs::read(full_path).await?))
}

fn build_bytes_response(path: impl AsRef<Path>, bytes: Vec<u8>) -> axum::response::Response {
    let path = path.as_ref();
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

/// `GET /api/metrics` — Prometheus text exposition of the always-on
/// in-daemon metrics registry (see `src/metrics.rs` and PERFORMANCE.md).
/// Sits behind the normal auth layer on purpose: timings and volumes are
/// low-risk but not something to leak to unauthenticated visitors.
async fn metrics_endpoint() -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        crate::metrics::render_prometheus(),
    )
        .into_response()
}

/// The closed set of route templates we label with; anything else lands
/// in `"other"` so a stray wildcard or unmatched path cannot blow up
/// label cardinality.
const METRIC_ROUTES: &[&str] = &[
    "/api/health",
    "/api/metrics",
    "/api/auth/status",
    "/api/auth/login",
    "/api/auth/logout",
    "/api/push/public-key",
    "/api/push/subscribe",
    "/api/sessions",
    "/api/sessions/{id}",
    "/api/sessions/{id}/metadata",
    "/api/sessions/{id}/notifications",
    "/api/sessions/{id}/stop",
    "/api/sessions/{id}/kill",
    "/api/sessions/{id}/input",
    "/api/sessions/{id}/upload",
    "/api/sessions/{id}/logs",
    "/api/sessions/{id}/logs/tail",
    "/api/sessions/{id}/attach",
    "/api/nodes",
    "/api/nodes/host-key",
    "/api/static/apps",
];

/// Per-route request duration histogram. Long-lived responses (SSE) are
/// skipped: their lifetime is a connection duration, not handled work,
/// and mixing them into latency histograms destroys the signal.
async fn metrics_middleware(request: Request, next: axum::middleware::Next) -> Response {
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .and_then(|matched| {
            METRIC_ROUTES
                .iter()
                .find(|r| **r == matched.as_str())
                .copied()
        })
        .unwrap_or("other");
    let start = std::time::Instant::now();
    let response = next.run(request).await;
    let is_stream = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.starts_with("text/event-stream") || v.starts_with("application/x-ndjson")
        });
    if !is_stream {
        crate::metrics::observe("http_request", Some(route), start.elapsed());
    }
    response
}

/// Middleware that injects standard security response headers on every reply.
async fn security_headers(request: Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; img-src 'self' data: blob:; font-src 'self' data:; worker-src 'self' blob:",
        ),
    );
    response
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
