//! Federation cryptography — node-identity authentication + AEAD channel.
//!
//! This is the v2 of the previous `sshauth` module. The module name no
//! longer implies an SSH integration: oly does not implement the SSH wire
//! protocol, does not read `~/.ssh/`, and does not talk to the system SSH
//! agent. The canonical public-key string is oly's own typed encoding:
//! `ed25519 <standard-base64(raw32)>`. Real OpenSSH wire-format lines are
//! intentionally not accepted.
//!
//! ## Cryptographic scope
//!
//! - **Identity**: Ed25519 (RFC 8032) signing key per daemon, persisted as a
//!   raw 32-byte seed.
//! - **Channel key derivation**: static-static X25519 Diffie-Hellman via the
//!   birational Ed25519↔X25519 map (RFC 7748 §4.1) plus HKDF-SHA-256 over the
//!   signed handshake transcript.
//! - **Frame confidentiality / integrity**: AES-256-GCM. Per-direction
//!   monotonic counters are bound into the 12-byte nonce as a four-byte zero
//!   prefix followed by an 8-byte big-endian sequence; replay and reordering
//!   are rejected terminally.
//!
//! **Forward secrecy is NOT provided.** Static identity keys are used as both
//! signing keys and DH long-term keys. Static-static DH plus per-connection
//! transcripts and counter nonces give session uniqueness and replay
//! protection but not forward secrecy. An attacker who later obtains a
//! daemon's long-term identity seed can derive past session keys from
//! captured public identities and handshake transcripts. Adopting ephemeral DH or
//! Noise is a separate project.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::Signer as _;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519Public, StaticSecret};

use aes_gcm::aead::{Aead, KeyInit, Payload, array::typenum::U12};
use aes_gcm::{Aes256Gcm, Key, Nonce};

use crate::error::{AppError, Result};

// ---------------------------------------------------------------------------
// Public-key encoding
// ---------------------------------------------------------------------------

/// Tag for the canonical public-key string. Real OpenSSH lines and the
/// previous `ssh-ed25519` intermediate form are explicitly not accepted.
pub const PUBKEY_TAG: &str = "ed25519";

/// Domain separation for the join transcript signature.
pub const NODE_JOIN_CONTEXT: &[u8] = b"oly-node-join-v2";

/// HKDF info prefix used for per-direction channel keys.
pub const CHANNEL_INFO_PREFIX: &[u8] = b"oly-channel-v2";

/// 12 bytes per AES-GCM nonce.
pub const AEAD_NONCE_LEN: usize = 12;
/// 32 bytes per AES-256 key.
pub const AEAD_KEY_LEN: usize = 32;
/// Magic prefix byte for an AEAD-sealed application frame.
pub const FRAME_TAG_SEALED: u8 = 0x21;
/// Length of the sealed frame header (1-byte tag + 8-byte sequence).
pub const SEALED_HEADER_LEN: usize = 1 + 8;
/// Length of an AES-GCM authentication tag.
pub const GCM_TAG_LEN: usize = 16;
/// Minimum sealed frame length (header + tag; empty payload allowed).
pub const MIN_SEALED_FRAME_LEN: usize = SEALED_HEADER_LEN + GCM_TAG_LEN;
/// Counter at which we close the channel rather than risk reuse.
pub const MAX_SEQUENCE: u64 = u64::MAX - 1;

const NONCE_PREFIX: [u8; 4] = [0u8; 4];

// ---------------------------------------------------------------------------
// Base64 helpers
// ---------------------------------------------------------------------------

pub fn b64_encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>> {
    B64.decode(s.trim())
        .map_err(|e| AppError::Protocol(format!("invalid base64: {e}")))
}

// ---------------------------------------------------------------------------
// Public-key text handling
// ---------------------------------------------------------------------------

/// 32-byte Ed25519 public key, structurally validated.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKeyBytes([u8; 32]);

impl PublicKeyBytes {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Canonical textual form: `ed25519 <standard-base64(raw32)>`.
    pub fn to_line(self) -> String {
        format!("{PUBKEY_TAG} {}", b64_encode(&self.0))
    }
}

impl std::fmt::Debug for PublicKeyBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicKeyBytes")
            .field("hex", &format!("{:02x?}", self.0))
            .finish()
    }
}

impl std::str::FromStr for PublicKeyBytes {
    type Err = AppError;
    fn from_str(s: &str) -> Result<Self> {
        parse_public_key(s)
    }
}

/// Parse an `ed25519 <base64(raw32)>` public-key line into typed bytes.
///
/// Rejects:
/// - empty / whitespace-only input,
/// - wrong tag (including legacy `ssh-ed25519` or real OpenSSH lines),
/// - extra tokens,
/// - base64 decode errors,
/// - wrong decoded length,
/// - invalid, non-canonical, small-order or non-prime-subgroup points.
pub fn parse_public_key(input: &str) -> Result<PublicKeyBytes> {
    let trimmed = input.trim();
    let mut parts = trimmed.split_whitespace();
    let first = parts.next();
    let second = parts.next();
    let third = parts.next();
    if third.is_some() {
        return Err(AppError::Protocol(format!(
            "expected public key in the form `{PUBKEY_TAG} <base64>`, got extra tokens after {input:?}"
        )));
    }
    let (Some(tag), Some(encoded)) = (first, second) else {
        return Err(AppError::Protocol(format!(
            "expected public key in the form `{PUBKEY_TAG} <base64>`, got: {:?}",
            input
        )));
    };
    if tag != PUBKEY_TAG {
        return Err(AppError::Protocol(format!(
            "expected public key in the form `{PUBKEY_TAG} <base64>`, got token '{tag}'"
        )));
    }
    let raw = b64_decode(encoded)?;
    let arr: [u8; 32] = <[u8; 32]>::try_from(raw.as_slice())
        .map_err(|_| AppError::Protocol("public-key decode length must be 32 bytes".into()))?;
    let key = ed25519_dalek::VerifyingKey::from_bytes(&arr).map_err(|e| {
        AppError::Protocol(format!(
            "public key is not a valid Ed25519 point ({e}); the peer may have \
             provided a small-subgroup or non-canonical encoding"
        ))
    })?;
    let point = curve25519_dalek::edwards::CompressedEdwardsY(arr)
        .decompress()
        .ok_or_else(|| AppError::Protocol("invalid Ed25519 point".into()))?;
    if key.is_weak() || !point.is_torsion_free() || point.compress().to_bytes() != arr {
        return Err(AppError::Protocol(
            "weak or non-canonical node identity".into(),
        ));
    }
    Ok(PublicKeyBytes(key.to_bytes()))
}

/// Canonical text form: `ed25519 <standard-base64(raw32)>`.
pub fn public_key_line(signing_key: &ed25519_dalek::SigningKey) -> String {
    format!(
        "{PUBKEY_TAG} {}",
        b64_encode(signing_key.verifying_key().as_bytes())
    )
}

// ---------------------------------------------------------------------------
// Shared-secret transcript signature
// ---------------------------------------------------------------------------

/// Domain-separated handshake transcript used by both sides to sign / verify.
///
/// Each field has a u32 big-endian byte length. Binary fields may contain
/// any byte; delimiter concatenation would not be unambiguous.
#[allow(clippy::too_many_arguments)]
pub fn build_transcript(
    federation_version: u16,
    role: &str,
    name: &str,
    secondary_pub: &[u8; 32],
    primary_pub: &[u8; 32],
    secondary_nonce: &[u8; 32],
    primary_nonce: &[u8; 32],
) -> Vec<u8> {
    let mut v = Vec::new();
    for field in [
        NODE_JOIN_CONTEXT,
        &federation_version.to_be_bytes(),
        role.as_bytes(),
        b"secondary",
        b"primary",
        name.as_bytes(),
        secondary_pub,
        primary_pub,
        secondary_nonce,
        primary_nonce,
    ] {
        v.extend_from_slice(
            &u32::try_from(field.len())
                .expect("bounded field")
                .to_be_bytes(),
        );
        v.extend_from_slice(field);
    }
    v
}

/// Sign `transcript` and return the base64-encoded raw 64-byte Ed25519
/// signature.
pub fn sign_transcript(signing_key: &ed25519_dalek::SigningKey, transcript: &[u8]) -> String {
    b64_encode(&signing_key.sign(transcript).to_bytes())
}

/// Verify a transcript signature against an identity-keyed transcript. Fails
/// closed on parse, length, or verification error.
pub fn verify_transcript(
    public_key: &PublicKeyBytes,
    signature_b64: &str,
    transcript: &[u8],
) -> bool {
    let Ok(sig_bytes) = b64_decode(signature_b64) else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    let Ok(verifying) = ed25519_dalek::VerifyingKey::from_bytes(&public_key.0) else {
        return false;
    };
    verifying
        .verify_strict(transcript, &ed25519_dalek::Signature::from_bytes(&sig_arr))
        .is_ok()
}

// ---------------------------------------------------------------------------
// Identity file I/O — file paths renamed from `ssh_host_key{,pub}` to
// `identity_key{,pub}` so the on-disk name no longer references SSH.
// ---------------------------------------------------------------------------

/// Path to the raw seed `<state>/identity_key`.
pub fn seed_path(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("identity_key")
}

/// Path to the published canonical public line `<state>/identity_key.pub`.
pub fn pub_key_path(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("identity_key.pub")
}

/// Load the raw 32-byte Ed25519 seed.
pub fn load_seed(path: &std::path::Path) -> Result<[u8; 32]> {
    let bytes = std::fs::read(path).map_err(|e| {
        AppError::Protocol(format!(
            "failed to read identity seed {}: {e}",
            path.display()
        ))
    })?;
    let mut seed = [0u8; 32];
    if bytes.len() != 32 {
        return Err(AppError::Protocol(format!(
            "identity seed {}: expected 32 raw bytes, got {}",
            path.display(),
            bytes.len()
        )));
    }
    seed.copy_from_slice(&bytes);
    Ok(seed)
}

/// Generate or load the daemon's identity under `state_dir`.
///
/// Writes the seed with user-private permissions on Unix **before** any data
/// is made readable. A corrupt or inaccessible seed disables federation with
/// a clear error rather than ever being silently regenerated.
pub fn create_or_load_identity(
    state_dir: &std::path::Path,
) -> std::io::Result<(ed25519_dalek::SigningKey, String)> {
    let path = seed_path(state_dir);
    let pub_path = pub_key_path(state_dir);

    let signing_key = if path.exists() {
        match load_seed(&path) {
            Ok(seed) => ed25519_dalek::SigningKey::from_bytes(&seed),
            Err(err) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "identity key at {} is unreadable; federation is disabled: {err}",
                        path.display()
                    ),
                ));
            }
        }
    } else {
        let mut seed_bytes = [0u8; 32];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut seed_bytes);
        let new_key = ed25519_dalek::SigningKey::from_bytes(&seed_bytes);
        let mut seed_out = std::fs::OpenOptions::new();
        seed_out.write(true).create_new(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            seed_out.mode(0o600);
        }
        let mut writer = seed_out.open(&path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("create identity seed at {}: {e}", path.display()),
            )
        })?;
        if let Err(e) = std::io::Write::write_all(&mut writer, new_key.to_bytes().as_slice()) {
            let _ = std::fs::remove_file(&path);
            return Err(std::io::Error::other(format!(
                "write identity seed to {}: {e}",
                path.display()
            )));
        }
        if let Err(e) = writer.sync_all() {
            // Best-effort durability; surface if it actually fails rather than
            // silently keeping an under-flushed seed on disk.
            let _ = std::fs::remove_file(&path);
            return Err(std::io::Error::other(format!(
                "flush identity seed to {}: {e}",
                path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        new_key
    };

    let line = public_key_line(&signing_key);
    std::fs::write(&pub_path, format!("{line}\n"))?;
    Ok((signing_key, line))
}

/// Read the on-disk canonical public line for `state_dir`, if present.
pub fn read_published_pubkey(state_dir: &std::path::Path) -> std::io::Result<Option<String>> {
    let path = pub_key_path(state_dir);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim().to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Channel keys via static-static DH + transcript
// ---------------------------------------------------------------------------

/// Raw pair of per-direction symmetric keys, before side orientation is
/// applied. Both peers MUST produce the same `(c2s, s2c)` pair from the
/// same handshake inputs.
#[derive(PartialEq, Eq)]
pub struct DerivedKeys {
    /// Client (secondary) → server (primary).
    pub c2s: [u8; AEAD_KEY_LEN],
    /// Server (primary) → client (secondary).
    pub s2c: [u8; AEAD_KEY_LEN],
}

/// Mint a fresh 32-byte nonce. Not a secret — only a transcript binding
/// input. Each side produces its own and binds it to the signed transcript.
pub fn fresh_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut n);
    n
}

/// Convert an Ed25519 verifying key to its X25519 Montgomery pubkey via the
/// birational map (RFC 7748 §4.1).
pub fn ed25519_pub_to_x25519(pub_ed: &ed25519_dalek::VerifyingKey) -> [u8; 32] {
    let mont: curve25519_dalek::MontgomeryPoint = pub_ed.to_montgomery();
    mont.to_bytes()
}

/// Convert the daemon's Ed25519 seed to an X25519 StaticSecret.
///
/// We feed `static_secrets` the **raw** `SHA-512(seed) || ...` bytes (i.e.
/// `SigningKey::to_scalar_bytes`), not the clamped-and-reduced form returned
/// by `to_scalar()`. The two operations are not equivalent; mixing them
/// produces different DH shared secrets and breaks channel agreement.
/// `x25519-dalek` clamps the input itself.
pub fn ed25519_priv_to_x25519(sk: &ed25519_dalek::SigningKey) -> StaticSecret {
    StaticSecret::from(sk.to_scalar_bytes())
}

/// Derive per-connection, per-direction channel keys from a transcript and
/// cross-DH shared secret.
///
/// Rejects the all-zero shared secret, which would indicate a degenerate
/// curve point or non-contributory peer key, rather than producing a
/// known-weak key stream.
///
/// Identities are placed into the HKDF info in **lexicographic order**,
/// not "local / peer", so both sides arrive at the same `(c2s, s2c)` pair.
pub fn derive_channel_keys(
    local_x: &StaticSecret,
    peer_x_bytes: &[u8; 32],
    transcript: &[u8],
    local_ed_bytes: &[u8; 32],
    peer_ed_bytes: &[u8; 32],
) -> Result<DerivedKeys> {
    let peer = X25519Public::from(*peer_x_bytes);
    let shared = local_x.diffie_hellman(&peer);
    if shared.as_bytes() == &[0u8; 32] {
        return Err(AppError::Protocol(
            "X25519 key agreement produced a zero shared secret; refusing to derive keys".into(),
        ));
    }
    let hash = transcript_hash(transcript);
    let prk = Hkdf::<Sha256>::new(Some(&hash), shared.as_bytes());

    let (lo, hi) = order_pair(local_ed_bytes, peer_ed_bytes);

    let mut c2s = [0u8; AEAD_KEY_LEN];
    let mut s2c = [0u8; AEAD_KEY_LEN];
    for (label, slot) in [(b"c2s" as &[u8], &mut c2s), (b"s2c", &mut s2c)] {
        let mut info =
            Vec::with_capacity(CHANNEL_INFO_PREFIX.len() + 1 + 32 + 1 + 32 + 1 + label.len());
        info.extend_from_slice(CHANNEL_INFO_PREFIX);
        info.push(b'|');
        info.extend_from_slice(lo);
        info.push(b'|');
        info.extend_from_slice(hi);
        info.push(b'|');
        info.extend_from_slice(label);
        prk.expand(&info, slot.as_mut_slice())
            .map_err(|e| AppError::Protocol(format!("HKDF expand for channel keys failed: {e}")))?;
    }
    Ok(DerivedKeys { c2s, s2c })
}

fn order_pair<'a>(a: &'a [u8; 32], b: &'a [u8; 32]) -> (&'a [u8; 32], &'a [u8; 32]) {
    if a[..] <= b[..] { (a, b) } else { (b, a) }
}

// ---------------------------------------------------------------------------
// AEAD sealing with per-direction monotonic counters. Default frame layout:
// [tag:u8 || sequence:u64 BE || ciphertext || GCM tag:16]
// Nonce: [0u8; 4] || sequence.to_be_bytes()
// ---------------------------------------------------------------------------

fn nonce_for(sequence: u64) -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    n[..NONCE_PREFIX.len()].copy_from_slice(&NONCE_PREFIX);
    n[NONCE_PREFIX.len()..].copy_from_slice(&sequence.to_be_bytes());
    n
}

/// Per-direction channel state holding the AEAD key plus monotonic counters.
///
/// Not `Clone`: the counters must not be duplicated. The single relay task
/// is its only owner.
pub struct ChannelSide {
    key: [u8; AEAD_KEY_LEN],
    send_seq: u64,
    recv_seq: u64,
    failed: bool,
}

impl ChannelSide {
    pub fn new(key: [u8; AEAD_KEY_LEN]) -> Self {
        ChannelSide {
            key,
            send_seq: 0,
            recv_seq: 0,
            failed: false,
        }
    }

    /// Advance the send counter as a side effect.
    pub fn seal(&mut self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
        if self.failed || self.send_seq >= MAX_SEQUENCE {
            self.failed = true;
            return Err(AppError::Protocol(
                "sealed send counter near exhaustion; closing connection".into(),
            ));
        }
        let seq = self.send_seq;
        self.send_seq += 1;
        let nonce = nonce_for(seq);
        let aad = frame_aad(aad, seq);
        let cipher = Aes256Gcm::new(
            <&Key<Aes256Gcm>>::try_from(self.key.as_slice()).expect("AEAD key must be 32 bytes"),
        );
        let ct = match cipher.encrypt(
            <&Nonce<U12>>::try_from(nonce.as_slice()).expect("nonce fits U12"),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        ) {
            Ok(ct) => ct,
            Err(err) => {
                self.failed = true;
                return Err(AppError::Protocol(format!(
                    "sealed frame encryption failed: {err}"
                )));
            }
        };
        let mut out = Vec::with_capacity(SEALED_HEADER_LEN + ct.len());
        out.push(FRAME_TAG_SEALED);
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open a sealed frame, advancing receive state only on success.
    #[cfg(test)]
    pub fn open(&mut self, aad: &[u8], frame: &[u8]) -> Result<Vec<u8>> {
        self.open_validated(aad, frame, |bytes| Ok(bytes.to_vec()))
    }

    /// Poison on any failure; commit the receive sequence only after parsing.
    fn open_validated<T>(
        &mut self,
        aad: &[u8],
        frame: &[u8],
        parse: impl FnOnce(&[u8]) -> Result<T>,
    ) -> Result<T> {
        if self.failed || self.recv_seq >= MAX_SEQUENCE {
            self.failed = true;
            return Err(AppError::Protocol(
                "sealed channel failed or exhausted".into(),
            ));
        }
        self.failed = true;
        if frame.len() < MIN_SEALED_FRAME_LEN {
            return Err(AppError::Protocol(format!(
                "sealed frame too short ({} bytes, need at least {})",
                frame.len(),
                MIN_SEALED_FRAME_LEN
            )));
        }
        if frame[0] != FRAME_TAG_SEALED {
            return Err(AppError::Protocol(format!(
                "sealed frame missing 0x{:02x} tag, got 0x{:02x}",
                FRAME_TAG_SEALED, frame[0]
            )));
        }
        let mut seq_bytes = [0u8; 8];
        seq_bytes.copy_from_slice(&frame[1..1 + 8]);
        let seq = u64::from_be_bytes(seq_bytes);

        if seq != self.recv_seq {
            return Err(AppError::Protocol(format!(
                "sealed frame sequence mismatch: received {seq}, expected {}",
                self.recv_seq
            )));
        }
        let aad = frame_aad(aad, seq);
        let nonce = nonce_for(seq);
        let cipher = Aes256Gcm::new(
            <&Key<Aes256Gcm>>::try_from(self.key.as_slice()).expect("AEAD key must be 32 bytes"),
        );
        let plain = cipher
            .decrypt(
                <&Nonce<U12>>::try_from(nonce.as_slice()).expect("nonce fits U12"),
                Payload {
                    msg: &frame[SEALED_HEADER_LEN..],
                    aad: &aad,
                },
            )
            .map_err(|e| {
                AppError::Protocol(format!("sealed frame AEAD verification failed: {e}"))
            })?;
        let parsed = parse(&plain)?;
        self.recv_seq += 1;
        self.failed = false;
        Ok(parsed)
    }
}

/// Transcript digest is public, never a key or a standalone authenticator.
pub fn transcript_hash(transcript: &[u8]) -> [u8; 32] {
    Sha256::digest(transcript).into()
}

fn frame_aad(context: &[u8], seq: u64) -> Vec<u8> {
    let mut aad = context.to_vec();
    aad.push(FRAME_TAG_SEALED);
    aad.extend_from_slice(&seq.to_be_bytes());
    aad
}

/// A live connection has exactly one mutable owner and no plaintext mode.
pub struct Channel {
    send: ChannelSide,
    recv: ChannelSide,
    send_aad: Vec<u8>,
    recv_aad: Vec<u8>,
    transcript_hash: [u8; 32],
}

impl Channel {
    pub fn new(
        local: &ed25519_dalek::SigningKey,
        peer: &PublicKeyBytes,
        transcript: &[u8],
        secondary: bool,
    ) -> Result<Self> {
        let peer_key = ed25519_dalek::VerifyingKey::from_bytes(peer.as_bytes())
            .map_err(|e| AppError::Protocol(e.to_string()))?;
        let keys = derive_channel_keys(
            &ed25519_priv_to_x25519(local),
            &ed25519_pub_to_x25519(&peer_key),
            transcript,
            local.verifying_key().as_bytes(),
            peer.as_bytes(),
        )?;
        let hash = transcript_hash(transcript);
        let context = |direction: &[u8]| {
            let mut aad = b"oly-node-frame-v2".to_vec();
            aad.extend_from_slice(&hash);
            aad.extend_from_slice(direction);
            aad
        };
        let (send, recv, send_label, recv_label) = if secondary {
            (keys.c2s, keys.s2c, b"c2s", b"s2c")
        } else {
            (keys.s2c, keys.c2s, b"s2c", b"c2s")
        };
        Ok(Self {
            send: ChannelSide::new(send),
            recv: ChannelSide::new(recv),
            send_aad: context(send_label),
            recv_aad: context(recv_label),
            transcript_hash: hash,
        })
    }

    pub fn transcript_hash(&self) -> [u8; 32] {
        self.transcript_hash
    }

    pub fn encode(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        if self.recv.failed {
            return Err(AppError::Protocol("sealed channel failed".into()));
        }
        self.send.seal(&self.send_aad, payload)
    }

    pub fn decode<T>(&mut self, frame: &[u8], parse: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        if self.send.failed {
            return Err(AppError::Protocol("sealed channel failed".into()));
        }
        self.recv.open_validated(&self.recv_aad, frame, parse)
    }
}

/// Handshake frames are small uncompressed binary JSON, never text or gzip.
pub const MAX_HANDSHAKE_BYTES: usize = 4096;

pub fn encode_handshake<T: serde::Serialize>(message: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(message)?;
    if bytes.len() > MAX_HANDSHAKE_BYTES {
        return Err(AppError::Protocol("handshake frame too large".into()));
    }
    Ok(bytes)
}

pub fn decode_handshake<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    if bytes.len() > MAX_HANDSHAKE_BYTES {
        return Err(AppError::Protocol("handshake frame too large".into()));
    }
    serde_json::from_slice(bytes).map_err(AppError::from)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng as _;

    #[test]
    fn parse_round_trip_canonical_line() {
        // Build a real key to obtain a valid 32-byte payload.
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
        let line = public_key_line(&sk);
        let pk1 = parse_public_key(&line).expect("canonical parses");
        let again = parse_public_key(&pk1.to_line()).expect("roundtrip parses");
        assert_eq!(pk1, again);
    }

    #[test]
    fn parse_rejects_legacy_ssh_tag() {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
        let body = b64_encode(sk.verifying_key().as_bytes());
        let bogus = format!("ssh-ed25519 {body}");
        assert!(parse_public_key(&bogus).is_err());
    }

    #[test]
    fn parse_rejects_extra_tokens() {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
        let body = b64_encode(sk.verifying_key().as_bytes());
        let extra = format!("ed25519 {body} extra");
        assert!(parse_public_key(&extra).is_err());
    }

    #[test]
    fn parse_rejects_wrong_length_payload() {
        let short = "ed25519 AAAAAAAAAAAAAAAAAAAAAA==";
        assert!(parse_public_key(short).is_err());
    }

    #[test]
    fn channel_keys_derive_deterministically_when_transcript_matches() {
        let mut b = [0u8; 32];
        rand::rng().fill_bytes(&mut b);
        let pri = ed25519_dalek::SigningKey::from_bytes(&b);
        rand::rng().fill_bytes(&mut b);
        let sec = ed25519_dalek::SigningKey::from_bytes(&b);

        let primary_pub_x = ed25519_pub_to_x25519(&pri.verifying_key());
        let secondary_pub_x = ed25519_pub_to_x25519(&sec.verifying_key());
        let primary_priv_x = ed25519_priv_to_x25519(&pri);
        let secondary_priv_x = ed25519_priv_to_x25519(&sec);
        let primary_pub_ed = pri.verifying_key().to_bytes();
        let secondary_pub_ed = sec.verifying_key().to_bytes();

        let t = build_transcript(
            2,
            "secondary",
            "worker-1",
            &secondary_pub_ed,
            &primary_pub_ed,
            &[0xAA; 32],
            &[0xBB; 32],
        );
        let a = derive_channel_keys(
            &primary_priv_x,
            &secondary_pub_x,
            &t,
            &primary_pub_ed,
            &secondary_pub_ed,
        )
        .expect("primary");
        let bk = derive_channel_keys(
            &secondary_priv_x,
            &primary_pub_x,
            &t,
            &secondary_pub_ed,
            &primary_pub_ed,
        )
        .expect("secondary");
        assert_eq!(a.c2s, bk.c2s);
        assert_eq!(a.s2c, bk.s2c);
        assert_ne!(a.c2s, a.s2c);
    }

    #[test]
    fn channel_keys_differ_when_transcript_changes() {
        let mut b = [0u8; 32];
        rand::rng().fill_bytes(&mut b);
        let pri = ed25519_dalek::SigningKey::from_bytes(&b);
        rand::rng().fill_bytes(&mut b);
        let sec = ed25519_dalek::SigningKey::from_bytes(&b);

        let secondary_pub_x = ed25519_pub_to_x25519(&sec.verifying_key());
        let primary_priv_x = ed25519_priv_to_x25519(&pri);
        let primary_pub_ed = pri.verifying_key().to_bytes();
        let secondary_pub_ed = sec.verifying_key().to_bytes();

        let t1 = build_transcript(
            2,
            "secondary",
            "w",
            &secondary_pub_ed,
            &primary_pub_ed,
            &[1; 32],
            &[2; 32],
        );
        let t2 = build_transcript(
            2,
            "secondary",
            "w",
            &secondary_pub_ed,
            &primary_pub_ed,
            &[1; 32],
            &[3; 32],
        );
        let a = derive_channel_keys(
            &primary_priv_x,
            &secondary_pub_x,
            &t1,
            &primary_pub_ed,
            &secondary_pub_ed,
        )
        .unwrap();
        let bk = derive_channel_keys(
            &primary_priv_x,
            &secondary_pub_x,
            &t2,
            &primary_pub_ed,
            &secondary_pub_ed,
        )
        .unwrap();
        assert_ne!(a.c2s, bk.c2s);
    }

    #[test]
    fn sealed_frames_replay_and_out_of_order_are_rejected() {
        let mut k = [0u8; 32];
        rand::rng().fill_bytes(&mut k);
        let mut sender = ChannelSide::new(k);
        let mut receiver = ChannelSide::new(k);
        let fa = sender.seal(b"aad", b"hello").unwrap();
        let fb = sender.seal(b"aad", b"world").unwrap();
        assert_eq!(receiver.open(b"aad", &fa).unwrap(), b"hello");
        assert!(receiver.open(b"aad", &fa).is_err(), "replay must fail");
        assert!(receiver.open(b"aad", &fb).is_err(), "failure is terminal");
    }

    #[test]
    fn sealed_frame_zero_length_payload_round_trips() {
        let mut k = [0u8; 32];
        rand::rng().fill_bytes(&mut k);
        let mut sender = ChannelSide::new(k);
        let mut receiver = ChannelSide::new(k);
        let frame = sender.seal(b"", b"").unwrap();
        assert_eq!(frame.len(), MIN_SEALED_FRAME_LEN);
        let plain = receiver.open(b"", &frame).unwrap();
        assert!(plain.is_empty());
    }

    #[test]
    fn sealed_frame_short_or_wrong_tag_rejected() {
        let mut k = [0u8; 32];
        rand::rng().fill_bytes(&mut k);
        let mut receiver = ChannelSide::new(k);
        // Too short.
        assert!(receiver.open(b"", &[0x21]).is_err());
        // Wrong tag byte.
        let mut bad = vec![0u8; MIN_SEALED_FRAME_LEN];
        bad[0] = 0xAB;
        assert!(receiver.open(b"", &bad).is_err());
    }

    #[test]
    fn create_or_load_identity_persists_and_round_trips() {
        let dir = tempdir();
        let (k1, line1) = create_or_load_identity(&dir.join("s")).expect("first creation succeeds");
        let (k2, line2) = create_or_load_identity(&dir.join("s")).expect("reload succeeds");
        assert_eq!(k1.to_bytes(), k2.to_bytes());
        assert_eq!(line1, line2);
        // File on disk matches the canonical line.
        let on_disk = std::fs::read_to_string(dir.join("s").join("identity_key.pub"))
            .expect("public key file readable");
        assert!(on_disk.starts_with("ed25519 "));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn create_or_load_identity_seed_is_user_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir();
        let _ = create_or_load_identity(&dir.join("s")).expect("create");
        let meta =
            std::fs::metadata(dir.join("s").join("identity_key")).expect("seed metadata available");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "seed must be readable/writable only by owner");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_or_load_identity_corrupt_seed_is_fatal() {
        let dir = tempdir();
        std::fs::create_dir_all(dir.join("s")).expect("dir");
        std::fs::write(dir.join("s").join("identity_key"), b"not-32-bytes-short")
            .expect("write corrupt seed");
        let err =
            create_or_load_identity(&dir.join("s")).expect_err("corrupt seed must fail loudly");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn transcript_is_unambiguous_and_every_input_is_bound() {
        let transcript = |version, role: &str, name: &str, sec, pri, sn, pn| {
            build_transcript(version, role, name, &sec, &pri, &sn, &pn)
        };
        let base = transcript(
            2,
            "secondary-proof",
            "worker",
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        for changed in [
            transcript(
                3,
                "secondary-proof",
                "worker",
                [1; 32],
                [2; 32],
                [3; 32],
                [4; 32],
            ),
            transcript(
                2,
                "primary-proof",
                "worker",
                [1; 32],
                [2; 32],
                [3; 32],
                [4; 32],
            ),
            transcript(
                2,
                "secondary-proof",
                "victim",
                [1; 32],
                [2; 32],
                [3; 32],
                [4; 32],
            ),
            transcript(
                2,
                "secondary-proof",
                "worker",
                [9; 32],
                [2; 32],
                [3; 32],
                [4; 32],
            ),
            transcript(
                2,
                "secondary-proof",
                "worker",
                [1; 32],
                [9; 32],
                [3; 32],
                [4; 32],
            ),
            transcript(
                2,
                "secondary-proof",
                "worker",
                [1; 32],
                [2; 32],
                [9; 32],
                [4; 32],
            ),
            transcript(
                2,
                "secondary-proof",
                "worker",
                [1; 32],
                [2; 32],
                [3; 32],
                [9; 32],
            ),
        ] {
            assert_ne!(base, changed);
        }
        assert_ne!(
            transcript(2, "a\0b", "c", [1; 32], [2; 32], [3; 32], [4; 32]),
            transcript(2, "a", "b\0c", [1; 32], [2; 32], [3; 32], [4; 32])
        );
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let public = parse_public_key(&public_key_line(&key)).unwrap();
        let sig = sign_transcript(&key, &base);
        assert!(verify_transcript(&public, &sig, &base));
        assert!(!verify_transcript(&public, &sig, b"another transcript"));
    }

    #[test]
    fn weak_and_noncanonical_identities_are_rejected() {
        for raw in [
            [0; 32],
            {
                let mut a = [0; 32];
                a[0] = 1;
                a
            },
            [255; 32],
        ] {
            assert!(parse_public_key(&format!("ed25519 {}", b64_encode(&raw))).is_err());
        }
        assert!(
            derive_channel_keys(
                &StaticSecret::from([7; 32]),
                &[0; 32],
                b"t",
                &[1; 32],
                &[2; 32]
            )
            .is_err()
        );
    }

    #[test]
    fn sealed_header_aad_and_ciphertext_tampering_are_terminal() {
        let mut tx = ChannelSide::new([7; 32]);
        let frame = tx.seal(b"context", b"hello").unwrap();
        for index in [0, 8, 9, frame.len() - 1] {
            let mut bad = frame.clone();
            bad[index] ^= 1;
            let mut rx = ChannelSide::new([7; 32]);
            assert!(rx.open(b"context", &bad).is_err());
            assert_eq!(rx.recv_seq, 0);
            assert!(rx.open(b"context", &frame).is_err());
        }
        let mut rx = ChannelSide::new([7; 32]);
        assert!(rx.open(b"wrong context", &frame).is_err());
        let mut rx = ChannelSide::new([7; 32]);
        let second = tx.seal(b"context", b"second").unwrap();
        assert!(rx.open(b"context", &second).is_err(), "out of order");
        let mut tx = ChannelSide::new([7; 32]);
        tx.send_seq = MAX_SEQUENCE;
        assert!(tx.seal(b"", b"").is_err());
        let mut rx = ChannelSide::new([7; 32]);
        rx.recv_seq = MAX_SEQUENCE;
        assert!(rx.open(b"context", &frame).is_err());
    }

    #[test]
    fn failed_parse_does_not_commit_receive_sequence() {
        let mut tx = ChannelSide::new([7; 32]);
        let frame = tx.seal(b"context", b"not json").unwrap();
        let mut rx = ChannelSide::new([7; 32]);
        let result = rx.open_validated(b"context", &frame, |bytes| {
            serde_json::from_slice::<serde_json::Value>(bytes).map_err(AppError::from)
        });
        assert!(result.is_err());
        assert_eq!(rx.recv_seq, 0);
        assert!(rx.failed);
    }

    #[test]
    fn channel_rejects_reflection_wrong_key_and_prior_session_frames() {
        let primary = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let secondary = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let peer = parse_public_key(&public_key_line(&secondary)).unwrap();
        let mut sender = Channel::new(&primary, &peer, b"session one", false).unwrap();
        let frame = sender.encode(b"hello").unwrap();
        let mut reflected = Channel::new(&primary, &peer, b"session one", false).unwrap();
        assert!(reflected.decode(&frame, |b| Ok(b.to_vec())).is_err());
        let primary_pub = parse_public_key(&public_key_line(&primary)).unwrap();
        let mut next_session =
            Channel::new(&secondary, &primary_pub, b"session two", true).unwrap();
        assert!(next_session.decode(&frame, |b| Ok(b.to_vec())).is_err());
        let wrong = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut wrong_peer = Channel::new(&wrong, &primary_pub, b"session one", true).unwrap();
        assert!(wrong_peer.decode(&frame, |b| Ok(b.to_vec())).is_err());
    }

    #[test]
    fn fixed_handshake_crypto_vector() {
        let primary = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let secondary = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        let transcript = build_transcript(
            2,
            "secondary-proof",
            "worker",
            secondary.verifying_key().as_bytes(),
            primary.verifying_key().as_bytes(),
            &[1; 32],
            &[2; 32],
        );
        let keys = derive_channel_keys(
            &ed25519_priv_to_x25519(&secondary),
            &ed25519_pub_to_x25519(&primary.verifying_key()),
            &transcript,
            secondary.verifying_key().as_bytes(),
            primary.verifying_key().as_bytes(),
        )
        .unwrap();
        let mut sender = Channel::new(
            &primary,
            &parse_public_key(&public_key_line(&secondary)).unwrap(),
            &transcript,
            false,
        )
        .unwrap();
        let frame = sender.encode(br#"{"type":"key_confirm"}"#).unwrap();
        assert_eq!(
            b64_encode(&transcript_hash(&transcript)),
            "HmtdC0lXeYwNKHouLnfydZW8IaLOo3Trj6KGmdDE7DQ="
        );
        assert_eq!(
            sign_transcript(&secondary, &transcript),
            "DT3jT3pYnGE0YLAbIrpjYIq7kpEqEjUhBLiDqRJDS4cbDcZtGu22pwaBUzFuxyjPomfDEe3obj/yCyfRcbxeCg=="
        );
        assert_eq!(
            b64_encode(&keys.c2s),
            "mfzqQzh8/eny4kmnF08AM6UmqF39ygIVLzXArYynzgI="
        );
        assert_eq!(
            b64_encode(&keys.s2c),
            "4InHLOGwjhF5h9bZILyvlzFt+Fdx3jPFzeAoCrVo/O4="
        );
        assert_eq!(
            b64_encode(&frame),
            "IQAAAAAAAAAAJpkj8mmYdLZyejRx1iDTOjN9SGxhDtokVT0lE2FlJWBhNilDsAc="
        );
    }

    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let s = std::env::temp_dir().join(format!(
            "oly-fed-{}-{}-{}",
            std::process::id(),
            id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // Lay out a working state directory with boths the parent and the
        // inner "s" subdir so callers can drop in the `identity_key` files.
        std::fs::create_dir_all(s.join("s")).expect("state temp dir");
        s
    }
}
