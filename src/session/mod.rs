pub(crate) mod persist;
pub(crate) mod ring;
mod runtime;
mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::protocol::SessionSummary;
pub use store::SessionStore;
pub use store::SilentCandidate;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    Stopping,
    Stopped,
    Failed,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: SessionStatus,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
}

pub struct StartSpec {
    pub title: Option<String>,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub notifications_enabled: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum SessionLookupError {
    Evicted,
    NotRunning,
}

impl SessionLookupError {
    pub fn message(self, id: &str) -> String {
        match self {
            Self::Evicted => format!("session evicted from memory: {id}"),
            Self::NotRunning => format!("session not running: {id}"),
        }
    }
}

pub struct SessionLiveSummary {
    pub summary: SessionSummary,
    pub last_output_at: Option<Instant>,
}
