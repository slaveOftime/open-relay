use serde::{Deserialize, Serialize};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{JoinSummary, RpcResponse},
};

// ---------------------------------------------------------------------------
// Persisted join configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinConfig {
    pub name: String,
    pub primary_url: String,
    /// Plaintext API key — stored with user-private permissions on the secondary.
    pub api_key: String,
}

fn joins_path(config: &AppConfig) -> std::path::PathBuf {
    config.state_dir.join("joins.json")
}

pub fn load_join_configs(config: &AppConfig) -> Vec<JoinConfig> {
    let path = joins_path(config);
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_join_config(config: &AppConfig, join: &JoinConfig) -> Result<()> {
    let path = joins_path(config);
    let mut joins = load_join_configs(config);
    joins.retain(|j| j.name != join.name);
    joins.push(join.clone());
    let data = serde_json::to_string_pretty(&joins)?;
    std::fs::write(&path, data)?;
    // Set file permissions to user-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn remove_join_config(config: &AppConfig, name: &str) -> bool {
    let path = joins_path(config);
    let mut joins = load_join_configs(config);
    let before = joins.len();
    joins.retain(|j| j.name != name);
    if joins.len() < before {
        let data = serde_json::to_string_pretty(&joins).unwrap_or_default();
        let _ = std::fs::write(&path, data);
        true
    } else {
        false
    }
}

pub fn list_join_summaries(config: &AppConfig) -> Vec<JoinSummary> {
    load_join_configs(config)
        .into_iter()
        .map(|j| JoinSummary {
            name: j.name,
            primary_url: j.primary_url,
            connected: false, // live status is known only inside the daemon
        })
        .collect()
}

// ---------------------------------------------------------------------------
// CLI handlers
// ---------------------------------------------------------------------------

/// `oly join start` — persist config and signal the local daemon to connect.
pub async fn run_join(config: &AppConfig, url: String, name: String, key: String) -> Result<()> {
    let join = JoinConfig {
        name: name.clone(),
        primary_url: url.clone(),
        api_key: key.clone(),
    };
    save_join_config(config, &join)?;

    match ipc::send_request(
        config,
        crate::protocol::RpcRequest::JoinStart {
            url,
            name: name.clone(),
            key,
        },
    )
    .await
    {
        Ok(RpcResponse::Ack) => {
            println!(
                "Joining primary as \"{name}\". Use `oly join stop --name {name}` to disconnect."
            );
            Ok(())
        }
        Ok(RpcResponse::Error { message }) => Err(AppError::DaemonUnavailable(message)),
        Err(AppError::DaemonUnavailable(_)) => {
            // Daemon not running — config is saved; will connect on next daemon start.
            println!(
                "Saved join config for \"{name}\". \
                 Start the daemon with `oly daemon start` to connect automatically."
            );
            Ok(())
        }
        _ => Err(AppError::Protocol("unexpected response".into())),
    }
}

/// `oly join stop` — remove persisted config and signal the local daemon to disconnect.
pub async fn run_join_stop(config: &AppConfig, name: String) -> Result<()> {
    let removed = remove_join_config(config, &name);
    if !removed {
        eprintln!("warning: no saved join config found for \"{name}\"");
    }

    match ipc::send_request(
        config,
        crate::protocol::RpcRequest::JoinStop { name: name.clone() },
    )
    .await
    {
        Ok(RpcResponse::Ack) | Ok(RpcResponse::Error { .. }) => {}
        Err(_) => {} // Daemon not running is fine — config already removed.
        _ => {}
    }

    println!("Stopped join for \"{name}\".");
    Ok(())
}
