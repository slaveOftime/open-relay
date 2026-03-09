pub mod auth;
pub mod nodes;
pub mod sessions;
pub mod sse;
pub mod ws;

use std::{collections::HashMap, sync::Arc, time::Duration};

#[allow(unused_imports)]
use axum::{
    Router,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use tower_http::cors::CorsLayer;
use tracing::{error, info};

pub use auth::AuthState;

use crate::{
    config::AppConfig,
    db::Database,
    node::NodeRegistry,
    protocol::{ListQuery, SessionSummary},
    session::SessionStore,
};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<SessionStore>>,
    pub config: AppConfig,
    pub db: Arc<Database>,
    pub event_tx: broadcast::Sender<SessionEvent>,
    /// None when `--no-auth` was specified; Some when password auth is active.
    pub auth: Option<Arc<AuthState>>,
    /// Registry of connected secondary nodes (only populated on a primary daemon).
    pub node_registry: Arc<NodeRegistry>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum SessionEvent {
    SessionCreated(SessionSummary),
    SessionUpdated(SessionSummary),
    SessionDeleted {
        id: String,
    },
    SessionNotification {
        kind: String,
        summary: String,
        body: String,
        session_ids: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_rule: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionFingerprint {
    status: String,
    pid: Option<u32>,
    input_needed: bool,
}

impl From<&SessionSummary> for SessionFingerprint {
    fn from(value: &SessionSummary) -> Self {
        Self {
            status: value.status.clone(),
            pid: value.pid,
            input_needed: value.input_needed,
        }
    }
}

// ── Release-only: embed the contents of web/dist into the binary ─────────────
// `build.rs` guarantees that `npm run build` has already run in release mode,
// so the folder is always present when this crate is compiled with --release.
#[cfg(not(debug_assertions))]
#[derive(rust_embed::Embed)]
#[folder = "web/dist"]
struct WebAssets;

pub async fn serve(state: AppState) {
    let port = state.config.http_port;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));

    // Background task: detect session state changes and push only deltas.
    let bg_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        let mut last_sent: HashMap<String, SessionFingerprint> = HashMap::new();
        let mut initialized = false;

        loop {
            interval.tick().await;
            let sessions = {
                let mut store = bg_state.store.lock().await;
                let q = ListQuery {
                    search: None,
                    statuses: vec![],
                    since: None,
                    until: None,
                    limit: 1000,
                    offset: 0,
                    sort: None,
                    order: None,
                };
                store.list_summaries(&q).await.unwrap_or_default()
            };

            let mut seen_ids = std::collections::HashSet::new();

            // First poll establishes a baseline; snapshot already covers current state.
            if !initialized {
                for s in sessions {
                    seen_ids.insert(s.id.clone());
                    last_sent.insert(s.id.clone(), SessionFingerprint::from(&s));
                }
                initialized = true;
                continue;
            }

            for s in sessions {
                let fp = SessionFingerprint::from(&s);
                seen_ids.insert(s.id.clone());

                let changed = match last_sent.get(&s.id) {
                    Some(prev) => prev != &fp,
                    None => true,
                };

                if changed {
                    let _ = bg_state
                        .event_tx
                        .send(SessionEvent::SessionUpdated(s.clone()));
                    last_sent.insert(s.id.clone(), fp);
                }
            }

            // Drop fingerprints for sessions no longer listed.
            last_sent.retain(|id, _| seen_ids.contains(id));
        }
    });

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
        // ── Node federation ────────────────────────────────────────────────
        .route("/api/nodes", get(nodes::list_nodes))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let base_router = Router::new()
        .route("/api/nodes/join", get(nodes::join_handler))
        .merge(protected_router)
        .layer(CorsLayer::permissive())
        .with_state(state);

    #[cfg(not(debug_assertions))]
    let router = base_router.fallback(serve_embedded);
    #[cfg(debug_assertions)]
    let router = base_router;

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
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
    // _vite_handle is dropped here → kill_on_drop kills `npm run dev`
}

/// Axum fallback handler that serves files embedded via `rust-embed`.
///
/// Resolution order:
///   1. Exact path match inside `web/dist`  (e.g. `/assets/main.js`)
///   2. Path + `.html`                       (e.g. `/session` → `session.html`)
///   3. `index.html`                         (SPA catch-all)
#[cfg(not(debug_assertions))]
async fn serve_embedded(uri: axum::http::Uri) -> Response {
    let req_path = uri.path().trim_start_matches('/');
    let req_path = if req_path.is_empty() {
        "index.html"
    } else {
        req_path
    };

    if let Some(asset) = WebAssets::get(req_path) {
        return build_embedded_response(req_path, asset);
    }

    let with_html = format!("{req_path}.html");
    if let Some(asset) = WebAssets::get(&with_html) {
        return build_embedded_response(&with_html, asset);
    }

    // SPA fallback – send index.html and let the client-side router handle it.
    if let Some(asset) = WebAssets::get("index.html") {
        return build_embedded_response("index.html", asset);
    }

    axum::http::StatusCode::NOT_FOUND.into_response()
}

#[cfg(not(debug_assertions))]
fn build_embedded_response(
    path: &str,
    asset: rust_embed::EmbeddedFile,
) -> axum::response::Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [(
            axum::http::header::CONTENT_TYPE,
            mime.essence_str().to_owned(),
        )],
        asset.data.into_owned(),
    )
        .into_response()
}
