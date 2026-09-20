use serde::{Deserialize, Serialize};
use ed25519_dalek::Signer as _;

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

/// Load an SSH private key from the given path and derive the public key.
/// Returns (public_key_in_openssh_format, key_blob_for_signing).
pub fn load_ssh_key(path: &std::path::Path) -> Result<String> {
    // Read the private key file.
    // SSH private keys can be in various formats:
    // - OpenSSH new format (starts with "-----BEGIN OPENSSH PRIVATE KEY-----")
    // - PEM/PKCS#8 format (starts with "-----BEGIN PRIVATE KEY-----")
    // - PKCS#1 RSA format (starts with "-----BEGIN RSA PRIVATE KEY-----")
    //
    // For Ed25519 keys, we can extract the public key from the private key.
    // The ssh-key crate handles parsing; we use it to extract the public key.
    let key_data = std::fs::read(path)
        .map_err(|e| AppError::Protocol(format!("failed to read SSH key: {e}")))?;

    let key_str = std::str::from_utf8(&key_data)
        .map_err(|e| AppError::Protocol(format!("invalid UTF-8 in SSH key: {e}")))?;

    // Parse as OpenSSH private key to extract the public key
    let ssh_key = ssh_key::PrivateKey::from_openssh(key_str)
        .map_err(|e| AppError::Protocol(format!("failed to parse SSH key: {e}")))?;

    let public_key = ssh_key.public_key();
    let key_type = public_key.algorithm().as_str().to_string();
    let public_bytes = public_key.to_bytes().map_err(|e| AppError::Protocol(format!("failed to serialize public key: {e}")))?;

    let public_key_str = format!("{} {}", key_type, base64::encode(public_bytes));

    Ok(public_key_str)
}

/// Verify that a known_hosts entry matches the given host key.
/// Returns true if the host key is found and matches.
pub fn verify_known_hosts(known_hosts_path: &std::path::Path, host: &str, key: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(known_hosts_path) else {
        return false;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // known_hosts format: hostnames key-type base64key
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[0].contains(host) {
            if format!("{} {}", parts[1], parts.get(2).unwrap_or(&"")) == key {
                return true;
            }
        }
    }
    false
}

/// Append a host key to the known_hosts file.
pub fn append_known_hosts(
    known_hosts_path: &std::path::Path,
    host: &str,
    key: &str,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(known_hosts_path)?;
    writeln!(file, "{} {}", host, key)?;
    Ok(())
}



/// Sign data with an SSH private key and return the SSH-format signature.
/// The signature is in SSH wire format: base64(algorithm_name || signature_blob).
/// Sign data with an SSH private key using ed25519-dalek.
/// Returns the SSH-format signature (base64-encoded SSH wire format).
pub fn sign_ssh_data(path: &std::path::Path, data: &[u8]) -> Result<String> {
    let key_data = std::fs::read(path)
        .map_err(|e| AppError::Protocol(format!("failed to read SSH key: {e}")))?;

    let key_str = std::str::from_utf8(&key_data)
        .map_err(|e| AppError::Protocol(format!("invalid UTF-8 in SSH key: {e}")))?;

    // Parse as OpenSSH private key
    let ssh_key = ssh_key::PrivateKey::from_openssh(key_str)
        .map_err(|e| AppError::Protocol(format!("failed to parse SSH key: {e}")))?;

    // Extract the Ed25519 keypair from the SSH key structure
    let keypair_data = ssh_key.key_data();
    let ed_keypair = match keypair_data {
        ssh_key::private::KeypairData::Ed25519(ed) => ed,
        _ => return Err(AppError::Protocol("only Ed25519 keys are supported".into())),
    };

    // Extract the 32-byte private key seed from the Ed25519PrivateKey
    let priv_bytes = ed_keypair.private.to_bytes();

    // Create ed25519-dalek signing key from the raw bytes
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&priv_bytes);

    // Sign the data
    let signature = signing_key.sign(data);

    // Return the raw 64-byte signature, base64-encoded
    // The verification function accepts both raw and SSH wire format
    Ok(base64::encode(signature.to_bytes()))
}

/// Fetch the SSH host key from the primary over HTTP.
/// Returns the public key string (e.g. "ssh-ed25519 AAAA...").
pub async fn fetch_host_key(url: &str) -> Result<String> {
    let host = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| AppError::Protocol("invalid URL".into()))?;
    let host_key_url = format!("http://{}/api/nodes/host-key", host);

    let body = reqwest::get(&host_key_url)
        .await
        .map_err(|e| AppError::Protocol(format!("failed to fetch host key: {e}")))?
        .text()
        .await
        .map_err(|e| AppError::Protocol(format!("failed to read host key response: {e}")))?;

    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| AppError::Protocol(format!("failed to parse host key response: {e}")))?;

    json.get("public_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AppError::Protocol("host key not found in response".into()))
}

/// Verify the fetched host key against known_hosts.
/// Returns true if the key matches, or if known_hosts is not set (TOFU mode).
pub fn check_host_key(
    known_hosts_path: Option<&str>,
    host: &str,
    fetched_key: &str,
) -> bool {
    let Some(path) = known_hosts_path else {
        // No known_hosts configured — accept the key (TOFU)
        return true;
    };

    let path = std::path::Path::new(path);
    verify_known_hosts(path, host, fetched_key)
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
pub async fn run_join(
    config: &AppConfig,
    url: String,
    name: String,
    key: Option<String>,
    ssh_key_path: Option<String>,
    ssh_known_hosts: Option<String>,
) -> Result<()> {
    let ssh_public_key = ssh_key_path
        .as_ref()
        .and_then(|p| load_ssh_key(std::path::Path::new(p)).ok());

    let join = JoinConfig {
        name: name.clone(),
        primary_url: url.clone(),
        api_key: key.clone(),
        ssh_key_path: ssh_key_path.clone(),
        ssh_public_key,
        ssh_known_hosts: ssh_known_hosts.clone(),
    };
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
