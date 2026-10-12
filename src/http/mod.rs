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
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Redirect, Response},
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

pub use sse::{EncodedSessionEvent, run_event_encoder};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<SessionStore>,
    /// Live, hot-reloadable configuration view: read via `config.get()` so
    /// edits to config.json apply without a daemon restart.
    pub config: LiveConfig,
    pub db: Arc<Database>,
    pub notifier: SharedNotifier,
    pub event_tx: broadcast::Sender<SessionEvent>,
    /// Pre-encoded SSE frame broadcast. Producers run
    /// [`run_event_encoder`] once at startup; each connected SSE handler
    /// subscribes to this and forwards bytes verbatim. Bounded; on lag the
    /// encoder publishes a `resync_required` frame and continues.
    pub event_bytes_tx: broadcast::Sender<EncodedSessionEvent>,
    /// None when `--no-auth` was specified; Some when password auth is active.
    pub auth: Option<Arc<AuthState>>,
    /// Registry of connected secondary nodes (only populated on a primary daemon).
    pub node_registry: Arc<NodeRegistry>,
    /// Persisted Ed25519 node identity. Disabled identities refuse federation;
    /// HTTP `--no-auth` never bypasses identity verification or encryption.
    pub node_identity: NodeIdentity,
}

/// Node identity key pair (Ed25519): every daemon auto-generates one of
/// these on first start, persisted as `<state>/identity_key` (the seed,
/// mode 0600) + `<state>/identity_key.pub` (the canonical public line).
///
/// The same keypair serves both federation roles:
///
/// - On a **primary** it derives the per-direction channel keys
///   (`PRIV_X * PEER_X25519_PUB` via the Ed25519→X25519 birational
///   map and HKDF-Expand) for the post-handshake AES-256-GCM phase.
/// - On a **secondary** it signs the full nonce-bound handshake transcript
///   so the primary can authenticate the secondary against the key registered via
///   `oly node accept --name <n> -k <...>`, and it also participates
///   in the same ECDH process to derive the channel keys.
///
/// Auth is end-to-end exact-pin (operator types the primary's pubkey on
/// the secondary via `--id-pub-key`); no in-band trust-root discovery,
/// TOFU, or plaintext fallback exists. Static-static DH does not provide
/// forward secrecy.
#[derive(Clone)]
pub struct NodeIdentity {
    /// 32-byte Ed25519 private key seed (empty when disabled).
    seed: Vec<u8>,
}

impl NodeIdentity {
    /// Used when identity loading failed; federation is then rejected.
    pub fn disabled() -> Self {
        NodeIdentity { seed: Vec::new() }
    }

    /// The signing key, if this identity is usable.
    pub fn signing_key(&self) -> Option<ed25519_dalek::SigningKey> {
        let seed = <[u8; 32]>::try_from(self.seed.as_slice()).ok()?;
        Some(ed25519_dalek::SigningKey::from_bytes(&seed))
    }

    /// Generate a new Ed25519 identity key or load an existing one under
    /// `state_dir`. The seed is persisted as `identity_key` (0600) and the
    /// canonical public key line as `identity_key.pub`.
    pub async fn create_or_load(state_dir: &std::path::Path) -> std::io::Result<Self> {
        let state_dir = state_dir.to_path_buf();
        let (signing_key, _) = tokio::task::spawn_blocking(move || {
            crate::federation_crypto::create_or_load_identity(&state_dir)
        })
        .await
        .map_err(std::io::Error::other)??;
        Ok(NodeIdentity {
            seed: signing_key.to_bytes().to_vec(),
        })
    }

    /// Read the on-disk public key line for the daemon under `state_dir`,
    /// if present. Used by `oly daemon status` to surface the identity
    /// without having to talk to the running daemon.
    pub fn read_published_pubkey(state_dir: &std::path::Path) -> std::io::Result<Option<String>> {
        crate::federation_crypto::read_published_pubkey(state_dir)
    }
}

// ── Release-only: embed the contents of web/dist into the binary ─────────────
// `build.rs` guarantees that `npm run build` has already run in release mode,
// so the folder is always present when this crate is compiled with --release.
// Debug/test builds (`cargo clippy --all-targets`, `cargo test`) never build
// the web bundle — the Vite dev server serves those files at runtime — so the
// folder is legitimately absent there. `allow_missing` keeps the derive from
// hard-erroring on the missing folder; `WebAssets::get` then returns `None`,
// which every caller already treats as "fall through to the next candidate".
#[derive(RustEmbed)]
#[folder = "web/dist"]
#[allow_missing = true]
struct WebAssets;

pub async fn serve(state: AppState) {
    let bind = state.config.get().http.bind.clone();
    let port = state.config.get().http.port;
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

    // Daemon lifecycle owns the activity sampler, including on HTTP-disabled nodes.

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
        .route(
            "/api/sessions/{id}",
            get(sessions::get_session).delete(sessions::remove_session),
        )
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
        // Keep the login shell accessible even if wwwroot/login.html exists.
        .route("/login", get(serve_login_page))
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

    match apps::resolve_app_request(wwwroot_dir.clone(), uri.clone()).await {
        Ok(Some(apps::AppRequestTarget::LocalFile(candidate))) => {
            if let Some(response) = auth::authorize_request(
                &state,
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
                uri.path(),
                auth_token.clone(),
                bearer.clone(),
                client_ip.clone(),
            )
            .await
            {
                return redirect_unauthorized_proxy_navigation(&method, &headers, &uri, response);
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

    let local_candidate =
        match apps::find_existing_local_asset(wwwroot_dir.clone(), candidates.clone()).await {
            Ok(candidate) => candidate,
            Err(err) => {
                error!(%err, path = %uri.path(), "failed to inspect static file in wwwroot");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
    if let Some(candidate) = local_candidate {
        if let Some(response) =
            auth::authorize_request(&state, uri.path(), auth_token, bearer, client_ip).await
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

    // A request that names a concrete asset file is a subresource, not a
    // client-side route. Answering it with the SPA shell hides build mistakes:
    // the daemon once served index.html (200 + text/html) for every missing
    // font, so the browser refused it as a font and the terminal silently fell
    // back to a system monospace. A 404 shows up in devtools instead.
    if names_asset_file(uri.path()) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let default_asset_name = "index.html";
    if let Some(asset) = WebAssets::get(default_asset_name) {
        return build_bytes_response(default_asset_name, asset.data.into_owned());
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn serve_login_page() -> Response {
    match WebAssets::get("index.html") {
        Some(asset) => build_bytes_response("index.html", asset.data.into_owned()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Only document navigations should enter the password flow. API calls,
/// subresource requests, and WebSocket handshakes still receive their 401.
fn redirect_unauthorized_proxy_navigation(
    method: &Method,
    headers: &HeaderMap,
    uri: &Uri,
    response: Response,
) -> Response {
    if response.status() != StatusCode::UNAUTHORIZED
        || (method != Method::GET && method != Method::HEAD)
        || !headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| {
                accept
                    .split(',')
                    .any(|part| part.trim().starts_with("text/html"))
            })
        || headers
            .get("sec-fetch-dest")
            .is_some_and(|dest| dest != "document")
    {
        return response;
    }

    // Preserve deep links and ordinary query parameters, but never reflect a
    // credential from the URL into the login page's address or browser history.
    let mut next = uri.path().to_owned();
    if let Some(query) = apps::filtered_proxy_query(uri.query()) {
        next.push('?');
        next.push_str(&query);
    }
    let mut login = reqwest::Url::parse("http://localhost/login").expect("static login URL");
    login.query_pairs_mut().append_pair("next", &next);
    Redirect::to(&format!("/login?{}", login.query().expect("next query"))).into_response()
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

/// Extensions that can only belong to a subresource. Deliberately a whitelist
/// rather than "any path with a dot", so client-side routes keep falling back
/// to the SPA shell even if an id happens to contain one.
const ASSET_EXTENSIONS: &[&str] = &[
    "css",
    "js",
    "mjs",
    "map",
    "ttf",
    "otf",
    "woff",
    "woff2",
    "eot",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "svg",
    "ico",
    "webp",
    "avif",
    "json",
    "webmanifest",
    "txt",
    "wasm",
];

/// True when the request path names a concrete file the build should have
/// emitted, rather than a client-side route.
fn names_asset_file(path: &str) -> bool {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() || trimmed.ends_with('/') {
        return false;
    }
    let Some(extension) = Path::new(trimmed).extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    ASSET_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
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
    "/api/push/subscriptions",
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
    "/api/static/apps",
];

/// Per-route request duration histogram. Long-lived responses (SSE) are
/// skipped: their lifetime is a connection duration, not handled work,
/// and mixing them into latency histograms destroys the signal.
async fn metrics_middleware(request: Request, next: axum::middleware::Next) -> Response {
    let route = resolve_route_label(
        request
            .extensions()
            .get::<axum::extract::MatchedPath>()
            .map(|m| m.as_str()),
    );
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

/// Resolve an HTTP `MatchedPath` template to the closed histogram label.
/// Anything not in `METRIC_ROUTES` collapses to `"other"` to bound label
/// cardinality. The route-label normaliser was made a free function
/// so a debug-only assertion
/// can confirm every METRIC_ROUTES entry round-trips and that the drift
/// the original `/api/push/subscribe` typo introduced cannot recur.
fn resolve_route_label(matched: Option<&str>) -> &'static str {
    let Some(matched) = matched else {
        return "other";
    };
    METRIC_ROUTES
        .iter()
        .find(|r| **r == matched)
        .copied()
        .unwrap_or("other")
}

#[cfg(test)]
mod tests {
    use super::{
        METRIC_ROUTES, names_asset_file, redirect_unauthorized_proxy_navigation,
        resolve_route_label, static_request_candidates,
    };
    use axum::{
        http::{HeaderMap, Method, StatusCode, Uri, header},
        response::IntoResponse,
    };

    /// Regression guard: a previous version of `METRIC_ROUTES` had
    /// `/api/push/subscribe` while the router served `/api/push/subscriptions`,
    /// so every push subscription silently fell into the `"other"` bucket.
    /// This test ensures the captured closure is reflected verbatim in the
    /// const list.
    #[test]
    fn every_metric_route_round_trips_to_itself() {
        for &entry in METRIC_ROUTES {
            assert_eq!(
                resolve_route_label(Some(entry)),
                entry,
                "METRIC_ROUTES entry `{entry}` must round-trip; otherwise METRIC_ROUTES is \
                 drifting away from the actual router's MatchedPath strings, which lands \
                 every request with that template in the `other` bucket."
            );
        }
    }

    /// Belt-and-suspenders: an unknown path falls into `"other"`, so a
    /// wild-card route can never blow up label cardinality.
    #[test]
    fn unknown_paths_collapse_to_other() {
        assert_eq!(resolve_route_label(Some("/api/this-is-fake")), "other");
        assert_eq!(resolve_route_label(None), "other");
    }

    /// A missing subresource must not be answered with the SPA shell. The
    /// terminal's fonts were once requested at a path the build never emitted;
    /// the daemon replied 200 + text/html, the browser refused it as a font, and
    /// the terminal quietly fell back to a system monospace (block-character art
    /// rendered with gaps on Android).
    #[test]
    fn only_asset_files_are_reported_as_assets() {
        for asset in [
            "/fonts/JetBrainsMono-Regular.ttf",
            "/fonts/OpenRelayTerminalSymbols.woff2",
            "/assets/index-1lAjgh0k.js",
            "/assets/index-DNUrvibf.css",
            "/manifest.webmanifest",
            "/pwa-192x192.png",
            "/FONTS/JetBrainsMono-Regular.TTF",
        ] {
            assert!(
                names_asset_file(asset),
                "{asset} should read as an asset file"
            );
        }

        // Client-side routes keep the SPA fallback, even with dots in them.
        for route in [
            "/",
            "/sessions",
            "/sessions/0fdc3fe",
            "/login",
            "/sessions/1.2.3/attach",
            "/assets/",
            "",
        ] {
            assert!(
                !names_asset_file(route),
                "{route} should stay a client-side route"
            );
        }
    }

    #[test]
    fn unauthorized_proxy_document_redirects_to_login_and_preserves_safe_deep_link() {
        let uri: Uri = "/apps/reports/details?tab=logs&token=secret&page=2"
            .parse()
            .expect("URI should parse");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            "text/html,application/xhtml+xml".parse().unwrap(),
        );
        headers.insert("sec-fetch-dest", "document".parse().unwrap());

        let response = redirect_unauthorized_proxy_navigation(
            &Method::GET,
            &headers,
            &uri,
            StatusCode::UNAUTHORIZED.into_response(),
        );
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response.headers()[header::LOCATION].to_str().unwrap();
        let login = reqwest::Url::parse(&format!("http://localhost{location}")).unwrap();
        assert_eq!(login.path(), "/login");
        assert_eq!(
            login
                .query_pairs()
                .find(|(key, _)| key == "next")
                .unwrap()
                .1,
            "/apps/reports/details?tab=logs&page=2"
        );
        assert!(!location.contains("secret"));
    }

    #[test]
    fn proxy_unauthorized_non_navigation_requests_keep_their_status() {
        let uri: Uri = "/apps/reports/data".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        for (method, dest, status) in [
            (Method::POST, "document", StatusCode::UNAUTHORIZED),
            (Method::GET, "script", StatusCode::UNAUTHORIZED),
            (Method::GET, "document", StatusCode::FORBIDDEN),
        ] {
            headers.insert("sec-fetch-dest", dest.parse().unwrap());
            let response = redirect_unauthorized_proxy_navigation(
                &method,
                &headers,
                &uri,
                status.into_response(),
            );
            assert_eq!(response.status(), status);
        }
        headers.remove(header::ACCEPT);
        headers.insert("sec-fetch-dest", "document".parse().unwrap());
        assert_eq!(
            redirect_unauthorized_proxy_navigation(
                &Method::GET,
                &headers,
                &uri,
                StatusCode::UNAUTHORIZED.into_response(),
            )
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    #[test]
    fn static_request_candidates_reject_parent_segments() {
        let uri: Uri = "/../secret.txt".parse().expect("URI should parse");

        let result = static_request_candidates(&uri);

        assert!(result.is_err());
    }
}
