use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::{
    error::{AppError, Result},
    federation_crypto,
};

/// The remote peer's role, not a global role of this daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    Primary,
    Secondary,
}

impl NodeRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub name: String,
    pub role: NodeRole,
    pub key: String,
    pub url: Option<String>,
    #[serde(rename = "as")]
    pub as_name: Option<String>,
    pub enabled: bool,
    /// Fences handshakes that began before a configuration mutation.
    #[serde(skip)]
    pub revision: String,
}

impl NodeConfig {
    pub fn validate(&mut self) -> Result<()> {
        validate_name(&self.name)?;
        self.key = federation_crypto::parse_public_key(self.key.trim())?.to_line();
        match self.role {
            NodeRole::Primary => {
                validate_name(self.as_name.as_deref().ok_or_else(|| {
                    AppError::Protocol("primary peers require --as <local-name>".into())
                })?)?;
                let raw = self.url.as_deref().ok_or_else(|| {
                    AppError::Protocol("primary peers require --url <url>".into())
                })?;
                let url = reqwest::Url::parse(raw)
                    .map_err(|e| AppError::Protocol(format!("invalid node URL: {e}")))?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(AppError::Protocol("node URL must be an http(s) base URL without credentials, query, or fragment".into()));
                }
                self.url = Some(url.to_string().trim_end_matches('/').to_string());
            }
            NodeRole::Secondary if self.url.is_some() || self.as_name.is_some() => {
                return Err(AppError::Protocol(
                    "secondary peers do not accept --url or --as".into(),
                ));
            }
            NodeRole::Secondary => {}
        }
        Ok(())
    }
}

/// Validated, derived parameters for an outbound connector (not another store).
pub(crate) struct PrimaryConnection {
    pub name: String,
    pub as_name: String,
    pub url: String,
    pub key: String,
}

impl TryFrom<&NodeConfig> for PrimaryConnection {
    type Error = AppError;
    fn try_from(config: &NodeConfig) -> Result<Self> {
        if config.role != NodeRole::Primary {
            return Err(AppError::Protocol(
                "only primary peers have outbound connectors".into(),
            ));
        }
        let mut config = config.clone();
        config.validate()?;
        Ok(Self {
            name: config.name,
            as_name: config.as_name.unwrap(),
            url: config.url.unwrap(),
            key: config.key,
        })
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 256
        || name.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(AppError::Protocol(
            "node names must be 1–256 bytes without whitespace or control characters".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeUpdate {
    pub key: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "as")]
    pub as_name: Option<String>,
}

impl NodeUpdate {
    pub fn apply(self, config: &mut NodeConfig) -> Result<()> {
        if self.key.is_none() && self.url.is_none() && self.as_name.is_none() {
            return Err(AppError::Protocol(
                "node update requires --key, --url, or --as".into(),
            ));
        }
        if let Some(key) = self.key {
            config.key = key;
        }
        if let Some(url) = self.url {
            config.url = Some(url);
        }
        if let Some(name) = self.as_name {
            config.as_name = Some(name);
        }
        config.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Connected,
    Connecting,
    Retrying,
    Waiting,
    Disabled,
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Connecting => "connecting",
            Self::Retrying => "retrying",
            Self::Waiting => "waiting",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    #[serde(flatten)]
    pub config: NodeConfig,
    pub status: NodeStatus,
    pub last_error: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
}

/// Live outbound state. Never written to the database.
#[derive(Debug, Clone)]
pub(crate) struct ConnectorState {
    pub status: NodeStatus,
    pub last_error: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
}

impl Default for ConnectorState {
    fn default() -> Self {
        Self {
            status: NodeStatus::Connecting,
            last_error: None,
            next_retry_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(role: NodeRole) -> NodeConfig {
        let signing = ed25519_dalek::SigningKey::from_bytes(&[31; 32]);
        NodeConfig {
            name: "peer".into(),
            role,
            key: federation_crypto::public_key_line(&signing),
            url: None,
            as_name: None,
            enabled: true,
            revision: String::new(),
        }
    }
    #[test]
    fn primary_requires_endpoint_and_advertised_name() {
        let mut c = config(NodeRole::Primary);
        assert!(c.validate().is_err());
        c.as_name = Some("local".into());
        for url in [
            "ftp://host",
            "http://user:pass@host",
            "http://host?q=1",
            "http://host#fragment",
            "not-url",
        ] {
            c.url = Some(url.into());
            assert!(c.validate().is_err(), "{url}");
        }
        c.url = Some("https://host/prefix/".into());
        c.validate().unwrap();
        assert_eq!(c.url.as_deref(), Some("https://host/prefix"));
    }
    #[test]
    fn secondary_rejects_connection_parameters_and_updates() {
        let mut c = config(NodeRole::Secondary);
        c.validate().unwrap();
        assert!(NodeUpdate::default().apply(&mut c).is_err());
        assert!(
            NodeUpdate {
                url: Some("http://host".into()),
                ..Default::default()
            }
            .apply(&mut c)
            .is_err()
        );
    }
    #[test]
    fn invalid_names_and_keys_are_rejected() {
        for name in ["", "a b", "a\nb", "a\u{7f}b"] {
            assert!(validate_name(name).is_err());
        }
        let mut c = config(NodeRole::Secondary);
        c.key = "bad".into();
        assert!(c.validate().is_err());
    }
}
