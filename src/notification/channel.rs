use async_trait::async_trait;
use base64::Engine as _;
use tracing::info;

use crate::{
    db::Database,
    error::{AppError, Result},
    protocol::PushSubscriptionRecord,
};

use super::event::NotificationEvent;

#[async_trait]
pub trait NotificationChannel {
    fn name(&self) -> &'static str;
    async fn send(&self, event: &NotificationEvent) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Local OS notifications
// ---------------------------------------------------------------------------

pub struct LocalOsNotificationChannel {
    /// Optional external command executed on every notification.
    /// Event data is provided via `OLY_EVENT_*` environment variables.
    pub hook: Option<String>,
}

impl LocalOsNotificationChannel {
    fn play_beep() {
        use std::io::Write;
        // Best-effort fallback: terminal bell.
        let _ = std::io::stdout().write_all(b"\x07");
        let _ = std::io::stdout().flush();
    }

    /// Spawn the hook process.
    ///
    /// The hook string is split into program + arguments using simple shell-word
    /// rules (single-quoted, double-quoted, and unquoted tokens).  Each token
    /// may contain `{placeholder}` substitutions that are replaced with the
    /// corresponding event field before the process is spawned.
    ///
    /// Supported placeholders:
    /// - `{kind}`           – e.g. `input_needed`
    /// - `{summary}`        – one-line summary
    /// - `{body}`           – notification body text
    /// - `{session_ids}`    – comma-separated session IDs
    /// - `{trigger_rule}`   – trigger rule name, or empty string
    /// - `{trigger_detail}` – trigger detail, or empty string
    ///
    /// Event data is also available as `OLY_EVENT_*` environment variables
    /// for scripts that prefer to read them from the environment.
    ///
    /// The hook runs fire-and-forget; failures are logged but do not block delivery.
    fn run_hook(hook: &str, event: &NotificationEvent) {
        info!(
            session_ids = event.session_ids.join(","),
            trigger_rule = event.trigger_rule.map(|r| r.as_str()).unwrap_or_default(),
            trigger_detail = event.trigger_detail.as_deref().unwrap_or_default(),
            "use notification hook"
        );

        let session_ids = event.session_ids.join(",");
        let trigger_rule = event
            .trigger_rule
            .map(|r| r.as_str().to_string())
            .unwrap_or_default();
        let trigger_detail = event.trigger_detail.clone().unwrap_or_default();

        let tokens = match split_hook_command(hook) {
            Some(t) => t,
            None => {
                tracing::warn!(hook, "notification hook command is empty");
                return;
            }
        };

        let substitute = |s: String| -> String {
            s.replace("{kind}", event.kind.as_str())
                .replace("{summary}", &event.summary)
                .replace("{body}", &event.body)
                .replace("{session_ids}", &session_ids)
                .replace("{trigger_rule}", &trigger_rule)
                .replace("{trigger_detail}", &trigger_detail)
        };

        let mut iter = tokens.into_iter().map(substitute);
        let program = iter
            .next()
            .expect("split_hook_command returned non-empty vec");
        let args: Vec<String> = iter.collect();

        let result = std::process::Command::new(&program)
            .args(&args)
            .env("OLY_EVENT_KIND", event.kind.as_str())
            .env("OLY_EVENT_SUMMARY", &event.summary)
            .env("OLY_EVENT_BODY", &event.body)
            .env("OLY_EVENT_SESSION_IDS", &session_ids)
            .env("OLY_EVENT_TRIGGER_RULE", &trigger_rule)
            .env("OLY_EVENT_TRIGGER_DETAIL", &trigger_detail)
            .spawn();

        match result {
            Ok(mut child) => {
                // Reap in a background thread so we never block the async runtime.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(err) => {
                tracing::warn!(hook, %err, "notification hook failed to start");
            }
        }
    }
}

#[async_trait]
impl NotificationChannel for LocalOsNotificationChannel {
    fn name(&self) -> &'static str {
        "local_os"
    }

    async fn send(&self, event: &NotificationEvent) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            LocalOsNotificationChannel::play_beep();

            notify_rust::Notification::new()
                .summary(&format!("oly: {}", event.summary))
                .body(&event.body)
                .show()
                .map_err(|err| {
                    AppError::Protocol(format!("OS notification delivery failed: {err}"))
                })?;
        }

        if let Some(hook) = &self.hook {
            LocalOsNotificationChannel::run_hook(hook, event);
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Web Push (RFC 8030 / RFC 8291)
// ---------------------------------------------------------------------------

pub struct WebPushChannel {
    vapid_private_key_bytes: [u8; 32],
    vapid_public_key: String,
    vapid_subject: String,
    db: std::sync::Arc<Database>,
    http: reqwest::Client,
}

impl WebPushChannel {
    pub fn new(
        vapid_private_key_b64: &str,
        vapid_public_key_b64: &str,
        vapid_subject: &str,
        db: std::sync::Arc<Database>,
    ) -> Result<Self> {
        use p256::elliptic_curve::sec1::ToEncodedPoint as _;

        let raw = b64url_decode(vapid_private_key_b64)?;
        if raw.len() != 32 {
            return Err(AppError::Protocol(format!(
                "VAPID private key length {}, want 32",
                raw.len()
            )));
        }
        let mut key_bytes = [0u8; 32];
        key_bytes.copy_from_slice(&raw);

        let fb = p256::elliptic_curve::FieldBytes::<p256::NistP256>::from(key_bytes);
        let secret = p256::SecretKey::from_bytes(&fb)
            .map_err(|e| AppError::Protocol(format!("invalid VAPID private key: {e}")))?;
        let derived_pub = secret.public_key().to_encoded_point(false);

        let provided_pub = b64url_decode(vapid_public_key_b64)?;
        if provided_pub.as_slice() != derived_pub.as_bytes() {
            return Err(AppError::Protocol(
                "VAPID key mismatch: public key does not match private key".into(),
            ));
        }

        validate_vapid_subject(vapid_subject)?;

        Ok(Self {
            vapid_private_key_bytes: key_bytes,
            vapid_public_key: b64url(derived_pub.as_bytes()),
            vapid_subject: vapid_subject.to_string(),
            db,
            http: reqwest::Client::new(),
        })
    }

    /// Build a VAPID JWT (ES256) for the push endpoint's origin.
    fn build_vapid_jwt(&self, endpoint: &str) -> Result<String> {
        use p256::elliptic_curve::FieldBytes;

        let url = reqwest::Url::parse(endpoint)
            .map_err(|e| AppError::Protocol(format!("bad push endpoint URL: {e}")))?;
        let aud = format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or(""),
            url.port().map(|p| format!(":{p}")).unwrap_or_default()
        );

        let header_b64 = b64url(br#"{"typ":"JWT","alg":"ES256"}"#);
        let exp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 43_200; // 12 hours
        let payload_b64 = b64url(
            serde_json::json!({"sub": self.vapid_subject, "aud": aud, "exp": exp})
                .to_string()
                .as_bytes(),
        );

        let signing_input = format!("{header_b64}.{payload_b64}");

        let fb = FieldBytes::<p256::NistP256>::from(self.vapid_private_key_bytes);
        let signing_key = p256::ecdsa::SigningKey::from_bytes(&fb)
            .map_err(|e| AppError::Protocol(format!("invalid VAPID private key: {e}")))?;

        use p256::ecdsa::signature::Signer as _;
        let signature: p256::ecdsa::Signature = signing_key.sign(signing_input.as_bytes());
        let sig_b64 = b64url(signature.to_bytes().as_ref());

        Ok(format!("{signing_input}.{sig_b64}"))
    }

    /// Encrypt `payload` for the given subscriber using RFC 8291 (aes128gcm).
    fn encrypt_payload(&self, p256dh_b64: &str, auth_b64: &str, payload: &[u8]) -> Result<Vec<u8>> {
        use aes_gcm::aead::Aead as _;
        use aes_gcm::{Aes128Gcm, KeyInit as _};
        use hkdf::Hkdf;
        use p256::SecretKey;
        use p256::elliptic_curve::{ecdh::diffie_hellman, sec1::ToEncodedPoint as _};
        use rand::RngCore as _;
        use sha2::Sha256;

        let p256dh_bytes = b64url_decode(p256dh_b64)?;
        let auth_bytes = b64url_decode(auth_b64)?;

        let subscriber_pub = p256::PublicKey::from_sec1_bytes(&p256dh_bytes)
            .map_err(|e| AppError::Protocol(format!("invalid p256dh: {e}")))?;
        // Normalise to uncompressed 65-byte form for key_info.
        let subscriber_pub_bytes = subscriber_pub.to_encoded_point(false);

        // Ephemeral P-256 keypair.
        let eph_key = SecretKey::random(&mut rand::thread_rng());
        let eph_pub_encoded = eph_key.public_key().to_encoded_point(false); // 65 bytes

        // ECDH shared secret (X coordinate only).
        let shared = diffie_hellman(eph_key.to_nonzero_scalar(), subscriber_pub.as_affine());
        let ecdh_secret: &[u8] = shared.raw_secret_bytes().as_ref();

        // PRK_key = HKDF-Extract(salt=auth, IKM=ecdh_secret)
        // IKM = HKDF-Expand(PRK_key, key_info, 32)
        let mut key_info = b"WebPush: info\x00".to_vec();
        key_info.extend_from_slice(subscriber_pub_bytes.as_bytes());
        key_info.extend_from_slice(eph_pub_encoded.as_bytes());

        let hk_auth = Hkdf::<Sha256>::new(Some(&auth_bytes), ecdh_secret);
        let mut ikm = [0u8; 32];
        hk_auth
            .expand(&key_info, &mut ikm)
            .map_err(|_| AppError::Protocol("HKDF expand (auth) failed".into()))?;

        // Random 16-byte salt for the content key.
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);

        // PRK = HKDF-Extract(salt=salt, IKM=ikm)
        let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
        let mut cek = [0u8; 16];
        hk.expand(b"Content-Encoding: aes128gcm\x00", &mut cek)
            .map_err(|_| AppError::Protocol("HKDF expand (CEK) failed".into()))?;
        let mut nonce_bytes = [0u8; 12];
        hk.expand(b"Content-Encoding: nonce\x00", &mut nonce_bytes)
            .map_err(|_| AppError::Protocol("HKDF expand (nonce) failed".into()))?;

        // AES-128-GCM: plaintext = payload || 0x02 (last-record delimiter per RFC 8188).
        let mut plaintext = payload.to_vec();
        plaintext.push(0x02);

        let cipher = Aes128Gcm::new_from_slice(&cek)
            .map_err(|_| AppError::Protocol("AES key init failed".into()))?;
        let nonce = aes_gcm::Nonce::from(nonce_bytes);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_ref())
            .map_err(|_| AppError::Protocol("AES-GCM encrypt failed".into()))?;

        // RFC 8188 aes128gcm header: salt(16) | rs(4 BE) | idlen(1) | keyid(idlen) | ciphertext
        let rs: u32 = 4096;
        let eph_pub_bytes = eph_pub_encoded.as_bytes();
        let mut body = Vec::with_capacity(16 + 4 + 1 + eph_pub_bytes.len() + ciphertext.len());
        body.extend_from_slice(&salt);
        body.extend_from_slice(&rs.to_be_bytes());
        body.push(eph_pub_bytes.len() as u8);
        body.extend_from_slice(eph_pub_bytes);
        body.extend_from_slice(&ciphertext);

        Ok(body)
    }
}

#[async_trait]
impl NotificationChannel for WebPushChannel {
    fn name(&self) -> &'static str {
        "web_push"
    }

    async fn send(&self, event: &NotificationEvent) -> Result<()> {
        let subscriptions = self.db.list_push_subscriptions().await?;
        if subscriptions.is_empty() {
            return Ok(());
        }

        let payload = serde_json::json!({
            "summary": event.summary,
            "body": event.body,
            "session_ids": event.session_ids,
        })
        .to_string();

        let mut any_delivered = false;
        let mut had_failure = false;

        for sub in &subscriptions {
            if let Err(e) = self
                .deliver_one(sub, payload.as_bytes(), &mut any_delivered)
                .await
            {
                tracing::warn!(endpoint = %crate::utils::get_base_url(&sub.endpoint), %e, "web push delivery failed");
                had_failure = true;
            }
        }

        if any_delivered || !had_failure {
            Ok(())
        } else {
            Err(AppError::Protocol("all web push deliveries failed".into()))
        }
    }
}

impl WebPushChannel {
    async fn send_push_request(
        &self,
        endpoint: &str,
        encrypted: Vec<u8>,
        jwt: &str,
        use_webpush_bearer: bool,
    ) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .post(endpoint)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "86400")
            .header("Urgency", "high");

        if use_webpush_bearer {
            req = req
                .header("Authorization", format!("WebPush {jwt}"))
                .header("Crypto-Key", format!("p256ecdsa={}", self.vapid_public_key));
        } else {
            req = req.header(
                "Authorization",
                format!("vapid t={jwt},k={}", self.vapid_public_key),
            );
        }

        req.body(encrypted)
            .send()
            .await
            .map_err(|e| AppError::Protocol(format!("push request: {e}")))
    }

    async fn deliver_one(
        &self,
        sub: &PushSubscriptionRecord,
        payload: &[u8],
        any_delivered: &mut bool,
    ) -> Result<()> {
        let jwt = self.build_vapid_jwt(&sub.endpoint)?;
        let encrypted = self.encrypt_payload(&sub.p256dh, &sub.auth, payload)?;

        let mut resp = self
            .send_push_request(&sub.endpoint, encrypted.clone(), &jwt, false)
            .await?;
        let mut status = resp.status();

        if status.as_u16() == 403 {
            let first_body = resp.text().await.unwrap_or_default();
            if first_body.contains("BadJwtToken") {
                tracing::warn!(
                    endpoint = %crate::utils::get_base_url(&sub.endpoint),
                    "push service rejected JWT in vapid header format, retrying with WebPush auth format"
                );
                resp = self
                    .send_push_request(&sub.endpoint, encrypted, &jwt, true)
                    .await?;
                status = resp.status();
            } else {
                let body = first_body.trim();
                let body_summary = if body.is_empty() {
                    String::new()
                } else {
                    let clipped: String = body.chars().take(300).collect();
                    if body.chars().count() > 300 {
                        format!(": {clipped}...")
                    } else {
                        format!(": {clipped}")
                    }
                };
                return Err(AppError::Protocol(format!(
                    "push service returned HTTP {}{}",
                    status.as_u16(),
                    body_summary
                )));
            }
        }

        if status.is_success() {
            *any_delivered = true;
        } else if status.as_u16() == 410 {
            tracing::info!(endpoint = %crate::utils::get_base_url(&sub.endpoint), "push subscription expired (410), removing");
            let _ = self.db.delete_push_subscription(&sub.endpoint).await;
        } else {
            let body = resp.text().await.unwrap_or_default();
            let body = body.trim();
            let body_summary = if body.is_empty() {
                String::new()
            } else {
                let clipped: String = body.chars().take(300).collect();
                if body.chars().count() > 300 {
                    format!(": {clipped}...")
                } else {
                    format!(": {clipped}")
                }
            };
            return Err(AppError::Protocol(format!(
                "push service returned HTTP {}{}",
                status.as_u16(),
                body_summary
            )));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split a hook command string into tokens using basic shell-word rules:
/// - Whitespace separates tokens.
/// - Single-quoted (`'…'`) content is taken literally.
/// - Double-quoted (`"…"`) content supports `\"` and `\\` escapes.
/// - Outside quotes, `\` escapes the next character.
///
/// Returns `None` if the result is empty (blank / all-whitespace input).
fn split_hook_command(s: &str) -> Option<Vec<String>> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Whitespace ends the current token (if any).
            ' ' | '\t' | '\n' | '\r' => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            // Single-quoted: everything until the closing `'` is literal.
            '\'' => {
                in_token = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            // Double-quoted: supports `\"` and `\\`; other characters are literal.
            '"' => {
                in_token = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                match next {
                                    '"' | '\\' => current.push(next),
                                    other => {
                                        current.push('\\');
                                        current.push(other);
                                    }
                                }
                            }
                        }
                        other => current.push(other),
                    }
                }
            }
            // Backslash outside quotes escapes the next character.
            '\\' => {
                in_token = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            other => {
                in_token = true;
                current.push(other);
            }
        }
    }

    if in_token {
        tokens.push(current);
    }

    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|e| AppError::Protocol(format!("base64url decode: {e}")))
}

fn validate_vapid_subject(subject: &str) -> Result<()> {
    let subject = subject.trim();
    if let Some(rest) = subject.strip_prefix("mailto:") {
        if !rest.contains('@') || rest.ends_with("@localhost") {
            return Err(AppError::Protocol(
                "invalid web_push_subject: use a real contact like mailto:you@example.com or an https URL"
                    .into(),
            ));
        }
        return Ok(());
    }

    let url = reqwest::Url::parse(subject).map_err(|e| {
        AppError::Protocol(format!(
            "invalid web_push_subject: expected mailto:... or https://... ({e})"
        ))
    })?;
    if url.scheme() != "https" {
        return Err(AppError::Protocol(
            "invalid web_push_subject: URL form must use https://".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::split_hook_command;

    #[test]
    fn test_simple_tokens() {
        assert_eq!(
            split_hook_command("/usr/bin/notify {summary} {body}"),
            Some(vec![
                "/usr/bin/notify".to_string(),
                "{summary}".to_string(),
                "{body}".to_string(),
            ])
        );
    }

    #[test]
    fn test_single_quoted_arg() {
        assert_eq!(
            split_hook_command("/bin/sh -c 'echo {summary}'"),
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo {summary}".to_string(),
            ])
        );
    }

    #[test]
    fn test_double_quoted_arg_with_escape() {
        assert_eq!(
            split_hook_command(r#"/bin/sh -c "echo \"hi\" {body}""#),
            Some(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo \"hi\" {body}".to_string(),
            ])
        );
    }

    #[test]
    fn test_backslash_escape() {
        assert_eq!(
            split_hook_command(r"/path/with\ spaces/tool arg"),
            Some(vec![
                "/path/with spaces/tool".to_string(),
                "arg".to_string(),
            ])
        );
    }

    #[test]
    fn test_empty_and_whitespace() {
        assert_eq!(split_hook_command(""), None);
        assert_eq!(split_hook_command("   "), None);
    }

    #[test]
    fn test_placeholder_substitution_in_all_tokens() {
        // Verify that substitute logic (done in run_hook) works as expected
        // by composing it here manually with the same replace chain.
        let tokens = split_hook_command("/usr/bin/curl -d {body} {summary}").unwrap();
        let body = "hello world";
        let summary = "test-summary";
        let result: Vec<String> = tokens
            .into_iter()
            .map(|t| t.replace("{body}", body).replace("{summary}", summary))
            .collect();
        assert_eq!(
            result,
            vec!["/usr/bin/curl", "-d", "hello world", "test-summary"]
        );
    }
}
