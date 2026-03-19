pub(crate) mod cursor_tracker;
pub mod logs;
pub(crate) mod mode_tracker;
pub(crate) mod persist;
pub mod pty;
pub(crate) mod resize;
pub(crate) mod ring;
mod runtime;
mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tokio::sync::broadcast;

use crate::protocol::SessionSummary;
pub use store::SessionStore;
pub use store::SilentCandidate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    Stopping,
    Stopped,
    Killed,
    Failed,
}

impl SessionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Killed => "killed",
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
    Busy,
}

impl SessionLookupError {
    pub fn message(self, id: &str) -> String {
        match self {
            Self::Evicted => format!("session evicted from memory: {id}"),
            Self::NotRunning => format!("session not running: {id}"),
            Self::Busy => format!("session input queue is full: {id}"),
        }
    }
}

pub struct SessionLiveSummary {
    pub summary: SessionSummary,
    pub last_output_at: Option<Instant>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum SessionEvent {
    SessionCreated(SessionSummary),
    SessionUpdated(SessionSummary),
    SessionDeleted {
        id: String,
    },
    SessionNotification {
        kind: String,
        summary: String,
        body: String,
        session_ids: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_rule: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_detail: Option<String>,
    },
}

pub type SessionEventTx = broadcast::Sender<SessionEvent>;
