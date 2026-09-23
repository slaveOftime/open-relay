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
//! - Secondary → Primary (`join` with SSH auth):
//!   `sign("oly-node-join-v1" || name || public_key)` — binds the response
//!   to the protocol version, the claimed node name, and the signer's key.
//!   There is no in-band host-key exchange; the primary's identity is the
//!   pubkey the operator pinned via `oly join start --ssh-pub-key ...`,
//!   which is the trust root of the entire flow.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::Signer as _;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

/// AES-GCM AEAD instantiations of this module.
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};

use crate::error::{AppError, Result};

/// Domain separation for the secondary's join signature.
pub const NODE_JOIN_CONTEXT: &[u8] = b"oly-node-join-v1";

/// Length in bytes of the AEAD nonce for sealed frames.
pub const AEAD_NONCE_LEN: usize = 12;
/// Length in bytes of the AES-256 key for one direction.
pub const AEAD_KEY_LEN: usize = 32;

/// Magic prefix byte for an AEAD-sealed frame.
///
/// Plain JSON frames are unchanged from the legacy wire format (and any
/// future frame whose first byte happens to be anything other than this
/// value); the connector's WS layer switches to `0x01` after both sides
/// agree on channel keys, so we can mix cleartext handshake traffic
/// (`Hello`, `Join`) with sealed post-handshake traffic (`Joined`, RPCs,
/// events, pings) on the same connection.
pub const FRAME_TAG_SEALED: u8 = 0x01;

/// HKDF info prefix used for the per-direction channel keys; the
/// remaining bytes of the info cover the bound identities ("the peer
/// half" + "the receiving half") to make sure two different channels
/// between the same two key pairs produce different keys.
pub const CHANNEL_INFO_PREFIX: &[u8] = b"oly-channel-v1";

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>> {
    B64.decode(s.trim())
        .map_err(|e| AppError::Protocol(format!("invalid base64: {e}")))
}/// Load the raw 32-byte Ed25519 seed from `<state>/ssh_host_key`. The
/// daemon writes this file at first start (see
/// `NodeIdentity::create_or_load`); it contains the bare seed bytes — no
/// OpenSSH framing, no PEM headers.
pub fn load_raw_seed(path: &std::path::Path) -> Result<[u8; 32]> {
    let bytes = std::fs::read(path).map_err(|e| {
        AppError::Protocol(format!("failed to read SSH seed {}: {e}", path.display()))
    })?;
    let mut seed = [0u8; 32];
    if bytes.len() != 32 {
        return Err(AppError::Protocol(format!(
            "SSH seed {}: expected 32 raw bytes, got {}",
            path.display(),
            bytes.len()
        )));
    }
    seed.copy_from_slice(&bytes);
    Ok(seed)
}

/// Canonical public key line derived from a signing key.
pub fn public_key_line(signing_key: &ed25519_dalek::SigningKey) -> String {
    format!(
        "ssh-ed25519 {}",
        b64_encode(signing_key.verifying_key().as_bytes())
    )
}

/// Result of deriving the per-direction session keys for a node channel.
///
/// `c2s` is the key the **secondary** uses for outbound frames and the
/// **primary** uses for inbound frames. `s2c` is the inverse. Both
/// sides compute the same pair; only the *direction* label attached to
/// each key changes. Nonces are random 12 bytes chosen per frame
/// (collision probability < 2⁻⁹⁶).
#[derive(Clone, Copy)]
pub struct ChannelKeys {
    pub c2s: [u8; AEAD_KEY_LEN],
    pub s2c: [u8; AEAD_KEY_LEN],
}

/// Convert an Ed25519 verifying key to its X25519 Montgomery pubkey
/// (the [birational map][rfc7748] used by all major ed25519 ↔ x25519
/// bridges). Safe to expose on the wire — it's a pubkey.
///
/// [rfc7748]: https://www.rfc-editor.org/rfc/rfc7748#section-4.1
pub fn ed25519_pub_to_x25519(pub_ed: &ed25519_dalek::VerifyingKey) -> [u8; 32] {
    let mont: curve25519_dalek::MontgomeryPoint = pub_ed.to_montgomery();
    mont.to_bytes()
}

/// Convert an Ed25519 signing key to an X25519 StaticSecret (the
/// long-term secret-input key used for ECDH). Implemented by taking
/// the same SHA-512 hash of the seed that ed25519 itself uses, then
/// applying the clamped scalar — which is exactly the conversion
/// [`ed25519_dalek::SigningKey::to_scalar`] performs. Result is
/// suitable for `diffie_hellman` against a peer's `X25519Public`.
pub fn ed25519_priv_to_x25519(sk: &ed25519_dalek::SigningKey) -> StaticSecret {
    let scalar: curve25519_dalek::Scalar = sk.to_scalar();
    StaticSecret::from(scalar.to_bytes())
}

/// Derive the two per-direction channel keys from the long-term keys
/// of the primary and the secondary.
///
/// Both sides call this with the same pair of pubkeys (in `Ed25519`
/// form, for binding the keys to the channel parties) and their own
/// private X25519 secret. The output is symmetric: both sides arrive at
/// the same `[c2s; s2c]` from their respective perspectives.
///
/// `primary_pub_ed` and `secondary_pub_ed` are passed as raw 32-byte
/// Ed25519 pubkey bodies (NOT the full OpenSSH line) so the keys are
/// deterministically bound to *exactly* the two identities on this
/// channel — re-using the same DH pair on a different link produces
/// different keys (no key-reuse across channels).
pub fn derive_channel_keys(
    priv_x: &StaticSecret,
    peer_pub_x_bytes: &[u8; 32],
    primary_pub_ed: &[u8; 32],
    secondary_pub_ed: &[u8; 32],
) -> ChannelKeys {
    let peer = X25519Public::from(*peer_pub_x_bytes);
    let shared = priv_x.diffie_hellman(&peer);
    let prk = Hkdf::<Sha256>::new(None, shared.as_bytes());

    // Two separate HKDF-Expand calls produce two *independent* keys.
    // The info string is built once per direction as:
    //   "oly-channel-v1" || "|" || primary_pub_ed || "|" || secondary_pub_ed
    //                    || "|" || direction (3 bytes)
    // The bound identities keep two channels that re-use the same DH
    // pair from accidentally producing the same keys.
    let mut c2s = [0u8; AEAD_KEY_LEN];
    let mut s2c = [0u8; AEAD_KEY_LEN];
    for (dir_label, slot) in [(b"c2s" as &[u8], &mut c2s), (b"s2c", &mut s2c)] {
        let mut info = Vec::with_capacity(
            CHANNEL_INFO_PREFIX.len() + 1 + 32 + 1 + 32 + 1 + dir_label.len(),
        );
        info.extend_from_slice(CHANNEL_INFO_PREFIX);
        info.push(b'|');
        info.extend_from_slice(primary_pub_ed);
        info.push(b'|');
        info.extend_from_slice(secondary_pub_ed);
        info.push(b'|');
        info.extend_from_slice(dir_label);
        prk.expand(&info, slot.as_mut_slice())
            .expect("HKDF expand with 32-byte output fits");
    }
    ChannelKeys { c2s, s2c }
}

/// Header byte + len: a sealed frame is `[0x01, nonce(12), ct, tag(16)]`.
pub const SEALED_HEADER_LEN: usize = 1 + AEAD_NONCE_LEN;

/// Seal `plaintext` with `key`. Returns a frame of the form
/// `[0x01, nonce(12), AES-GCM ciphertext + 16-byte tag]`. The nonce is
/// drawn from rand's thread-local RNG (backed by the OS CSPRNG); the
/// AAD is the message *kind* string so different message types are
/// distinguishable inside the same channel.
pub fn seal_frame(key: &[u8; AEAD_KEY_LEN], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut nonce_bytes = [0u8; AEAD_NONCE_LEN];
    rand::fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|e| AppError::Protocol(format!("AEAD encryption failed: {e}")))?;
    let mut out = Vec::with_capacity(SEALED_HEADER_LEN + ct.len());
    out.push(FRAME_TAG_SEALED);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a sealed frame produced by [`seal_frame`]; returns the
/// original plaintext on success. An AEAD failure (wrong key, tampered
/// ciphertext, or wrong AAD) surfaces as a protocol error.
pub fn open_frame(key: &[u8; AEAD_KEY_LEN], aad: &[u8], frame: &[u8]) -> Result<Vec<u8>> {
    if frame.is_empty() || frame[0] != FRAME_TAG_SEALED {
        return Err(AppError::Protocol(
            "expected sealed frame (0x01 prefix) after handshake; got plain or empty".into(),
        ));
    }
    if frame.len() < SEALED_HEADER_LEN + 16 {
        return Err(AppError::Protocol(
            "sealed frame too short (missing nonce or tag)".into(),
        ));
    }
    let nonce_bytes = &frame[1..1 + AEAD_NONCE_LEN];
    let ct = &frame[1 + AEAD_NONCE_LEN..];
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher
        .decrypt(
            Nonce::from_slice(nonce_bytes),
            Payload {
                msg: ct,
                aad,
            },
        )
        .map_err(|e| AppError::Protocol(format!("AEAD decryption failed: {e}")))
}

/// Quick test if a frame is sealed (its first byte is the magic).
pub fn is_sealed_frame(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes[0] == FRAME_TAG_SEALED
}

/// Where the channel is on the way to / past its cryptographically
/// authenticated phase. Both the secondary's connector and the
/// primary's WS server walk the same state machine — and because the
/// two sides agree on when sealed mode starts (after both have the
/// peer's pubkey and have derived the same AES-256-GCM keys), a
/// single value is enough to make the right framing choice per
/// direction.
#[derive(Default, Clone, Copy)]
pub enum ChannelPhase {
    /// Plain JSON (or legacy gzipped JSON, see
    /// [`decode_node_ws_payload`]). Used for the initial handshake:
    /// `hello` (the secondary's pubkey announcement) and `join` /
    /// `joined`.
    #[default]
    Plain,
    /// All subsequent application frames are AES-256-GCM sealed under
    /// derived per-direction keys. The mismatch risk is captured by the
    /// `0x01` byte: a stray plain frame after handshake is rejected.
    Sealed {
        /// Key used when *this side* writes (i.e. outbound).
        send: [u8; AEAD_KEY_LEN],
        /// Key used when *this side* reads (i.e. inbound).
        recv: [u8; AEAD_KEY_LEN],
    },
}

impl ChannelPhase {
    pub fn seal(&mut self, send: [u8; AEAD_KEY_LEN], recv: [u8; AEAD_KEY_LEN]) {
        *self = ChannelPhase::Sealed { send, recv };
    }
}

/// Encode `message` for transmission under `phase`.
///
/// Plain messages go through the legacy JSON / gzipped path so the
/// pre-handshake traffic remains wire-compatible with prior versions.
/// Sealed phase frames start with the [`FRAME_TAG_SEALED`] magic byte
/// and carry a 12-byte random nonce + AES-256-GCM ciphertext + 16-byte
/// tag. The message type is encoded into the sealey AAD so receivers
/// that don't decrypt can at least detect protocol confusion.
pub fn phase_encode_message(
    phase: &ChannelPhase,
    message: &crate::protocol::NodeWsMessage,
) -> Result<Vec<u8>> {
    use crate::protocol::NodeWsMessage;
    match phase {
        ChannelPhase::Plain => match message {
            NodeWsMessage::Error { .. } => {
                // Reuse the legacy encoder for plain JSON / gzipped payloads.
                crate::protocol::encode_node_ws_payload(message)
            }
            _ => crate::protocol::encode_node_ws_payload(message),
        }.map_err(|e| AppError::Protocol(format!("{e}"))),
        ChannelPhase::Sealed { send, .. } => {
            let plain_json = serde_json::to_vec(message)
                .map_err(|e| AppError::Protocol(format!("failed to serialize sealed message: {e}")))?;
            seal_frame(send, &[], &plain_json)
        }
    }
}

/// Decode wire bytes + phase into a [`NodeWsMessage`].
///
/// In plain phase we just call the legacy JSON / gzipped decoder. In
/// sealed phase we require the [`FRAME_TAG_SEALED`] magic, decrypt
/// under `recv`, then JSON-decode (gzipped decompression is also routed
/// through to the legacy receipt helper, so cross-version fallback in
/// trusted-only environments remains consistent).
pub fn phase_decode_payload(
    phase: &ChannelPhase,
    bytes: &[u8],
) -> Result<crate::protocol::NodeWsMessage> {
    use crate::protocol::NodeWsMessage;
    if is_sealed_frame(bytes) {
        let recv = match phase {
            ChannelPhase::Sealed { recv, .. } => recv,
            ChannelPhase::Plain => {
                return Err(AppError::Protocol(
                    "received sealed frame before channel keys established".into(),
                ));
            }
        };
        let plain = open_frame(recv, &[], bytes)?;
        let parsed: NodeWsMessage = serde_json::from_slice(&plain)
            .map_err(|e| AppError::Protocol(format!("failed to decode sealed message: {e}")))?;
        return Ok(parsed);
    }
    // Plain frame: ensure the receiver side is also in plain (we never
    // silently accept plaintext *after* handshake began — a downgrade
    // here would let an MITM swap sealed frames for plain).
    if matches!(phase, ChannelPhase::Sealed { .. }) {
        return Err(AppError::Protocol(
            "received plain frame after channel keys established (downgrade attempt?)".into(),
        ));
    }
    crate::protocol::decode_node_ws_payload(bytes)
       .map_err(|e| AppError::Protocol(format!("{e}")))
}

/// Stable "kind" string for a [`NodeWsMessage`] used as AEAD AAD when
/// sealing. The receiver derives the *plain* payload first via
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

/// Build the payload the secondary signs to authenticate a join.
pub fn node_join_payload(name: &str, public_key: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(NODE_JOIN_CONTEXT.len() + name.len() + public_key.len());
    payload.extend_from_slice(NODE_JOIN_CONTEXT);
    payload.extend_from_slice(name.as_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng as _;

    fn test_signing_key(seed: u8) -> ed25519_dalek::SigningKey {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
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

    #[test]
    fn sign_verify_round_trip() {
        let sk = test_signing_key(1);
        let payload = node_join_payload("worker-a", &public_key_line(&sk));
        let sig = sign_b64(&sk, &payload);
        assert!(verify_signature(&public_key_line(&sk), &sig, &payload));
    }

    #[test]
    fn verify_rejects_wrong_key_tampered_payload_and_names() {
        let sk = test_signing_key(2);
        let other = test_signing_key(3);
        let payload = node_join_payload("worker-a", &public_key_line(&sk));
        let sig = sign_b64(&sk, &payload);

        // Wrong public key.
        assert!(!verify_signature(&public_key_line(&other), &sig, &payload));
        // Tampered payload (name substituted).
        let tampered = node_join_payload("victim", &public_key_line(&sk));
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
    fn full_handshake_payloads_are_domain_separated() {
        let host_sk = test_signing_key(6);
        let node_sk = test_signing_key(7);

        let canonical_host = public_key_line(&host_sk);
        let canonical_node = public_key_line(&node_sk);
        let join_payload = node_join_payload("worker-a", &canonical_node);
        let join_sig = sign_b64(&node_sk, &join_payload);
        assert!(verify_signature(&canonical_node, &join_sig, &join_payload));

        // A signature produced for some other context (here: using
        // identity != canonical_node, identical bytes) must not be
        // accepted as the join signature.
        let bogus_payload = node_join_payload("worker-a", &canonical_host);
        assert!(!verify_signature(&canonical_node, &join_sig, &bogus_payload));
    }
}
