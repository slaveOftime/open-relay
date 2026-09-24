//! App discovery, manifest parsing, and request resolution for the
//! `/apps/<slug>/*` HTTP surface. PLAN2 S1.2 split.
use axum::{
    Json,
    extract::State,
    http::Uri,
    response::{IntoResponse, Response},
};
use reqwest::Url;
use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
};
use tracing::{error, info};

use crate::config::AppConfig;

use super::AppState;

mod html;
mod manifest;
mod proxy_targets;
mod resolve;

// Production code uses unqualified call-sites because the moves from
// `apps.rs` were carried out verbatim (PLAN2 S1.2). Items tests need by
// short name are re-exported below under `#[cfg(test)]`.
#[cfg(test)]
use html::extract_meta_content;
#[cfg(test)]
pub(super) use html::resolve_app_asset_href;
use html::{
    detect_app_icon_href, extract_app_description, extract_app_icon_href, extract_app_kind,
    extract_title,
};
#[cfg(test)]
use manifest::APP_MANIFEST_FILE;
use manifest::{build_manifest_app_definition, load_app_manifest};
use proxy_targets::build_proxy_target_urls;
use resolve::{
    app_local_request_candidates, find_existing_app_local_asset, find_existing_redirect_asset,
    local_asset_exists, split_app_request_path,
};

// `include_str!` paths resolve relative to this file, not the source-root,
// so the embedded asset is referenced as `../apps-index.html` — the same
// blob as before the S1.2 split, just with a deeper file location.
const DEFAULT_WWWROOT_INDEX: &str = include_str!("../apps-index.html");

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum StaticAppKind {
    SingleHtml,
    Spa,
}

impl StaticAppKind {
    pub(super) fn from_meta_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "spa" => Some(Self::Spa),
            "single_html" | "single-html" | "html" | "singlefile" | "single-file" => {
                Some(Self::SingleHtml)
            }
            _ => None,
        }
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct StaticApp {
    href: String,
    title: String,
    description: Option<String>,
    icon_href: Option<String>,
    #[serde(rename = "type")]
    app_type: StaticAppKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AppEntry {
    Local {
        entry_path: String,
        entry_source_path: PathBuf,
        redirect_files: Vec<PathBuf>,
    },
    Proxy {
        entry_url: Url,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AppDefinition {
    pub(super) static_app: StaticApp,
    pub(super) entry: AppEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AppRequestTarget {
    LocalFile(PathBuf),
    Proxy(Vec<Url>),
}

pub(super) fn ensure_wwwroot(config: &AppConfig) -> io::Result<PathBuf> {
    let wwwroot_dir = config.wwwroot_dir();
    let apps_dir = wwwroot_dir.join("apps");
    std::fs::create_dir_all(&wwwroot_dir)?;
    std::fs::create_dir_all(&apps_dir)?;
    let index_path = apps_dir.join("index.html");
    if !index_path.exists() {
        std::fs::write(&index_path, DEFAULT_WWWROOT_INDEX)?;
        info!(path = %index_path.display(), "created default wwwroot index.html");
    }
    Ok(wwwroot_dir)
}

pub(super) async fn list_static_apps(State(state): State<AppState>) -> Response {
    let wwwroot = state.config.get().wwwroot_dir();
    match discover_static_apps_async(wwwroot).await {
        Ok(apps) => Json(apps).into_response(),
        Err(err) => {
            error!(%err, "failed to enumerate apps in {}", state.config.get().wwwroot_dir().display());
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Async wrapper: resolves an `/apps/<slug>/*` request off the tokio
/// worker pool. The expanded pipeline does ~3–4 `std::fs::metadata`
/// syscalls plus the manifest/index.html reads in
/// `load_app_definition`; all are sync. Routing the hot /app-static
/// path through `spawn_blocking` keeps the 4-worker runtime free
/// to drive the WS attach pumps while a slow disk is being pawed at.
pub(super) async fn resolve_app_request(
    wwwroot: PathBuf,
    uri: Uri,
) -> io::Result<Option<AppRequestTarget>> {
    tokio::task::spawn_blocking(move || resolve_app_request_blocking(&wwwroot, &uri))
        .await
        .map_err(|join_err| {
            io::Error::other(format!("apps resolution worker join failed: {join_err}"))
        })?
}

fn resolve_app_request_blocking(wwwroot: &Path, uri: &Uri) -> io::Result<Option<AppRequestTarget>> {
    let Some((slug, request_tail, trailing_slash)) = split_app_request_path(uri.path()) else {
        return Ok(None);
    };

    let app_dir = wwwroot.join("apps").join(&slug);
    let Some(definition) = load_app_definition(&app_dir, &slug)? else {
        return Ok(None);
    };

    match definition.entry {
        AppEntry::Local {
            entry_path,
            redirect_files,
            ..
        } => {
            let request_candidates =
                app_local_request_candidates(&entry_path, &request_tail, trailing_slash);
            if let Some(path) = find_existing_app_local_asset(&app_dir, &request_candidates)? {
                return Ok(Some(AppRequestTarget::LocalFile(path)));
            }
            for redirect_path in &redirect_files {
                if let Some(path) =
                    find_existing_redirect_asset(redirect_path, &request_candidates)?
                {
                    return Ok(Some(AppRequestTarget::LocalFile(path)));
                }
            }
            Ok(None)
        }
        AppEntry::Proxy { entry_url } => Ok(Some(AppRequestTarget::Proxy(
            build_proxy_target_urls(&entry_url, &request_tail, uri.query())?,
        ))),
    }
}

/// Async wrapper: looks up an existing static asset under
/// `wwwroot` on the blocking pool. The sync body makes up to three
/// `std::fs::metadata` calls per candidate via `local_asset_exists`,
/// which is a relevant cost on cold caches.
pub(super) async fn find_existing_local_asset(
    wwwroot: PathBuf,
    candidates: Vec<String>,
) -> io::Result<Option<String>> {
    tokio::task::spawn_blocking(move || find_existing_local_asset_blocking(&wwwroot, &candidates))
        .await
        .map_err(|join_err| {
            io::Error::other(format!(
                "static asset lookup worker join failed: {join_err}"
            ))
        })?
}

fn find_existing_local_asset_blocking(
    wwwroot: &Path,
    candidates: &[String],
) -> io::Result<Option<String>> {
    for candidate in candidates {
        if local_asset_exists(wwwroot, candidate)? {
            return Ok(Some(candidate.clone()));
        }
    }
    Ok(None)
}

/// Async wrapper: discovers apps under `wwwroot` on the blocking pool.
///
/// `discover_static_apps` calls `load_app_definition` per entry, which
/// touches `index.html` and `oly.app.json` synchronously. With seven
/// shipped apps the directory walk + manifest reads are ~50–100µs on a
/// warm disk; on NFS or under pointer-chasing contention that can
/// bump the live attach keystroke→screen latency enough to be visible.
/// Centralising the off-thread hop here keeps callers from each
/// spawning their own `spawn_blocking` closure.
async fn discover_static_apps_async(wwwroot: PathBuf) -> io::Result<Vec<StaticApp>> {
    tokio::task::spawn_blocking(move || discover_static_apps(&wwwroot))
        .await
        .map_err(|join_err| {
            io::Error::other(format!("apps discovery worker join failed: {join_err}"))
        })?
}

fn discover_static_apps(wwwroot: &Path) -> io::Result<Vec<StaticApp>> {
    let apps_dir = wwwroot.join("apps");
    if !apps_dir.exists() {
        return Ok(Vec::new());
    }

    let mut apps = Vec::new();

    for entry in std::fs::read_dir(&apps_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') || !file_type.is_dir() {
            continue;
        }

        let slug = name.to_string_lossy();
        if let Some(definition) = load_app_definition(&entry.path(), slug.as_ref())? {
            apps.push(definition.static_app);
        }
    }

    apps.sort_by(|left, right| left.href.cmp(&right.href));
    Ok(apps)
}

fn load_app_definition(app_dir: &Path, slug: &str) -> io::Result<Option<AppDefinition>> {
    let app_href = format!("/apps/{slug}/");
    if let Some(manifest) = load_app_manifest(app_dir)? {
        return Ok(Some(build_manifest_app_definition(
            app_dir, &app_href, slug, manifest,
        )?));
    }

    let index_path = app_dir.join("index.html");
    if !index_path.is_file() {
        return Ok(None);
    }

    Ok(Some(AppDefinition {
        static_app: build_static_app(&index_path, &app_href, slug)?,
        entry: AppEntry::Local {
            entry_path: "index.html".into(),
            entry_source_path: index_path,
            redirect_files: Vec::new(),
        },
    }))
}

fn build_static_app(index_path: &Path, href: &str, fallback_title: &str) -> io::Result<StaticApp> {
    let html = std::fs::read_to_string(index_path)?;
    let description = extract_app_description(&html);
    let icon_href = extract_app_icon_href(&html, href)
        .or_else(|| detect_app_icon_href(index_path.parent(), href));
    let app_type = extract_app_kind(&html);

    Ok(StaticApp {
        href: href.to_string(),
        title: extract_title(&html).unwrap_or_else(|| fallback_title.to_string()),
        description,
        icon_href,
        app_type,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        APP_MANIFEST_FILE, AppRequestTarget, DEFAULT_WWWROOT_INDEX, StaticApp, StaticAppKind,
        app_local_request_candidates, discover_static_apps, ensure_wwwroot,
        extract_app_description, extract_app_icon_href, extract_app_kind, extract_meta_content,
        extract_title, resolve_app_asset_href, resolve_app_request,
    };

    /// Test helper: drives the sync `discover_static_apps` through a
    /// blocking-pool worker so the unit tests exercise the same path
    /// the production handler does (PLAN2 §P1.3).
    async fn discover_static_apps_async_for_tests(
        wwwroot: PathBuf,
    ) -> std::io::Result<Vec<StaticApp>> {
        tokio::task::spawn_blocking(move || discover_static_apps(&wwwroot))
            .await
            .map_err(|join_err| {
                std::io::Error::other(format!("apps discovery worker join failed: {join_err}"))
            })?
    }
    use crate::config::AppConfig;
    use axum::http::Uri;
    use reqwest::Url;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn temp_state_dir() -> PathBuf {
        std::env::temp_dir().join(format!("oly-http-wwwroot-{}", Uuid::new_v4()))
    }

    fn test_config(state_dir: PathBuf) -> AppConfig {
        AppConfig {
            paths: crate::config::PathsConfig {
                state_dir: state_dir.clone(),
                sessions_dir: state_dir.join("sessions"),
                db_file: state_dir.join("oly.db"),
                lock_file: state_dir.join("daemon.lock"),
                info_file: state_dir.join("daemon.info"),
                socket_name: "test.sock".into(),
                socket_file: state_dir.join("daemon.sock"),
            },
            http: crate::config::HttpConfig {
                bind: "127.0.0.1".into(),
                port: 0,
            },
            notify: crate::config::NotifyConfig {
                min_interval_seconds: 10,
                prompt_patterns: vec![],
                hook: None,
            },
            limits: crate::config::LimitsConfig {
                max_running_sessions: 10,
                session_eviction_seconds: 15,
                screen_scrollback_rows: crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS,
                silence_seconds: 10,
                stop_grace_seconds: 5,
                max_journal_bytes_per_session: 0,
                journal_retention_days: 0,
            },
            web_push: crate::config::WebPushConfig {
                subject: None,
                vapid_public_key: None,
                vapid_private_key: None,
                proxy: None,
            },
            resume: crate::config::ResumeConfig {
                patterns: crate::config::default_resume_patterns(),
            },
            log_level: "info".into(),
            runtime_overrides: Default::default(),
        }
    }

    #[test]
    fn ensure_wwwroot_creates_directory_and_default_index() {
        let state_dir = temp_state_dir();
        let config = test_config(state_dir.clone());

        let wwwroot = ensure_wwwroot(&config).expect("wwwroot should be created");

        assert_eq!(wwwroot, state_dir.join("wwwroot"));
        assert!(wwwroot.is_dir());
        assert!(wwwroot.join("apps").is_dir());
        let index = fs::read_to_string(wwwroot.join("apps").join("index.html"))
            .expect("index.html should exist");
        assert_eq!(index, DEFAULT_WWWROOT_INDEX);
        assert!(index.contains("oly little apps"));
        assert!(index.contains("wwwroot/apps"));
        assert!(index.contains("/api/static/apps"));

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn ensure_wwwroot_preserves_existing_index() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let apps = wwwroot.join("apps");
        fs::create_dir_all(&apps).expect("apps directory should exist");
        fs::write(apps.join("index.html"), "custom").expect("custom index should be written");
        let config = test_config(state_dir.clone());

        ensure_wwwroot(&config).expect("wwwroot bootstrap should succeed");

        let index = fs::read_to_string(apps.join("index.html")).expect("index.html should exist");
        assert_eq!(index, "custom");

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn discover_static_apps_reads_root_and_folder_apps() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let apps = wwwroot.join("apps");
        fs::create_dir_all(apps.join("admin")).expect("admin directory should be created");
        fs::create_dir_all(apps.join("spa").join("assets"))
            .expect("spa assets directory should be created");
        fs::write(
            apps.join("index.html"),
            "<html><head><title>Home App</title></head><body></body></html>",
        )
        .expect("root app should be written");
        fs::write(
            apps.join("admin").join("index.html"),
            "<html><head><title>Admin Console</title><meta name=\"description\" content=\"Review approvals and session state\"></head><body></body></html>",
        )
        .expect("admin app should be written");
        fs::write(apps.join("admin").join("favicon.svg"), "<svg></svg>")
            .expect("admin favicon should be written");
        fs::write(
            apps.join("spa").join("index.html"),
            "<html><head><title>SPA Shell</title><meta name=\"oly:description\" content=\"Interactive result console\"><link rel=\"icon\" href=\"./assets/icon.png\"></head><body><div id=\"root\"></div><script type=\"module\" src=\"./assets/main.js\"></script></body></html>",
        )
        .expect("spa app should be written");
        fs::write(apps.join("notes.html"), "<title>Ignore Me</title>")
            .expect("file app should be ignored");
        fs::write(
            apps.join("spa").join("assets").join("main.js"),
            "console.log('x');",
        )
        .expect("spa asset should be written");

        let apps = discover_static_apps_async_for_tests(wwwroot.clone())
            .await
            .expect("apps should be discovered");

        assert_eq!(
            apps,
            vec![
                StaticApp {
                    href: "/apps/admin/".into(),
                    title: "Admin Console".into(),
                    description: Some("Review approvals and session state".into()),
                    icon_href: Some("/apps/admin/favicon.svg".into()),
                    app_type: StaticAppKind::SingleHtml,
                },
                StaticApp {
                    href: "/apps/spa/".into(),
                    title: "SPA Shell".into(),
                    description: Some("Interactive result console".into()),
                    icon_href: Some("/apps/spa/assets/icon.png".into()),
                    app_type: StaticAppKind::Spa,
                },
            ]
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn discover_static_apps_prefers_manifest_over_index_html() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("reporting");

        fs::create_dir_all(&app_dir).expect("app directory should be created");
        fs::write(
            app_dir.join("index.html"),
            "<html><head><title>Fallback Index</title></head></html>",
        )
        .expect("fallback index should be written");

        fs::write(
            app_dir.join("dashboard.html"),
            "<html><head><title>Dashboard HTML</title></head><body>ok</body></html>",
        )
        .expect("dashboard entry should be written");

        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            r#"{
                "title": "Reporting Center",
                "description": "Manifest metadata wins",
                "entry": "dashboard.html"
            }"#,
        )
        .expect("manifest should be written");

        let apps = discover_static_apps_async_for_tests(wwwroot.clone())
            .await
            .expect("apps should be discovered");

        assert_eq!(
            apps,
            vec![StaticApp {
                href: "/apps/reporting/".into(),
                title: "Reporting Center".into(),
                description: Some("Manifest metadata wins".into()),
                icon_href: None,
                app_type: StaticAppKind::SingleHtml,
            }]
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn resolve_app_request_uses_manifest_entry_and_nested_assets() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("nested");
        fs::create_dir_all(app_dir.join("dist").join("assets"))
            .expect("dist assets directory should be created");
        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            r#"{
                "title": "Nested",
                "entry": "dist/index.html"
            }"#,
        )
        .expect("manifest should be written");
        fs::write(
            app_dir.join("dist").join("index.html"),
            "<html><head><title>Nested App</title></head></html>",
        )
        .expect("entry should be written");
        fs::write(
            app_dir.join("dist").join("assets").join("main.js"),
            "console.log('nested');",
        )
        .expect("asset should be written");

        let root_uri: Uri = "/apps/nested/".parse().expect("URI should parse");
        let asset_uri: Uri = "/apps/nested/assets/main.js"
            .parse()
            .expect("URI should parse");

        let root = resolve_app_request(wwwroot.clone(), root_uri.clone())
            .await
            .expect("request should resolve");
        let asset = resolve_app_request(wwwroot.clone(), asset_uri.clone())
            .await
            .expect("request should resolve");

        assert_eq!(
            root,
            Some(AppRequestTarget::LocalFile(
                app_dir.join("dist").join("index.html")
            ))
        );
        assert_eq!(
            asset,
            Some(AppRequestTarget::LocalFile(
                app_dir.join("dist").join("assets").join("main.js")
            ))
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn resolve_app_request_uses_redirect_file_and_folder_after_local_candidates() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("fallback");
        fs::create_dir_all(app_dir.join("dist").join("assets"))
            .expect("dist assets directory should be created");
        let shared_dir = state_dir.join("shared-assets");
        fs::create_dir_all(shared_dir.join("assets"))
            .expect("shared assets directory should be created");
        let fallback_file = state_dir.join("fallback.html");
        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            serde_json::to_string(&serde_json::json!({
                "title": "Fallback",
                "entry": "dist/index.html",
                "redirect_files": [shared_dir.clone(), fallback_file.clone()],
            }))
            .expect("manifest JSON should serialize"),
        )
        .expect("manifest should be written");
        fs::write(
            app_dir.join("dist").join("index.html"),
            "<html><head><title>Fallback App</title></head></html>",
        )
        .expect("entry should be written");
        fs::write(
            shared_dir.join("assets").join("main.js"),
            "console.log('shared');",
        )
        .expect("shared asset should be written");
        fs::write(&fallback_file, "<html><body>fallback</body></html>")
            .expect("fallback file should be written");

        let asset_uri: Uri = "/apps/fallback/assets/main.js"
            .parse()
            .expect("URI should parse");
        let unmatched_uri: Uri = "/apps/fallback/missing/page"
            .parse()
            .expect("URI should parse");

        let asset = resolve_app_request(wwwroot.clone(), asset_uri.clone())
            .await
            .expect("request should resolve");
        let unmatched = resolve_app_request(wwwroot.clone(), unmatched_uri.clone())
            .await
            .expect("request should resolve");
        let expected_shared_asset = fs::canonicalize(shared_dir.join("assets").join("main.js"))
            .expect("shared asset path should canonicalize");
        let expected_fallback_file =
            fs::canonicalize(&fallback_file).expect("fallback file path should canonicalize");

        assert_eq!(
            asset,
            Some(AppRequestTarget::LocalFile(expected_shared_asset))
        );
        assert_eq!(
            unmatched,
            Some(AppRequestTarget::LocalFile(expected_fallback_file))
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn resolve_app_request_builds_proxy_url_from_manifest_entry() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("remote");
        fs::create_dir_all(&app_dir).expect("app directory should be created");
        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            r#"{
                "title": "Remote",
                "entry": "https://example.com/dash/index.html?mode=full"
            }"#,
        )
        .expect("manifest should be written");

        let uri: Uri = "/apps/remote/assets/main.js?theme=dark&token=secret"
            .parse()
            .expect("URI should parse");

        let resolved = resolve_app_request(wwwroot.clone(), uri.clone())
            .await
            .expect("request should resolve");

        assert_eq!(
            resolved,
            Some(AppRequestTarget::Proxy(vec![
                Url::parse("https://example.com/dash/assets/main.js?theme=dark")
                    .expect("proxy URL should parse"),
                Url::parse("https://example.com/assets/main.js?theme=dark")
                    .expect("root fallback proxy URL should parse"),
            ]))
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn resolve_app_request_adds_root_fallback_for_vite_client() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("demo2");
        fs::create_dir_all(&app_dir).expect("app directory should be created");
        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            r#"{
                "title": "Remote Demo",
                "entry": "http://127.0.0.1:5173/"
            }"#,
        )
        .expect("manifest should be written");

        let uri: Uri = "/apps/demo2/@vite/client"
            .parse()
            .expect("URI should parse");

        let resolved = resolve_app_request(wwwroot.clone(), uri.clone())
            .await
            .expect("request should resolve");

        assert_eq!(
            resolved,
            Some(AppRequestTarget::Proxy(vec![
                Url::parse("http://127.0.0.1:5173/@vite/client")
                    .expect("entry-relative vite URL should parse"),
            ]))
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn app_local_request_candidates_include_entry_directory_fallback() {
        let candidates = app_local_request_candidates("dist/index.html", "assets/main.js", false);

        assert_eq!(
            candidates,
            vec![
                "assets/main.js".to_string(),
                "assets/main.js/index.html".to_string(),
                "dist/assets/main.js".to_string(),
                "dist/assets/main.js/index.html".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn resolve_app_request_rejects_missing_redirect_files() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("invalid");
        fs::create_dir_all(app_dir.join("dist")).expect("dist directory should be created");
        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            r#"{
                "title": "Invalid",
                "entry": "dist/index.html",
                "redirect_files": ["../../missing-folder"]
            }"#,
        )
        .expect("manifest should be written");
        fs::write(
            app_dir.join("dist").join("index.html"),
            "<html><head><title>Invalid</title></head></html>",
        )
        .expect("entry should be written");

        let root_uri: Uri = "/apps/invalid/".parse().expect("URI should parse");
        let err = resolve_app_request(wwwroot.clone(), root_uri.clone())
            .await
            .expect_err("request should fail");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("redirect path"));
        assert!(err.to_string().contains("does not exist"));

        let _ = fs::remove_dir_all(state_dir);
    }

    #[tokio::test]
    async fn discover_static_apps_allows_entry_from_redirect_dir_with_manifest_fields() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        let app_dir = wwwroot.join("apps").join("interview-markdown-viewer");
        let notes_dir = state_dir.join("interview-notes");

        fs::create_dir_all(&app_dir).expect("app directory should be created");
        fs::create_dir_all(&notes_dir).expect("notes directory should be created");
        fs::write(
            notes_dir.join("index.html"),
            "<html><head><title>Interview Notes</title></head><body>ok</body></html>",
        )
        .expect("redirected entry should be written");
        fs::write(
            app_dir.join(APP_MANIFEST_FILE),
            serde_json::to_string(&serde_json::json!({
                "entry": "index.html",
                "title": "Interview Markdown Viewer",
                "description": "A reusable Oly-hosted viewer for interview preparation notes and markdown knowledge packs.",
                "redirect_files": [notes_dir.clone()],
            }))
            .expect("manifest JSON should serialize"),
        )
        .expect("manifest should be written");

        let apps = discover_static_apps_async_for_tests(wwwroot.clone())
            .await
            .expect("apps should be discovered");
        let root_uri: Uri = "/apps/interview-markdown-viewer/"
            .parse()
            .expect("URI should parse");
        let resolved = resolve_app_request(wwwroot.clone(), root_uri.clone())
            .await
            .expect("request should resolve");

        assert_eq!(
            apps,
            vec![StaticApp {
                href: "/apps/interview-markdown-viewer/".into(),
                title: "Interview Markdown Viewer".into(),
                description: Some(
                    "A reusable Oly-hosted viewer for interview preparation notes and markdown knowledge packs."
                        .into(),
                ),
                icon_href: None,
                app_type: StaticAppKind::SingleHtml,
            }]
        );
        assert_eq!(
            resolved,
            Some(AppRequestTarget::LocalFile(
                fs::canonicalize(notes_dir.join("index.html"))
                    .expect("redirected entry should canonicalize")
            ))
        );

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn extract_title_reads_case_insensitive_title_tag() {
        let html = "<HTML><HEAD><TITLE> Dashboard App </TITLE></HEAD></HTML>";

        let title = extract_title(html);

        assert_eq!(title.as_deref(), Some("Dashboard App"));
    }

    #[test]
    fn extract_meta_content_reads_name_or_property_attributes() {
        let html = concat!(
            "<head>",
            "<meta property=\"og:description\" content=\"Graph summary\">",
            "<meta name='description' content='Human readable summary'>",
            "</head>"
        );

        assert_eq!(
            extract_meta_content(html, "og:description").as_deref(),
            Some("Graph summary")
        );
        assert_eq!(
            extract_meta_content(html, "description").as_deref(),
            Some("Human readable summary")
        );
    }

    #[test]
    fn extract_app_description_prefers_oly_specific_metadata() {
        let html = concat!(
            "<head>",
            "<meta name=\"description\" content=\"Fallback\">",
            "<meta name=\"oly:description\" content=\"Preferred\">",
            "</head>"
        );

        let description = extract_app_description(html);

        assert_eq!(description.as_deref(), Some("Preferred"));
    }

    #[test]
    fn extract_app_kind_uses_meta_override_before_heuristics() {
        let html = concat!(
            "<head><meta name=\"oly:app-type\" content=\"single-html\"></head>",
            "<body><div id=\"root\"></div><script type=\"module\"></script></body>"
        );

        let kind = extract_app_kind(html);

        assert_eq!(kind, StaticAppKind::SingleHtml);
    }

    #[test]
    fn extract_app_kind_detects_spa_heuristics() {
        let html = "<body><div id=\"root\"></div><script type=\"module\" src=\"./assets/main.js\"></script></body>";

        let kind = extract_app_kind(html);

        assert_eq!(kind, StaticAppKind::Spa);
    }

    #[test]
    fn extract_app_icon_href_reads_link_tag() {
        let html = "<head><link rel=\"icon\" href=\"./assets/icon.png\"></head>";

        let icon_href = extract_app_icon_href(html, "/apps/spa/");

        assert_eq!(icon_href.as_deref(), Some("/apps/spa/assets/icon.png"));
    }

    #[test]
    fn resolve_app_asset_href_rejects_parent_segments() {
        let icon_href = resolve_app_asset_href("/apps/spa/", "../icon.png");

        assert_eq!(icon_href, None);
    }
}
