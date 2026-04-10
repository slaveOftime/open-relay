use axum::{
    Json,
    extract::State,
    http::Uri,
    response::{IntoResponse, Response},
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Component, Path, PathBuf},
};
use tracing::{error, info};

use crate::config::AppConfig;

use super::AppState;

const DEFAULT_WWWROOT_INDEX: &str = include_str!("apps-index.html");
const APP_MANIFEST_FILE: &str = "oly.app.json";

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppEntry {
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
struct AppDefinition {
    static_app: StaticApp,
    entry: AppEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AppRequestTarget {
    LocalFile(PathBuf),
    Proxy(Vec<Url>),
}

#[derive(Debug, Deserialize)]
struct AppManifest {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon_href: Option<String>,
    #[serde(default)]
    app_type: Option<String>,
    #[serde(default)]
    redirect_files: Vec<String>,
    entry: String,
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

pub(super) fn resolve_app_request(
    wwwroot: &Path,
    uri: &Uri,
) -> io::Result<Option<AppRequestTarget>> {
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

pub(super) fn find_existing_local_asset(
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

fn build_manifest_app_definition(
    app_dir: &Path,
    app_href: &str,
    fallback_title: &str,
    manifest: AppManifest,
) -> io::Result<AppDefinition> {
    let entry = resolve_manifest_entry(app_dir, &manifest.entry, &manifest.redirect_files)?;
    let entry_html = match &entry {
        AppEntry::Local {
            entry_source_path, ..
        } => maybe_read_entry_html(entry_source_path)?,
        AppEntry::Proxy { .. } => None,
    };
    let entry_source_dir = match &entry {
        AppEntry::Local {
            entry_source_path, ..
        } => entry_source_path.parent(),
        AppEntry::Proxy { .. } => None,
    };

    let title = cleaned_field(manifest.title)
        .or_else(|| entry_html.as_deref().and_then(extract_title))
        .unwrap_or_else(|| fallback_title.to_string());
    let description = cleaned_field(manifest.description)
        .or_else(|| entry_html.as_deref().and_then(extract_app_description));
    let icon_href = cleaned_field(manifest.icon_href)
        .and_then(|value| resolve_manifest_asset_href(app_href, &value))
        .or_else(|| {
            entry_html
                .as_deref()
                .and_then(|html| extract_app_icon_href(html, app_href))
        })
        .or_else(|| detect_app_icon_href(entry_source_dir, app_href))
        .or_else(|| detect_app_icon_href(Some(app_dir), app_href));
    let app_type = cleaned_field(manifest.app_type)
        .as_deref()
        .and_then(StaticAppKind::from_meta_value)
        .or_else(|| entry_html.as_deref().map(extract_app_kind))
        .unwrap_or(StaticAppKind::SingleHtml);

    Ok(AppDefinition {
        static_app: StaticApp {
            href: app_href.to_string(),
            title,
            description,
            icon_href,
            app_type,
        },
        entry,
    })
}

fn load_app_manifest(app_dir: &Path) -> io::Result<Option<AppManifest>> {
    let manifest_path = app_dir.join(APP_MANIFEST_FILE);
    if manifest_path.is_file() {
        return Ok(Some(read_manifest_file(&manifest_path)?));
    }

    Ok(None)
}

fn read_manifest_file(path: &Path) -> io::Result<AppManifest> {
    let raw = std::fs::read_to_string(path)?;
    parse_manifest(&raw, path)
}

fn parse_manifest(raw: &str, source_path: &Path) -> io::Result<AppManifest> {
    serde_json::from_str(raw)
        .map_err(|err| invalid_data(format!("failed to parse {}: {err}", source_path.display())))
}

fn resolve_manifest_entry(
    app_dir: &Path,
    entry: &str,
    redirect_files: &[String],
) -> io::Result<AppEntry> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err(invalid_data("app manifest entry cannot be empty"));
    }

    if let Ok(url) = Url::parse(entry) {
        if matches!(url.scheme(), "http" | "https") {
            if !redirect_files.is_empty() {
                return Err(invalid_data(
                    "app manifest redirect files require a local entry",
                ));
            }
            // Block proxying to private LAN / link-local addresses to
            // prevent SSRF via crafted oly.app.json manifests.
            if is_private_proxy_target(&url) {
                return Err(invalid_data(
                    "app manifest proxy entry must not target private or link-local addresses",
                ));
            }
            return Ok(AppEntry::Proxy { entry_url: url });
        }
    }

    let entry_path = normalize_relative_asset_path(entry)
        .ok_or_else(|| invalid_data("app manifest entry must stay inside the app directory"))?;
    let redirect_files = resolve_manifest_redirect_files(app_dir, redirect_files)?;
    let Some(entry_source_path) =
        resolve_manifest_entry_source(app_dir, &entry_path, &redirect_files)?
    else {
        return Err(invalid_data(format!(
            "app manifest entry {} does not exist in the app directory or redirect files",
            app_dir
                .join(entry_path.replace('/', std::path::MAIN_SEPARATOR_STR))
                .display()
        )));
    };

    Ok(AppEntry::Local {
        entry_path,
        entry_source_path,
        redirect_files,
    })
}

fn maybe_read_entry_html(entry_source_path: &Path) -> io::Result<Option<String>> {
    let extension = entry_source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());
    if !matches!(extension.as_deref(), Some("html" | "htm")) {
        return Ok(None);
    }

    Ok(Some(std::fs::read_to_string(entry_source_path)?))
}

fn resolve_manifest_entry_source(
    app_dir: &Path,
    entry_path: &str,
    redirect_files: &[PathBuf],
) -> io::Result<Option<PathBuf>> {
    let local_path = app_dir.join(entry_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if file_exists(&local_path)? {
        return Ok(Some(local_path));
    }

    let candidates = [entry_path.to_string()];
    for redirect_path in redirect_files {
        if let Some(path) = find_existing_redirect_asset(redirect_path, &candidates)? {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

fn resolve_manifest_redirect_files(
    app_dir: &Path,
    redirect_files: &[String],
) -> io::Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    for redirect_file in redirect_files {
        let redirect_file = redirect_file.trim();
        if redirect_file.is_empty() {
            continue;
        }

        let resolved_path = canonicalize_redirect_path(app_dir, redirect_file)?;
        let metadata = std::fs::metadata(&resolved_path)?;
        if !metadata.is_file() && !metadata.is_dir() {
            return Err(invalid_data(format!(
                "app manifest redirect path {} must be a file or directory",
                resolved_path.display()
            )));
        }
        if !resolved.contains(&resolved_path) {
            resolved.push(resolved_path);
        }
    }

    Ok(resolved)
}

fn canonicalize_redirect_path(app_dir: &Path, redirect_file: &str) -> io::Result<PathBuf> {
    let candidate = Path::new(redirect_file);
    let resolved_path = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        app_dir.join(candidate)
    };
    std::fs::canonicalize(&resolved_path).map_err(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            invalid_data(format!(
                "app manifest redirect path {} does not exist",
                resolved_path.display()
            ))
        } else {
            err
        }
    })
}

fn cleaned_field(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
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

fn split_app_request_path(path: &str) -> Option<(String, String, bool)> {
    let remainder = path.strip_prefix("/apps/")?;
    if remainder.is_empty() {
        return None;
    }

    let trailing_slash = path.ends_with('/');
    let normalized = normalize_relative_asset_path(remainder)?;
    let (slug, tail) = normalized
        .split_once('/')
        .map_or((normalized.as_str(), ""), |(slug, tail)| (slug, tail));

    if slug.is_empty() {
        None
    } else {
        Some((slug.to_string(), tail.to_string(), trailing_slash))
    }
}

fn app_local_request_candidates(
    entry_path: &str,
    request_tail: &str,
    trailing_slash: bool,
) -> Vec<String> {
    if request_tail.is_empty() {
        return vec![entry_path.to_string()];
    }

    let mut candidates = local_request_candidates(request_tail, trailing_slash);
    if let Some(entry_dir) = entry_parent_dir(entry_path) {
        append_local_request_candidates_with_prefix(
            &mut candidates,
            &entry_dir,
            request_tail,
            trailing_slash,
        );
    }

    candidates
}

fn append_local_request_candidates_with_prefix(
    candidates: &mut Vec<String>,
    prefix: &str,
    request_tail: &str,
    trailing_slash: bool,
) {
    for candidate in local_request_candidates(request_tail, trailing_slash) {
        let prefixed = format!("{prefix}/{candidate}");
        if !candidates.contains(&prefixed) {
            candidates.push(prefixed);
        }
    }
}

fn find_existing_app_local_asset(
    app_dir: &Path,
    candidates: &[String],
) -> io::Result<Option<PathBuf>> {
    for candidate in candidates {
        let full_path = app_dir.join(candidate.replace('/', std::path::MAIN_SEPARATOR_STR));
        if file_exists(&full_path)? {
            return Ok(Some(full_path));
        }
    }
    Ok(None)
}

fn find_existing_redirect_asset(
    redirect_path: &Path,
    candidates: &[String],
) -> io::Result<Option<PathBuf>> {
    if file_exists(redirect_path)? {
        return Ok(Some(redirect_path.to_path_buf()));
    }
    if !directory_exists(redirect_path)? {
        return Ok(None);
    }

    for candidate in candidates {
        let full_path = redirect_path.join(candidate.replace('/', std::path::MAIN_SEPARATOR_STR));
        if file_exists(&full_path)? {
            return Ok(Some(full_path));
        }
    }

    Ok(None)
}

fn local_request_candidates(path: &str, trailing_slash: bool) -> Vec<String> {
    let mut candidates = Vec::with_capacity(3);
    if trailing_slash {
        candidates.push(format!("{path}/index.html"));
        return candidates;
    }

    candidates.push(path.to_string());
    if Path::new(path).extension().is_none() {
        candidates.push(format!("{path}.html"));
    }
    candidates.push(format!("{path}/index.html"));
    candidates.dedup();
    candidates
}

fn entry_parent_dir(entry_path: &str) -> Option<String> {
    normalize_relative_asset_path(
        Path::new(entry_path)
            .parent()
            .and_then(|parent| parent.to_str())
            .unwrap_or_default(),
    )
}

fn normalize_relative_asset_path(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

fn local_asset_exists(wwwroot: &Path, relative_path: &str) -> io::Result<bool> {
    let full_path = wwwroot.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    file_exists(&full_path)
}

fn file_exists(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn directory_exists(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn build_proxy_target_urls(
    entry_url: &Url,
    request_tail: &str,
    query: Option<&str>,
) -> io::Result<Vec<Url>> {
    let mut targets = Vec::new();
    if request_tail.is_empty() {
        targets.push(with_proxy_query(entry_url.clone(), query));
    } else {
        let entry_relative = entry_url.join(request_tail).map_err(|err| {
            invalid_data(format!(
                "failed to join proxied app URL {entry_url} with {request_tail}: {err}"
            ))
        })?;
        targets.push(with_proxy_query(entry_relative, query));

        let root_relative = origin_root_url(entry_url)
            .join(request_tail)
            .map_err(|err| {
                invalid_data(format!(
                    "failed to build root-relative proxied app URL {entry_url} with {request_tail}: {err}"
                ))
            })?;
        let root_relative = with_proxy_query(root_relative, query);
        if !targets.iter().any(|existing| existing == &root_relative) {
            targets.push(root_relative);
        }

        let public_path_relative = origin_root_url(entry_url)
            .join(request_tail.trim_start_matches('/'))
            .map_err(|err| {
                invalid_data(format!(
                    "failed to build public-path proxied app URL {entry_url} with {request_tail}: {err}"
                ))
            })?;
        let public_path_relative = with_proxy_query(public_path_relative, query);
        if !targets
            .iter()
            .any(|existing| existing == &public_path_relative)
        {
            targets.push(public_path_relative);
        }
    }

    Ok(targets)
}

fn with_proxy_query(mut target: Url, query: Option<&str>) -> Url {
    if let Some(filtered_query) = filtered_proxy_query(query) {
        let merged_query = match target.query() {
            Some(existing) if !existing.is_empty() => format!("{existing}&{filtered_query}"),
            _ => filtered_query,
        };
        target.set_query(Some(&merged_query));
    }

    target
}

fn origin_root_url(entry_url: &Url) -> Url {
    let mut root = entry_url.clone();
    root.set_path("/");
    root.set_query(None);
    root.set_fragment(None);
    root
}

fn filtered_proxy_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    let filtered = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| *pair != "token" && !pair.starts_with("token="))
        .collect::<Vec<_>>();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.join("&"))
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Returns `true` if the proxy target URL resolves to a private LAN,
/// link-local, or unspecified address.  Loopback (127.0.0.0/8, ::1) is
/// intentionally allowed because the primary use-case for proxy entries is
/// forwarding to local dev servers (e.g. Vite on 127.0.0.1:5173).
fn is_private_proxy_target(url: &Url) -> bool {
    use std::net::IpAddr;

    let host = match url.host_str() {
        Some(h) => h,
        None => return true, // No host → reject
    };

    // Try to parse as IP directly first.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_ssrf_dangerous_ip(&ip);
    }

    false
}

/// Returns `true` for IPs that are SSRF-dangerous: private LAN ranges,
/// link-local (cloud metadata), and unspecified.  Loopback is allowed.
fn is_ssrf_dangerous_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_private()        // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                || v4.is_link_local()  // 169.254.0.0/16 (cloud metadata)
                || v4.is_unspecified() // 0.0.0.0
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_unspecified() // ::
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_private() || v4.is_link_local()
                })
        }
    }
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

fn resolve_manifest_asset_href(app_href: &str, asset_href: &str) -> Option<String> {
    let asset_href = asset_href.trim();
    if asset_href.is_empty() {
        return None;
    }

    if asset_href.starts_with("http://")
        || asset_href.starts_with("https://")
        || asset_href.starts_with("//")
        || asset_href.starts_with("data:")
    {
        return Some(asset_href.to_string());
    }

    resolve_app_asset_href(app_href, asset_href)
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
        APP_MANIFEST_FILE, AppRequestTarget, DEFAULT_WWWROOT_INDEX, StaticApp, StaticAppKind,
        app_local_request_candidates, discover_static_apps, ensure_wwwroot,
        extract_app_description, extract_app_icon_href, extract_app_kind, extract_meta_content,
        extract_title, resolve_app_asset_href, resolve_app_request,
    };
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
            http_port: 0,
            log_level: "info".into(),
            stop_grace_seconds: 5,
            prompt_patterns: vec![],
            web_push_subject: None,
            web_push_vapid_public_key: None,
            web_push_vapid_private_key: None,
            state_dir: state_dir.clone(),
            sessions_dir: state_dir.join("sessions"),
            db_file: state_dir.join("oly.db"),
            lock_file: state_dir.join("daemon.lock"),
            info_file: state_dir.join("daemon.info"),
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
    fn discover_static_apps_prefers_manifest_over_index_html() {
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

        let apps = discover_static_apps(&wwwroot).expect("apps should be discovered");

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

    #[test]
    fn resolve_app_request_uses_manifest_entry_and_nested_assets() {
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

        let root = resolve_app_request(&wwwroot, &root_uri).expect("request should resolve");
        let asset = resolve_app_request(&wwwroot, &asset_uri).expect("request should resolve");

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

    #[test]
    fn resolve_app_request_uses_redirect_file_and_folder_after_local_candidates() {
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

        let asset = resolve_app_request(&wwwroot, &asset_uri).expect("request should resolve");
        let unmatched =
            resolve_app_request(&wwwroot, &unmatched_uri).expect("request should resolve");
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

    #[test]
    fn resolve_app_request_builds_proxy_url_from_manifest_entry() {
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

        let resolved = resolve_app_request(&wwwroot, &uri).expect("request should resolve");

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

    #[test]
    fn resolve_app_request_adds_root_fallback_for_vite_client() {
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

        let resolved = resolve_app_request(&wwwroot, &uri).expect("request should resolve");

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

    #[test]
    fn resolve_app_request_rejects_missing_redirect_files() {
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
        let err = resolve_app_request(&wwwroot, &root_uri).expect_err("request should fail");

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("redirect path"));
        assert!(err.to_string().contains("does not exist"));

        let _ = fs::remove_dir_all(state_dir);
    }

    #[test]
    fn discover_static_apps_allows_entry_from_redirect_dir_with_manifest_fields() {
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

        let apps = discover_static_apps(&wwwroot).expect("apps should be discovered");
        let root_uri: Uri = "/apps/interview-markdown-viewer/"
            .parse()
            .expect("URI should parse");
        let resolved = resolve_app_request(&wwwroot, &root_uri).expect("request should resolve");

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
