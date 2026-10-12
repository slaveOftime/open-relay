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
    /// Canonical identity public key of the primary, in the form
    /// `ed25519 <base64(raw32)>`. The connector pins the primary to
    /// exactly this identifier; the trust root lives here, not on the wire.
    pub primary_identity_pub_key: String,
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
/// identity key directly from `<state>/identity_key` at connect time —
/// we don't need to track its private material in the persisted config.
pub fn build_join_config(
    url: String,
    name: String,
    primary_identity_pub_key: Option<String>,
) -> Result<JoinConfig> {
    let raw = primary_identity_pub_key
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Protocol(
                "missing --identity-pub-key <PRIMARY_PUB_KEY> - the operator-pinned identity \
                 of the primary is the trust root for the join"
                    .into(),
            )
        })?;
    let pk = crate::federation_crypto::parse_public_key(&raw).map_err(|e| {
        AppError::Protocol(format!(
            "--identity-pub-key: {e} \
             (expected the canonical \"ed25519 <base64>\" line from `oly daemon status` on the primary)"
        ))
    })?;
    Ok(JoinConfig {
        name,
        primary_url: url,
        primary_identity_pub_key: pk.to_line(),
    })
}

/// `oly join start` — persist config and signal the local daemon to connect.
pub async fn run_join(
    config: &AppConfig,
    url: String,
    name: String,
    primary_identity_pub_key: Option<String>,
) -> Result<()> {
    let join = build_join_config(url.clone(), name.clone(), primary_identity_pub_key.clone())?;
    // Surface the secondary's own identity pub key so the operator can copy
    // it onto the primary (`oly node accept --name NAME --identity-pub-key
    // ...`). The daemon generates this on first start; read it from disk
    // so the hint is correct even if the daemon has never been started —
    // the connector won't be able to join in that state, but the CLI should
    // still surface the canonical line.
    match crate::http::NodeIdentity::read_published_pubkey(&config.paths.state_dir) {
        Ok(Some(pub_key)) => {
            println!(
                "Register it on the primary node with `oly node accept --name {name} --identity-pub-key \"{pub_key}\"`"
            );
        }
        Ok(None) => {
            return Err(AppError::Protocol(format!(
                "identity join requires the daemon's identity pubkey at {}/identity_key.pub, \
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
    save_join_config(config, &join)?;

    match ipc::send_request_checked(
        config,
        crate::protocol::RpcRequest::JoinStart {
            url,
            name: name.clone(),
            primary_identity_pub_key: join.primary_identity_pub_key.clone(),
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

    // Build a canonical `ed25519 <base64(raw32)>` pub-key line that
    // passes `federation_crypto::parse_public_key`. The signing key is
    // generated fresh each test run; only the canonical-form check
    // matters here.
    fn valid_primary_pub_line() -> String {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let sk = ed25519_dalek::SigningKey::from_bytes(&bytes);
        crate::federation_crypto::public_key_line(&sk)
    }

    #[test]
    fn build_join_config_requires_identity_pub_key() {
        let err = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            None,
        )
        .expect_err("must fail without --identity-pub-key");
        assert!(format!("{err}").contains("--identity-pub-key"));
    }

    #[test]
    fn build_join_config_round_trips_canonical_line() {
        let primary_pub = valid_primary_pub_line();
        let cfg = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some(primary_pub.clone()),
        )
        .expect("identity-pub-key joins succeed");
        assert!(cfg.primary_identity_pub_key.starts_with("ed25519 "));
        // The persisted value is the canonical line: parsing it round-trips.
        let parsed = crate::federation_crypto::parse_public_key(&cfg.primary_identity_pub_key)
            .expect("persisted value parses");
        assert_eq!(parsed.to_line(), cfg.primary_identity_pub_key);
    }

    #[test]
    fn build_join_config_rejects_legacy_ssh_ed25519_lines() {
        // The previous (now removed) format used `ssh-ed25519 ...`. The
        // new canonical form must reject it.
        let err = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some(
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHGWpslEDaC/jIdGIrRog0u6edoFj9W9J2kqPG+Sx/jL"
                    .to_string(),
            ),
        )
        .expect_err("legacy ssh-ed25519 line is rejected");
        assert!(format!("{err}").contains("ed25519"));
    }

    #[test]
    fn build_join_config_rejects_garbage_payload() {
        let err = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some("ed25519 !!!notbase64".into()),
        )
        .expect_err("invalid base64 must fail");
        assert!(format!("{err}").contains("--identity-pub-key"));
    }

    #[test]
    fn build_join_config_treats_empty_string_as_missing() {
        let err = build_join_config(
            "http://primary:15443".into(),
            "worker1".into(),
            Some(String::new()),
        )
        .expect_err("empty string is treated as missing");
        assert!(format!("{err}").contains("--identity-pub-key"));
    }
}

