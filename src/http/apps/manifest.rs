//! App-manifest parsing and definition construction. PLAN2 S1.2.
use reqwest::Url;
use serde::Deserialize;
use std::{
    io,
    path::{Path, PathBuf},
};

use super::AppDefinition;
use super::AppEntry;
use super::StaticApp;
use super::StaticAppKind;
use super::html::{
    detect_app_icon_href, extract_app_description, extract_app_icon_href, extract_app_kind,
    extract_title, resolve_manifest_asset_href,
};
use super::proxy_targets::{invalid_data, is_private_proxy_target};
use super::resolve::{file_exists, find_existing_redirect_asset, normalize_relative_asset_path};

pub const APP_MANIFEST_FILE: &str = "oly.app.json";

#[derive(Debug, Deserialize)]
pub(super) struct AppManifest {
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

pub(super) fn load_app_manifest(app_dir: &Path) -> io::Result<Option<AppManifest>> {
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

pub(super) fn build_manifest_app_definition(
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

fn resolve_manifest_entry(
    app_dir: &Path,
    entry: &str,
    redirect_files: &[String],
) -> io::Result<AppEntry> {
    let entry = entry.trim();
    if entry.is_empty() {
        return Err(invalid_data("app manifest entry cannot be empty"));
    }

    if let Ok(url) = Url::parse(entry)
        && matches!(url.scheme(), "http" | "https")
    {
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

pub(super) fn maybe_read_entry_html(entry_source_path: &Path) -> io::Result<Option<String>> {
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
