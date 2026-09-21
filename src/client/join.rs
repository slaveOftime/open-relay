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
    pub api_key: Option<String>,
    /// SSH key authentication fields (optional, mutually exclusive with api_key).
    pub ssh_key_path: Option<String>,
    pub ssh_public_key: Option<String>,
    pub ssh_known_hosts: Option<String>,
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

/// Validate join authentication arguments and derive the canonical SSH
/// public key line from the private key (if SSH auth is used). Shared by
/// the CLI (`oly join start`) and the daemon (`join_start` RPC) so both
/// fail identically on bad input.
pub fn build_join_config(
    url: String,
    name: String,
    key: Option<String>,
    ssh_key_path: Option<String>,
    ssh_known_hosts: Option<String>,
) -> Result<JoinConfig> {
    if key.is_none() && ssh_key_path.is_none() {
        return Err(AppError::Protocol(
            "authentication required: pass --key <API_KEY> or --ssh-key <PATH>".into(),
        ));
    }
    let ssh_public_key = match &ssh_key_path {
        Some(path) => {
            // Fail up-front (with a clear message) if the key cannot be loaded.
            let signing_key = crate::sshauth::load_signing_key(std::path::Path::new(path))?;
            Some(crate::sshauth::public_key_line(&signing_key))
        }
        None => None,
    };
    Ok(JoinConfig {
        name,
        primary_url: url,
        api_key: key,
        ssh_key_path,
        ssh_public_key,
        ssh_known_hosts,
    })
}

/// `oly join start` — persist config and signal the local daemon to connect.
pub async fn run_join(
    config: &AppConfig,
    url: String,
    name: String,
    key: Option<String>,
    ssh_key_path: Option<String>,
    ssh_known_hosts: Option<String>,
) -> Result<()> {
    let join = build_join_config(
        url.clone(),
        name.clone(),
        key.clone(),
        ssh_key_path.clone(),
        ssh_known_hosts.clone(),
    )?;
    if let Some(public_key) = &join.ssh_public_key {
        println!(
            "Node SSH public key\n(register it on the primary with `oly node accept-ssh-pubkey -n {name} -k \"<KEY>\"`):\n{public_key}"
        );
    }
    save_join_config(config, &join)?;

    match ipc::send_request_checked(
        config,
        crate::protocol::RpcRequest::JoinStart {
            url,
            name: name.clone(),
            key,
            ssh_key_path,
            ssh_known_hosts,
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
        Err(AppError::DaemonUnavailable(_)) => {
            // Daemon not running — config is saved; will connect on next daemon start.
            println!(
                "Saved join config for \"{name}\". \
                 Start the daemon with `oly daemon start` to connect automatically."
            );
            Ok(())
        }
        Err(err) => Err(err),
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
