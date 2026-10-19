use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    session::file::{
        ensure_session_files_dir, unique_path, unique_path_for_name, write_session_upload_path,
    },
};

pub fn handle_clipboard_paste(
    config: &AppConfig,
    id: &str,
    ignore_text: bool,
) -> Result<Option<String>> {
    handle_clipboard_paste_with(
        config,
        id,
        ignore_text,
        read_clipboard_files,
        read_clipboard_image,
        read_clipboard_text,
    )
}

// Keep OS clipboard access at the boundary so file/image persistence can be
// verified with isolated data without reading or replacing the user's clipboard.
pub(crate) fn handle_clipboard_paste_with(
    config: &AppConfig,
    id: &str,
    ignore_text: bool,
    read_files: impl FnOnce() -> Result<Option<Vec<PathBuf>>>,
    read_image: impl FnOnce() -> Result<Option<ClipboardImage>>,
    read_text: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<String>> {
    if let Some(paths) = read_files()? {
        let files_dir = ensure_session_files_dir(config, id)?;
        let mut saved_paths = Vec::with_capacity(paths.len());
        for source_path in paths {
            saved_paths.push(copy_clipboard_path_into_session(
                config,
                id,
                &source_path,
                &files_dir,
            )?);
        }

        return Ok(Some(format_saved_paths(&saved_paths)));
    }

    if let Some(image) = read_image()? {
        let files_dir = ensure_session_files_dir(config, id)?;
        let saved_path = save_clipboard_image(&files_dir, image)?;
        return Ok(Some(saved_path.display().to_string()));
    }

    if ignore_text {
        return Ok(None);
    }

    read_text()
}

/// True when `payload` is the outer terminal's own text for a clipboard file.
///
/// A file copied in Explorer puts `CF_HDROP` on the clipboard without text, so
/// the terminal's paste binding renders it as the first file's client-local
/// path (double-quoted when it contains spaces). That path names the client
/// machine's file, not the session's copy, so attach replaces the paste with
/// the session-local upload path instead of forwarding it.
pub fn paste_is_clipboard_file_path(payload: &str) -> Result<bool> {
    let Some(paths) = read_clipboard_files()? else {
        return Ok(false);
    };

    Ok(pastes_clipboard_file(payload, &paths))
}

/// Path comparison for [`paste_is_clipboard_file_path`], separated from OS
/// clipboard access so the match policy is testable without a clipboard.
fn pastes_clipboard_file(payload: &str, paths: &[PathBuf]) -> bool {
    let payload = payload.trim();
    let unquoted = payload.trim_matches('"');
    paths.iter().any(|path| {
        let path = path.to_string_lossy();
        payload == path || unquoted == path
    })
}

pub enum RemoteClipboardTransfer {
    Text(String),
    Files(Vec<RemoteClipboardFile>),
}

pub struct RemoteClipboardFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub fn collect_remote_clipboard_transfer(
    ignore_text: bool,
) -> Result<Option<RemoteClipboardTransfer>> {
    if let Some(paths) = read_clipboard_files()? {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            if path.is_dir() {
                return Err(AppError::Protocol(format!(
                    "remote clipboard directory paste is not supported yet: {}",
                    path.display()
                )));
            }

            let file_name = path.file_name().ok_or_else(|| {
                AppError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("clipboard path has no file name: {}", path.display()),
                ))
            })?;
            files.push(RemoteClipboardFile {
                name: file_name.to_string_lossy().to_string(),
                bytes: fs::read(&path)?,
            });
        }

        return Ok(Some(RemoteClipboardTransfer::Files(files)));
    }

    if let Some(image) = read_clipboard_image()? {
        return Ok(Some(RemoteClipboardTransfer::Files(vec![
            RemoteClipboardFile {
                name: format!("clipboard-image-{}.png", timestamp_millis()),
                bytes: encode_clipboard_image_png(image)?,
            },
        ])));
    }

    if ignore_text {
        return Ok(None);
    }

    Ok(read_clipboard_text()?.map(RemoteClipboardTransfer::Text))
}

fn read_clipboard_files() -> Result<Option<Vec<PathBuf>>> {
    let Some(mut clipboard) = try_open_clipboard() else {
        return Ok(None);
    };

    match clipboard.get().file_list() {
        Ok(files) => {
            let files: Vec<PathBuf> = files.into_iter().collect();
            if files.is_empty() {
                Ok(None)
            } else {
                Ok(Some(files))
            }
        }
        Err(_) => Ok(None),
    }
}

pub fn read_clipboard_text() -> Result<Option<String>> {
    let Some(mut clipboard) = try_open_clipboard() else {
        return Ok(None);
    };

    match clipboard.get_text() {
        Ok(text) if !text.is_empty() => Ok(Some(text)),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

fn read_clipboard_image() -> Result<Option<ClipboardImage>> {
    let Some(mut clipboard) = try_open_clipboard() else {
        return Ok(None);
    };

    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };

    if image.width == 0 || image.height == 0 || image.bytes.is_empty() {
        return Ok(None);
    }

    let width = u32::try_from(image.width).map_err(|_| {
        AppError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard image width exceeds supported size",
        ))
    })?;
    let height = u32::try_from(image.height).map_err(|_| {
        AppError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard image height exceeds supported size",
        ))
    })?;

    Ok(Some(ClipboardImage {
        width,
        height,
        bytes: image.bytes.into_owned(),
    }))
}

fn try_open_clipboard() -> Option<arboard::Clipboard> {
    arboard::Clipboard::new().ok()
}

fn format_saved_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn save_clipboard_image(files_dir: &Path, image: ClipboardImage) -> Result<PathBuf> {
    let file_path = unique_path(files_dir, "clipboard-image", "png");
    fs::write(&file_path, encode_clipboard_image_png(image)?)?;

    Ok(file_path)
}

fn copy_clipboard_path_into_session(
    config: &AppConfig,
    id: &str,
    source_path: &Path,
    files_dir: &Path,
) -> Result<PathBuf> {
    let file_name = source_path.file_name().ok_or_else(|| {
        AppError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("clipboard path has no file name: {}", source_path.display()),
        ))
    })?;

    if source_path.is_dir() {
        let target_path = unique_path_for_name(files_dir, file_name);
        copy_dir_recursive(source_path, &target_path)?;
        return Ok(target_path);
    }

    write_session_upload_path(
        config,
        id,
        Path::new(file_name),
        &fs::read(source_path)?,
        true,
    )
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let entry_path = entry.path();
        let target_path = target.join(entry.file_name());

        // Skip symbolic links to prevent copying unintended files (e.g. a
        // symlink pointing at /etc/shadow).
        let ft = entry_path.symlink_metadata()?.file_type();
        if ft.is_symlink() {
            continue;
        }

        if ft.is_dir() {
            copy_dir_recursive(&entry_path, &target_path)?;
        } else {
            fs::copy(&entry_path, &target_path)?;
        }
    }

    Ok(())
}

fn encode_clipboard_image_png(image: ClipboardImage) -> Result<Vec<u8>> {
    let Some(image_buffer) = image::RgbaImage::from_raw(image.width, image.height, image.bytes)
    else {
        return Err(AppError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "clipboard image buffer did not match reported dimensions",
        )));
    };

    let mut png_bytes = Vec::new();
    image_buffer
        .write_to(
            &mut io::Cursor::new(&mut png_bytes),
            image::ImageFormat::Png,
        )
        .map_err(|err| {
            AppError::Io(io::Error::other(format!(
                "failed to save clipboard image: {err}"
            )))
        })?;
    Ok(png_bytes)
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub(crate) struct ClipboardImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_saved_paths_joins_with_newlines() {
        let paths = vec![PathBuf::from("one.txt"), PathBuf::from("two.txt")];
        assert_eq!(format_saved_paths(&paths), "one.txt\ntwo.txt");
    }

    #[test]
    fn test_pastes_clipboard_file_matches_terminal_path_rendering() {
        let paths = vec![
            PathBuf::from(r"C:\Users\me\screenshot.png"),
            PathBuf::from(r"C:\Program Files\report.txt"),
        ];
        assert!(pastes_clipboard_file(r"C:\Users\me\screenshot.png", &paths));
        assert!(pastes_clipboard_file(
            "\"C:\\Program Files\\report.txt\"",
            &paths
        ));
        assert!(pastes_clipboard_file(
            "  C:\\Users\\me\\screenshot.png \r\n",
            &paths
        ));
        assert!(!pastes_clipboard_file(r"C:\Users\me\other.png", &paths));
        assert!(!pastes_clipboard_file("", &paths));
        assert!(!pastes_clipboard_file(r"C:\Users\me\screenshot.png", &[]));
    }
}
