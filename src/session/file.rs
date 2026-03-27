use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
};

fn normalize_session_upload_relative_path_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if normalized.as_os_str().is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(crate) fn ensure_session_files_dir(config: &AppConfig, id: &str) -> Result<PathBuf> {
    let files_dir = config.sessions_dir.join(id).join("files");
    fs::create_dir_all(&files_dir)?;
    Ok(files_dir)
}

pub(crate) fn normalize_session_upload_relative_path(path: &str) -> Option<PathBuf> {
    normalize_session_upload_relative_path_path(Path::new(path.trim()))
}

pub(crate) fn write_session_upload(
    config: &AppConfig,
    id: &str,
    relative_path: &str,
    bytes: &[u8],
    dedupe: bool,
) -> Result<PathBuf> {
    write_session_upload_path(config, id, Path::new(relative_path.trim()), bytes, dedupe)
}

pub(crate) fn write_session_upload_path(
    config: &AppConfig,
    id: &str,
    relative_path: &Path,
    bytes: &[u8],
    dedupe: bool,
) -> Result<PathBuf> {
    let Some(relative_path) = normalize_session_upload_relative_path_path(relative_path) else {
        return Err(AppError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid upload path",
        )));
    };

    let upload_root = ensure_session_files_dir(config, id)?;
    let target_path = resolve_session_upload_target(&upload_root, &relative_path, dedupe)?;
    let Some(parent) = target_path.parent() else {
        return Err(AppError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid upload path",
        )));
    };

    fs::create_dir_all(parent)?;
    fs::write(&target_path, bytes)?;
    Ok(target_path)
}

pub(crate) fn unique_path(files_dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    unique_path_for_name(files_dir, format!("{stem}-{timestamp}.{extension}"))
}

pub(crate) fn unique_path_for_name(files_dir: &Path, file_name: impl AsRef<Path>) -> PathBuf {
    let file_name = file_name.as_ref();
    let stem = file_name
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("clipboard");
    let extension = file_name.extension().and_then(|value| value.to_str());

    let mut candidate = match extension {
        Some(extension) => files_dir.join(format!("{stem}.{extension}")),
        None => files_dir.join(stem),
    };
    let mut suffix = 1usize;

    while candidate.exists() {
        candidate = match extension {
            Some(extension) => files_dir.join(format!("{stem}-{suffix}.{extension}")),
            None => files_dir.join(format!("{stem}-{suffix}")),
        };
        suffix += 1;
    }

    candidate
}

fn resolve_session_upload_target(
    upload_root: &Path,
    relative_path: &Path,
    dedupe: bool,
) -> Result<PathBuf> {
    if !dedupe {
        return Ok(upload_root.join(relative_path));
    }

    let parent_relative = relative_path.parent().unwrap_or_else(|| Path::new(""));
    let target_parent = upload_root.join(parent_relative);
    fs::create_dir_all(&target_parent)?;

    let file_name = relative_path.file_name().ok_or_else(|| {
        AppError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("upload path has no file name: {}", relative_path.display()),
        ))
    })?;

    Ok(unique_path_for_name(&target_parent, file_name))
}

#[cfg(test)]
mod tests {
    use super::normalize_session_upload_relative_path;

    #[test]
    fn normalize_upload_path_accepts_nested_relative_path() {
        let path =
            normalize_session_upload_relative_path("subdir/file.txt").expect("path should parse");
        assert_eq!(path.to_string_lossy().replace('\\', "/"), "subdir/file.txt");
    }

    #[test]
    fn normalize_upload_path_rejects_parent_traversal() {
        assert!(normalize_session_upload_relative_path("../file.txt").is_none());
    }
}
