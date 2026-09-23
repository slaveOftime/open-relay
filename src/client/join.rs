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
    /// Mutually exclusive with `ssh_primary_pubkey`.
    pub api_key: Option<String>,
    /// Canonical SSH public key of the primary (`ssh-ed25519 <base64(raw32[]>`).
    /// When set, the connector uses SSH-key auth: it signs the join with its
    /// own auto-generated identity (loaded from `<state>/ssh_host_key`) and
    /// pins the primary's wire-exposed public key to exactly this value.
    /// Mutually exclusive with `api_key`.
    pub ssh_primary_pubkey: Option<String>,
}

fn joins_path(config: &AppConfig) -> std::path::PathBuf {
    config.paths.state_dir.join("joins.json")
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

/// Validate join authentication arguments. Shared by the CLI
/// (`oly join start`) and the daemon (`join_start` RPC) so both fail
/// identically on bad input. The connector reads the daemon's own
/// identity key directly from `<state>/ssh_host_key` at connect time —
/// we don't need to track its private material in the persisted config.
pub fn build_join_config(
    url: String,
    name: String,
    key: Option<String>,
    ssh_primary_pubkey: Option<String>,
) -> Result<JoinConfig> {
    let key_present = key.as_ref().is_some_and(|s| !s.is_empty());
    let ssh_present = ssh_primary_pubkey.as_ref().is_some_and(|s| !s.is_empty());
    if !key_present && !ssh_present {
        return Err(AppError::Protocol(
            "authentication required: pass --key <API_KEY> or --ssh-pub-key <PRIMARY_PUB>".into(),
        ));
    }
    if key_present && ssh_present {
        return Err(AppError::Protocol(
            "pass either --key <API_KEY> or --ssh-pub-key <PRIMARY_PUB>, not both".into(),
        ));
    }
    // Validate the primary's pub key up-front: the connector requires an
    // exact canonical match against the wire-exposed pub key, so reject
    // typos / wrong-key-type here instead of letting the connector fail
    // on its first attempt.
    let ssh_primary_pubkey = ssh_primary_pubkey.and_then(|raw| {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    if let Some(raw) = &ssh_primary_pubkey {
        crate::sshauth::normalize_public_key(raw).map_err(|e| {
            AppError::Protocol(format!(
                "--ssh-pub-key: {e} \
                 (expected the canonical \"ssh-ed25519 <base64>\" line from `oly daemon status` on the primary)"
            ))
        })?;
    }
    let api_key = key.and_then(|k| if k.is_empty() { None } else { Some(k) });
    Ok(JoinConfig {
        name,
        primary_url: url,
        api_key,
        ssh_primary_pubkey,
    })
}

/// `oly join start` — persist config and signal the local daemon to connect.
pub async fn run_join(
    config: &AppConfig,
    url: String,
    name: String,
    key: Option<String>,
    ssh_primary_pubkey: Option<String>,
) -> Result<()> {
    let join = build_join_config(
        url.clone(),
        name.clone(),
        key.clone(),
        ssh_primary_pubkey.clone(),
    )?;
    // In SSH-key mode, print the secondary's own identity pub key so the
    // operator can copy it onto the primary (`oly node accept -n NAME
    // -k ...`).
    // The daemon generates this at first start; read it from disk so the
    // hint is correct even if the daemon has never been started — the
    // connector won't be able to join in that state, but the CLI should
    // still surface the canonical line.
    if join.ssh_primary_pubkey.is_some() {
        match crate::http::NodeIdentity::read_published_pubkey(&config.paths.state_dir) {
            Ok(Some(pub_key)) => {
                println!(
                    "Node SSH public key\n(register it on the primary with `oly node accept -n {name} -k \"<KEY>\"`):\n{pub_key}"
                );
            }
            Ok(None) => {
                return Err(AppError::Protocol(format!(
                    "ssh-pub-key join requires the daemon's identity key at {}/ssh_host_key.pub, \
                     but no file was found. Start the daemon at least once to generate it.",
                    config.paths.state_dir.display()
                )));
            }
            Err(err) => {
                return Err(AppError::Protocol(format!(
                    "failed to read this daemon's identity pub key: {err}"
                )));
            }
        }
    }
    save_join_config(config, &join)?;

    match ipc::send_request_checked(
        config,
        crate::protocol::RpcRequest::JoinStart {
            url,
            name: name.clone(),
            key,
            ssh_primary_pubkey,
        },
    )
    .await
    {
        Ok(RpcResponse::JoinStartStatus { state, message }) => {
            match state.as_str() {
                "connected" => {
                    println!(
                        "Joined primary as \"{name}\". Use `oly join stop --name {name}` to disconnect."
                    );
                }
                "joining" => {
                    // Connector is still in its initial backoff loop; this
                    // is the case `oly join start` can't escape from inside
                    // the IPC deadline. Don't fail the command — the user is
                    // expected to consult `oly join ls` for steady state.
                    if message.is_empty() {
                        eprintln!(
                            "note: still joining primary as \"{name}\"; check `oly join ls` for status."
                        );
                    } else {
                        eprintln!("note: {message}");
                    }
                }
                "failed" => {
                    // The first attempt was rejected synchronously by the
                    // primary (or rejected by the connector's own validation
                    // of the host key). Surface it loudly on stderr — the
                    // connector keeps retrying, but if the user only ever
                    // runs the CLI they would otherwise never see it.
                    if message.is_empty() {
                        eprintln!(
                            "warning: join attempt for \"{name}\" failed; connector keeps retrying; check `oly join ls` for status."
                        );
                    } else {
                        eprintln!("warning: join attempt for \"{name}\" failed: {message}");
                        eprintln!(
                            "note: connector keeps retrying in the background; check `oly join ls` for status."
                        );
                    }
                }
                other => {
                    return Err(AppError::Protocol(format!(
                        "unexpected join state from daemon: {other}"
                    )));
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    // Build a canonical `ssh-ed25519 <base64(raw32[]>` pub key line that
    // passes `sshauth::normalize_public_key` so the validation path in
    // `build_join_config` accepts it. The signing key is generated fresh
    // each test run; only the canonical-form check matters here.
    fn valid_primary_pub_line() -> String {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let sk = ed25519_dalek::SigningKey::from_bytes(&bytes);
        crate::sshauth::public_key_line(&sk)
    }

    #[test]
    fn build_join_config_accepts_api_key_only() {
        let cfg = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some("secret".into()),
            None,
        )
        .expect("api key only is valid");
        assert_eq!(cfg.api_key.as_deref(), Some("secret"));
        assert!(cfg.ssh_primary_pubkey.is_none());
    }

    #[test]
    fn build_join_config_accepts_ssh_pub_key_only() {
        let primary_pub = valid_primary_pub_line();
        let cfg = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            None,
            Some(primary_pub.clone()),
        )
        .expect("ssh pub key only is valid");
        assert!(cfg.api_key.is_none());
        assert_eq!(
            cfg.ssh_primary_pubkey.as_deref(),
            Some(primary_pub.as_str())
        );
    }

    #[test]
    fn build_join_config_rejects_missing_both() {
        let err = build_join_config("http://primary:15443".into(), "worker1".into(), None, None)
            .expect_err("must fail without any auth");
        assert!(format!("{err}").contains("authentication required"));
    }

    #[test]
    fn build_join_config_rejects_setting_both() {
        let primary_pub = valid_primary_pub_line();
        let err = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some("secret".into()),
            Some(primary_pub),
        )
        .expect_err("must fail when both are set");
        assert!(format!("{err}").contains("not both"));
    }

    #[test]
    fn build_join_config_rejects_bad_ssh_pub_key() {
        // Wrong algorithm: ssh-rsa is not a valid ssh-ed25519 line.
        let err = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            None,
            Some("ssh-rsa AAAA".into()),
        )
        .expect_err("must reject non-ed25519 line");
        assert!(format!("{err}").contains("--ssh-pub-key"));
    }

    #[test]
    fn build_join_config_treats_empty_strings_as_missing() {
        let primary_pub = valid_primary_pub_line();
        let cfg = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some(String::new()),
            Some(primary_pub.clone()),
        )
        .expect("empty --key falls back to ssh");
        assert!(cfg.api_key.is_none());
        assert_eq!(
            cfg.ssh_primary_pubkey.as_deref(),
            Some(primary_pub.as_str())
        );
    }
}
