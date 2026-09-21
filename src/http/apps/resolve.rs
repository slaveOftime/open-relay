//! Local-asset resolution, request-path candidates, and FS existence helpers.
//!
//! Sibling to `manifest.rs`, `html.rs`, `proxy_targets.rs`. PLAN2 S1.2.
use std::{
    io,
    path::{Component, Path, PathBuf},
};

/// Break `/apps/<slug>/<rest>` into `(slug, tail, trailing_slash)`. Returns
/// `None` when the request does not address an app route.
pub(super) fn split_app_request_path(path: &str) -> Option<(String, String, bool)> {
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

/// Build the list of relative asset paths to try for a given request tail.
pub fn app_local_request_candidates(
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

/// First candidate under `app_dir` that exists on disk.
pub(super) fn find_existing_app_local_asset(
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

/// `redirect_path` may itself be a file, or a directory whose `candidates`
/// are searched.
pub(super) fn find_existing_redirect_asset(
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

/// Strip `.`/`..` components and refuse absolute paths / drive-relative.
pub(super) fn normalize_relative_asset_path(path: &str) -> Option<String> {
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

pub(super) fn local_asset_exists(wwwroot: &Path, relative_path: &str) -> io::Result<bool> {
    let full_path = wwwroot.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    file_exists(&full_path)
}

pub(super) fn file_exists(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

pub(super) fn directory_exists(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}
