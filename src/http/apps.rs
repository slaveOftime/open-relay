use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::{
    io,
    path::{Path, PathBuf},
};
use tracing::{error, info};

use crate::config::AppConfig;

use super::AppState;

const DEFAULT_WWWROOT_INDEX: &str = include_str!("apps-index.html");

#[derive(Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum StaticAppKind {
    SingleHtml,
    Spa,
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
    match discover_static_apps(&state.config.wwwroot_dir()) {
        Ok(apps) => Json(apps).into_response(),
        Err(err) => {
            error!(%err, "failed to enumerate apps in {}", state.config.wwwroot_dir().display());
            axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
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

        let index_path = entry.path().join("index.html");
        if !index_path.is_file() {
            continue;
        }

        let slug = name.to_string_lossy();
        apps.push(build_static_app(
            &index_path,
            &format!("/apps/{slug}/"),
            slug.as_ref(),
        )?);
    }

    apps.sort_by(|left, right| left.href.cmp(&right.href));
    Ok(apps)
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

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let title_start = lower.find("<title")?;
    let content_start = lower[title_start..].find('>')? + title_start + 1;
    let content_end = lower[content_start..].find("</title>")? + content_start;
    let title = html[content_start..content_end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

fn extract_app_description(html: &str) -> Option<String> {
    extract_meta_content(html, "oly:description")
        .or_else(|| extract_meta_content(html, "description"))
        .or_else(|| extract_meta_content(html, "og:description"))
}

fn extract_app_kind(html: &str) -> StaticAppKind {
    if let Some(raw_kind) = extract_meta_content(html, "oly:app-type")
        .or_else(|| extract_meta_content(html, "oly:type"))
    {
        if let Some(app_kind) = StaticAppKind::from_meta_value(&raw_kind) {
            return app_kind;
        }
    }

    infer_app_kind(html)
}

fn extract_app_icon_href(html: &str, app_href: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("<link") {
        let tag_start = offset + relative_start;
        let tag_end = match lower[tag_start..].find('>') {
            Some(relative_end) => tag_start + relative_end + 1,
            None => break,
        };
        let tag = &html[tag_start..tag_end];
        let rel = extract_html_attribute(tag, "rel");
        let href = extract_html_attribute(tag, "href");

        if rel
            .as_deref()
            .is_some_and(|value| link_rel_mentions_icon(value))
        {
            if let Some(icon_href) = href.and_then(|value| resolve_app_asset_href(app_href, &value))
            {
                return Some(icon_href);
            }
        }

        offset = tag_end;
    }

    None
}

fn detect_app_icon_href(app_dir: Option<&Path>, app_href: &str) -> Option<String> {
    let app_dir = app_dir?;
    for candidate in [
        "favicon.svg",
        "favicon.ico",
        "favicon.png",
        "apple-touch-icon.png",
    ] {
        if app_dir.join(candidate).is_file() {
            return resolve_app_asset_href(app_href, candidate);
        }
    }

    None
}

fn link_rel_mentions_icon(rel: &str) -> bool {
    rel.split_ascii_whitespace().any(|part| {
        part.eq_ignore_ascii_case("icon")
            || part.eq_ignore_ascii_case("shortcut")
            || part.eq_ignore_ascii_case("apple-touch-icon")
    })
}

fn resolve_app_asset_href(app_href: &str, asset_href: &str) -> Option<String> {
    let asset_href = asset_href.trim();
    if asset_href.is_empty()
        || asset_href.starts_with("http://")
        || asset_href.starts_with("https://")
        || asset_href.starts_with("//")
        || asset_href.starts_with("data:")
        || asset_href.starts_with('#')
    {
        return None;
    }

    if asset_href.starts_with('/') {
        return Some(asset_href.to_string());
    }

    let mut base = app_href.trim_end_matches('/').to_string();
    if !base.ends_with('/') {
        base.push('/');
    }

    let normalized = asset_href
        .strip_prefix("./")
        .unwrap_or(asset_href)
        .trim_start_matches('/');
    if normalized.contains("../") {
        return None;
    }

    Some(format!("{base}{normalized}"))
}

fn extract_meta_content(html: &str, attribute_value: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut offset = 0;

    while let Some(relative_start) = lower[offset..].find("<meta") {
        let tag_start = offset + relative_start;
        let tag_end = match lower[tag_start..].find('>') {
            Some(relative_end) => tag_start + relative_end + 1,
            None => break,
        };
        let tag = &html[tag_start..tag_end];
        let name =
            extract_html_attribute(tag, "name").or_else(|| extract_html_attribute(tag, "property"));

        if name
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(attribute_value))
        {
            return extract_html_attribute(tag, "content").filter(|value| !value.is_empty());
        }

        offset = tag_end;
    }

    None
}

fn extract_html_attribute(tag: &str, attribute_name: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut index = 0;

    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        if index >= bytes.len() || matches!(bytes[index], b'<' | b'>' | b'/') {
            index += 1;
            continue;
        }

        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'='
            && bytes[index] != b'>'
        {
            index += 1;
        }

        if name_start == index {
            index += 1;
            continue;
        }

        let candidate_name = &tag[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }

        let mut value = String::new();
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }

            if index < bytes.len() && matches!(bytes[index], b'"' | b'\'') {
                let quote = bytes[index];
                index += 1;
                let value_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                value = tag[value_start..index].trim().to_string();
                if index < bytes.len() {
                    index += 1;
                }
            } else {
                let value_start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && bytes[index] != b'>'
                {
                    index += 1;
                }
                value = tag[value_start..index].trim().to_string();
            }
        }

        if candidate_name.eq_ignore_ascii_case(attribute_name) {
            return Some(value);
        }
    }

    None
}

fn infer_app_kind(html: &str) -> StaticAppKind {
    let lower = html.to_ascii_lowercase();
    let has_module_script = lower.contains("type=\"module\"") || lower.contains("type='module'");
    let has_mount_root = lower.contains("id=\"root\"")
        || lower.contains("id='root'")
        || lower.contains("id=\"app\"")
        || lower.contains("id='app'");
    let has_asset_pipeline = lower.contains("src=\"./assets/")
        || lower.contains("src=\"assets/")
        || lower.contains("src=\"/assets/")
        || lower.contains("href=\"./assets/")
        || lower.contains("href=\"assets/")
        || lower.contains("href=\"/assets/")
        || lower.contains("src='./assets/")
        || lower.contains("src='assets/")
        || lower.contains("src='/assets/")
        || lower.contains("href='./assets/")
        || lower.contains("href='assets/")
        || lower.contains("href='/assets/");

    if has_module_script || has_mount_root || has_asset_pipeline {
        StaticAppKind::Spa
    } else {
        StaticAppKind::SingleHtml
    }
}

impl StaticAppKind {
    fn from_meta_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "spa" => Some(Self::Spa),
            "single_html" | "single-html" | "html" | "singlefile" | "single-file" => {
                Some(Self::SingleHtml)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StaticApp, StaticAppKind, discover_static_apps, ensure_wwwroot, extract_app_description,
        extract_app_icon_href, extract_app_kind, extract_meta_content, extract_title,
        resolve_app_asset_href,
    };
    use crate::config::AppConfig;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn temp_state_dir() -> PathBuf {
        std::env::temp_dir().join(format!("oly-http-wwwroot-{}", Uuid::new_v4()))
    }

    fn test_config(state_dir: PathBuf) -> AppConfig {
        AppConfig {
            http_port: 0,
            log_level: "info".into(),
            ring_buffer_bytes: 1_024,
            stop_grace_seconds: 5,
            prompt_patterns: vec![],
            web_push_subject: None,
            web_push_vapid_public_key: None,
            web_push_vapid_private_key: None,
            state_dir: state_dir.clone(),
            sessions_dir: state_dir.join("sessions"),
            db_file: state_dir.join("oly.db"),
            lock_file: state_dir.join("daemon.lock"),
            socket_name: "test.sock".into(),
            socket_file: state_dir.join("daemon.sock"),
            silence_seconds: 10,
            session_eviction_seconds: 15,
            max_running_sessions: 10,
            notification_hook: None,
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
        assert!(index.contains("oly dashboard"));
        assert!(index.contains("wwwroot/apps"));
        assert!(index.contains("/api/static/apps"));

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn ensure_wwwroot_preserves_existing_index() {
        let state_dir = temp_state_dir();
        let wwwroot = state_dir.join("wwwroot");
        fs::create_dir_all(&wwwroot).expect("wwwroot directory should exist");
        fs::write(wwwroot.join("index.html"), "custom").expect("custom index should be written");
        let config = test_config(state_dir.clone());

        ensure_wwwroot(&config).expect("wwwroot bootstrap should succeed");

        let index =
            fs::read_to_string(wwwroot.join("index.html")).expect("index.html should exist");
        assert_eq!(index, "custom");

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn discover_static_apps_reads_root_and_folder_apps() {
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

        let apps = discover_static_apps(&wwwroot).expect("apps should be discovered");

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
