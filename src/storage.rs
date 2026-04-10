use std::{
    env,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub no_http: bool,
    pub no_auth: bool,
    pub started_at: String,
}

pub fn write_daemon_info(path: &PathBuf, info: &DaemonInfo) -> Result<()> {
    let json = serde_json::to_string(info).map_err(|e| AppError::Protocol(e.to_string()))?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    file.write_all(json.as_bytes())?;
    file.flush()?;
    Ok(())
}

pub fn read_daemon_info(path: &PathBuf) -> Result<Option<DaemonInfo>> {
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let info: DaemonInfo =
        serde_json::from_str(&content).map_err(|e| AppError::Protocol(e.to_string()))?;
    Ok(Some(info))
}

pub fn resolve_state_dir() -> PathBuf {
    // Direct override — used by tests to guarantee full isolation.
    if let Some(dir) = env::var_os("OLY_STATE_DIR") {
        return PathBuf::from(dir);
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data).join("oly");
        }
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("AppData")
            .join("Local")
            .join("oly");
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(xdg_state_home) = env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(xdg_state_home).join("oly");
        }

        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("state")
            .join("oly");
    }

    #[cfg(target_os = "macos")]
    {
        return dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Library")
            .join("Application Support")
            .join("oly");
    }
}

pub fn ensure_state_dirs(state_dir: &PathBuf, sessions_dir: &PathBuf) -> Result<()> {
    fs::create_dir_all(state_dir)?;
    fs::create_dir_all(sessions_dir)?;
    fs::create_dir_all(state_dir.join("logs"))?;
    // Restrict the state directory to owner-only; it contains sensitive files
    // such as the SQLite database, daemon lock, config with VAPID keys, etc.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

pub fn try_acquire_daemon_lock(lock_file: &PathBuf) -> Result<fs::File> {
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(lock_file)
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::DaemonAlreadyRunning
            } else {
                AppError::Io(err)
            }
        })
}

pub fn remove_file_if_exists(path: &PathBuf) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn write_pid(lock_file: &PathBuf, pid: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(lock_file)?;
    file.write_all(pid.to_string().as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Reserved for M3 notification engine.
#[allow(dead_code)]
pub fn read_pid(lock_file: &PathBuf) -> Result<Option<u32>> {
    let mut file = match OpenOptions::new().read(true).open(lock_file) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(trimmed.parse::<u32>().ok())
}
