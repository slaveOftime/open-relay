//! SSH Ed25519 authentication helpers for the node join protocol.
//!
//! # Key format
//!
//! The canonical representation used on the wire and in the database is
//! `"ssh-ed25519 <base64(raw 32-byte public key)>"`. Registration also
//! accepts real OpenSSH public key lines (base64 of SSH wire format, as
//! printed by `ssh-keygen`) and normalizes them via
//! [`normalize_public_key`].
//!
//! # Challenge-response
//!
//! All signatures are raw 64-byte Ed25519 signatures, base64-encoded.
//! The signed payloads are domain-separated:
//!
//! - Primary → Secondary (`host_key` reply):
//!   `sign("oly-host-challenge-v1" || nonce)` — the primary proves it owns
//!   the host key *on this very connection* and issues the challenge.
//! - Secondary → Primary (`join` with SSH auth):
//!   `sign("oly-node-join-v1" || name || nonce || public_key)` — binds the
//!   response to the protocol, the claimed node name, the primary's fresh
//!   per-connection nonce (no replay), and the signer's key (no key
//!   substitution).

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::Signer as _;

use crate::error::{AppError, Result};

/// Domain separation for the primary's host-key challenge signature.
pub const HOST_CHALLENGE_CONTEXT: &[u8] = b"oly-host-challenge-v1";
/// Domain separation for the secondary's join signature.
pub const NODE_JOIN_CONTEXT: &[u8] = b"oly-node-join-v1";

/// Length in bytes of the join challenge nonce.
pub const NONCE_LEN: usize = 32;

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>> {
    B64.decode(s.trim())
        .map_err(|e| AppError::Protocol(format!("invalid base64: {e}")))
}

/// Load an unencrypted Ed25519 SSH private key from an OpenSSH key file.
pub fn load_signing_key(path: &std::path::Path) -> Result<ed25519_dalek::SigningKey> {
    let data = std::fs::read(path).map_err(|e| {
        AppError::Protocol(format!("failed to read SSH key {}: {e}", path.display()))
    })?;
    let text = std::str::from_utf8(&data).map_err(|_| {
        AppError::Protocol(format!("SSH key {} is not valid UTF-8", path.display()))
    })?;
    let private = ssh_key::PrivateKey::from_openssh(text).map_err(|e| {
        AppError::Protocol(format!(
            "failed to parse SSH key {} (only unencrypted OpenSSH-format keys are supported): {e}",
            path.display()
        ))
    })?;
    match private.key_data() {
        ssh_key::private::KeypairData::Ed25519(kp) => Ok(ed25519_dalek::SigningKey::from_bytes(
            &kp.private.to_bytes(),
        )),
        _ => Err(AppError::Protocol(format!(
            "SSH key {}: only Ed25519 keys are supported",
            path.display()
        ))),
    }
}

/// Canonical public key line derived from a signing key.
pub fn public_key_line(signing_key: &ed25519_dalek::SigningKey) -> String {
    format!(
        "ssh-ed25519 {}",
        b64_encode(signing_key.verifying_key().as_bytes())
    )
}

/// Normalize any accepted public key representation to the canonical
/// internal form `"ssh-ed25519 <base64(raw32)>"`.
///
/// Accepts both real OpenSSH public key lines (`ssh-keygen` output, base64
/// of SSH wire format) and lines already in canonical form. Only Ed25519
/// keys are supported.
pub fn normalize_public_key(input: &str) -> Result<String> {
    let trimmed = input.trim();

    // Real OpenSSH wire-format line, e.g. from `cat ~/.ssh/id_ed25519.pub`.
    if let Ok(pk) = ssh_key::PublicKey::from_openssh(trimmed) {
        if let Some(ed) = pk.key_data().ed25519() {
            return Ok(format!("ssh-ed25519 {}", b64_encode(&ed.0)));
        }
        return Err(AppError::Protocol(format!(
            "unsupported SSH key type '{}' (only ssh-ed25519 is supported)",
            pk.algorithm().as_str()
        )));
    }

    // Canonical internal form produced by this tool.
    let mut parts = trimmed.split_whitespace();
    if let (Some("ssh-ed25519"), Some(encoded), None) = (parts.next(), parts.next(), parts.next())
        && let Ok(bytes) = b64_decode(encoded)
        && let Ok(raw) = <[u8; 32]>::try_from(bytes.as_slice())
    {
        return Ok(public_key_line_from_raw(&raw));
    }

    Err(AppError::Protocol(
        "invalid SSH public key: expected an OpenSSH ssh-ed25519 key line".into(),
    ))
}

fn public_key_line_from_raw(raw: &[u8; 32]) -> String {
    format!("ssh-ed25519 {}", b64_encode(raw))
}

/// Build the payload the primary signs to prove host-key ownership and
/// issue the join challenge.
pub fn host_challenge_payload(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(HOST_CHALLENGE_CONTEXT.len() + NONCE_LEN);
    payload.extend_from_slice(HOST_CHALLENGE_CONTEXT);
    payload.extend_from_slice(nonce);
    payload
}

/// Build the payload the secondary signs to authenticate a join.
pub fn node_join_payload(name: &str, nonce: &[u8; NONCE_LEN], public_key: &str) -> Vec<u8> {
    let mut payload =
        Vec::with_capacity(NODE_JOIN_CONTEXT.len() + name.len() + NONCE_LEN + public_key.len());
    payload.extend_from_slice(NODE_JOIN_CONTEXT);
    payload.extend_from_slice(name.as_bytes());
    payload.extend_from_slice(nonce);
    payload.extend_from_slice(public_key.as_bytes());
    payload
}

/// Sign `data` and return the base64-encoded raw 64-byte Ed25519 signature.
pub fn sign_b64(signing_key: &ed25519_dalek::SigningKey, data: &[u8]) -> String {
    b64_encode(&signing_key.sign(data).to_bytes())
}

/// Verify a base64 raw Ed25519 signature against a canonical or OpenSSH
/// public key line. Fails closed on any parse or verification error.
pub fn verify_signature(public_key: &str, signature_b64: &str, data: &[u8]) -> bool {
    verify_signature_deoded(
        public_key,
        &b64_decode(signature_b64).unwrap_or_default(),
        data,
    )
}

fn verify_signature_deoded(public_key: &str, signature: &[u8], data: &[u8]) -> bool {
    let Ok(canonical) = normalize_public_key(public_key) else {
        return false;
    };
    let Some(raw) = canonical
        .strip_prefix("ssh-ed25519 ")
        .and_then(|encoded| b64_decode(encoded).ok())
    else {
        return false;
    };
    let Ok(pub_arr) = <[u8; 32]>::try_from(raw.as_slice()) else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(signature) else {
        return false;
    };
    let Ok(verifying_key) = ed25519_dalek::VerifyingKey::from_bytes(&pub_arr) else {
        return false;
    };
    verifying_key
        .verify_strict(data, &ed25519_dalek::Signature::from_bytes(&sig_arr))
        .is_ok()
}

/// Outcome of looking a host up in a known_hosts file.
#[derive(Debug, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// An entry for this exact host matches the presented key.
    Match,
    /// The host has at least one entry but none matches the key.
    Mismatch,
    /// The host is not present (or the file does not exist).
    Unknown,
}

/// Look `host` up in a known_hosts file.
///
/// Host tokens are matched exactly (comma-separated lists are supported,
/// and `[host]:port` entries match the bare host). Marker lines
/// (`@cert-authority`, `@revoked`) are not supported and skipped.
pub fn lookup_known_hosts(path: &std::path::Path, host: &str, key_line: &str) -> HostKeyStatus {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HostKeyStatus::Unknown;
    };
    let key_line = key_line.trim();
    let mut host_present = false;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hosts) = parts.next() else { continue };
        if hosts.starts_with('@') {
            // Key-type marker lines are not supported.
            continue;
        }
        if !hosts
            .split(',')
            .any(|entry| host_entry_matches(entry, host))
        {
            continue;
        }
        host_present = true;
        let (Some(key_type), Some(key_b64)) = (parts.next(), parts.next()) else {
            continue;
        };
        if format!("{key_type} {key_b64}") == key_line {
            return HostKeyStatus::Match;
        }
    }
    if host_present {
        HostKeyStatus::Mismatch
    } else {
        HostKeyStatus::Unknown
    }
}

fn host_entry_matches(entry: &str, host: &str) -> bool {
    // known_hosts stores port-qualified (and IPv6) entries as "[host]:port".
    if let Some(rest) = entry.strip_prefix('[')
        && let Some((inner, _)) = rest.split_once(']')
    {
        return inner == host;
    }
    entry == host
}

/// Append a `host <key-line>` entry to a known_hosts file (TOFU pinning).
pub fn append_known_hosts(
    path: &std::path::Path,
    host: &str,
    key_line: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{host} {key_line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore as _;

    fn test_signing_key(seed: u8) -> ed25519_dalek::SigningKey {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        bytes[0] = seed; // deterministic-ish but distinct per seed
        ed25519_dalek::SigningKey::from_bytes(&bytes)
    }

    /// Serialize a dalek signing key as an OpenSSH private key file. Built
    /// from raw bytes because the ssh-key crate's dalek conversions are
    /// feature-gated behind a different dalek major version.
    fn openssh_private_key_pem(sk: &ed25519_dalek::SigningKey) -> String {
        let keypair = ssh_key::private::Ed25519Keypair {
            private: ssh_key::private::Ed25519PrivateKey::from_bytes(&sk.to_bytes()),
            public: ssh_key::public::Ed25519PublicKey(*sk.verifying_key().as_bytes()),
        };
        let private =
            ssh_key::PrivateKey::new(ssh_key::private::KeypairData::Ed25519(keypair), "test")
                .expect("build private key");
        (*private
            .to_openssh(ssh_key::LineEnding::LF)
            .expect("to_openssh"))
        .clone()
    }

    fn write_temp(file: &std::path::Path, content: &str) {
        std::fs::write(file, content).expect("write temp file");
    }

    #[test]
    fn sign_verify_round_trip() {
        let sk = test_signing_key(1);
        let payload = node_join_payload("worker-a", &[7u8; 32], &public_key_line(&sk));
        let sig = sign_b64(&sk, &payload);
        assert!(verify_signature(&public_key_line(&sk), &sig, &payload));
    }

    #[test]
    fn verify_rejects_wrong_key_tampered_payload_and_names() {
        let sk = test_signing_key(2);
        let other = test_signing_key(3);
        let nonce = [9u8; 32];
        let payload = node_join_payload("worker-a", &nonce, &public_key_line(&sk));
        let sig = sign_b64(&sk, &payload);

        // Wrong public key.
        assert!(!verify_signature(&public_key_line(&other), &sig, &payload));
        // Tampered payload (name substituted).
        let tampered = node_join_payload("victim", &nonce, &public_key_line(&sk));
        assert!(!verify_signature(&public_key_line(&sk), &sig, &tampered));
        // Tampered signature bytes.
        let mut sig_bytes = b64_decode(&sig).expect("sig decodes");
        sig_bytes[0] ^= 0xff;
        assert!(!verify_signature_deoded(
            &public_key_line(&sk),
            &sig_bytes,
            &payload
        ));
        // Truncated signature.
        assert!(!verify_signature(&public_key_line(&sk), "AAAA", &payload));
    }

    #[test]
    fn normalize_accepts_real_openssh_line_and_canonical_form() {
        let sk = test_signing_key(4);
        let pem = openssh_private_key_pem(&sk);
        let openssh_line = ssh_key::PrivateKey::from_openssh(&pem)
            .expect("parse test pem")
            .public_key()
            .to_openssh()
            .expect("openssh public key");

        let canonical = public_key_line(&sk);
        assert_eq!(normalize_public_key(&openssh_line).unwrap(), canonical);
        assert_eq!(normalize_public_key(&canonical).unwrap(), canonical);
        // Whitespace is tolerated.
        assert_eq!(
            normalize_public_key(&format!("  {canonical}\n")).unwrap(),
            canonical
        );
    }

    #[test]
    fn normalize_rejects_garbage_and_bad_lengths() {
        assert!(normalize_public_key("not a key").is_err());
        assert!(normalize_public_key("ssh-rsa AAAAB3Nz").is_err());
        // Valid base64 but wrong length.
        assert!(normalize_public_key(&format!("ssh-ed25519 {}", b64_encode(&[0u8; 31]))).is_err());
        assert!(normalize_public_key("").is_err());
    }

    #[test]
    fn load_signing_key_round_trips_through_file() {
        let sk = test_signing_key(5);
        let pem = openssh_private_key_pem(&sk);

        let dir = std::env::temp_dir().join(format!("oly_sshauth_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("id_ed25519");
        write_temp(&path, &pem);

        let loaded = load_signing_key(&path).expect("load key");
        assert_eq!(loaded.verifying_key(), sk.verifying_key());
        assert_eq!(public_key_line(&loaded), public_key_line(&sk));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_hosts_exact_host_matching() {
        let dir = std::env::temp_dir().join(format!("oly_sshauth_kh_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("known_hosts");

        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGdummykey";
        std::fs::write(
            &path,
            format!(
                "# comment\nprimary.example.com {key}\n[10.0.0.5]:2222 {key}\n@cert-authority marked.example.com {key}\n"
            ),
        )
        .expect("write known_hosts");

        assert_eq!(
            lookup_known_hosts(&path, "primary.example.com", key),
            HostKeyStatus::Match
        );
        // Port-qualified entries match the bare host.
        assert_eq!(
            lookup_known_hosts(&path, "10.0.0.5", key),
            HostKeyStatus::Match
        );
        // Marker lines are skipped: host counts as unknown, not mismatch.
        assert_eq!(
            lookup_known_hosts(&path, "marked.example.com", key),
            HostKeyStatus::Unknown
        );
        // A different key for a known host is a mismatch (possible MITM).
        assert_eq!(
            lookup_known_hosts(&path, "primary.example.com", "ssh-ed25519 BBBB"),
            HostKeyStatus::Mismatch
        );
        // Substring superframes of pinned hosts must NOT match.
        assert_eq!(
            lookup_known_hosts(&path, "evil-primary.example.com", key),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            lookup_known_hosts(&path, "notprimary.example.com", key),
            HostKeyStatus::Unknown
        );
        // Unknown host.
        assert_eq!(
            lookup_known_hosts(&path, "fresh.example.com", key),
            HostKeyStatus::Unknown
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_known_hosts_enables_match() {
        let dir = std::env::temp_dir().join(format!("oly_sshauth_tofu_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("known_hosts");
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            lookup_known_hosts(&path, "p.example", "ssh-ed25519 XXXX"),
            HostKeyStatus::Unknown
        );
        append_known_hosts(&path, "p.example", "ssh-ed25519 XXXX").expect("append");
        assert_eq!(
            lookup_known_hosts(&path, "p.example", "ssh-ed25519 XXXX"),
            HostKeyStatus::Match
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_handshake_payloads_are_domain_separated() {
        let host_sk = test_signing_key(6);
        let node_sk = test_signing_key(7);
        let nonce = [42u8; 32];

        let host_payload = host_challenge_payload(&nonce);
        let host_sig = sign_b64(&host_sk, &host_payload);
        assert!(verify_signature(
            &public_key_line(&host_sk),
            &host_sig,
            &host_payload
        ));

        let canonical = public_key_line(&node_sk);
        let join_payload = node_join_payload("worker-a", &nonce, &canonical);
        let join_sig = sign_b64(&node_sk, &join_payload);
        assert!(verify_signature(&canonical, &join_sig, &join_payload));

        // The host challenge signature must not be accepted as a join
        // signature and vice versa (domain separation via context prefix).
        assert!(!verify_signature(
            &public_key_line(&host_sk),
            &host_sig,
            &join_payload
        ));
        assert!(!verify_signature(&canonical, &join_sig, &host_payload));
    }
}
