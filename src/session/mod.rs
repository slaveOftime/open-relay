//! Session management: live runtimes, their on-disk state and the shared
//! metadata types the rest of the daemon speaks in.

pub(crate) mod file;
// The single canonical per-session recording (PLAN.md §6.1, ADR-0002):
// record format, segment writer/reader, torn-tail recovery, the bounded
// journal appender, ordered resize/lifecycle records, fixed-range/tail/
// history reads and checkpoint-gated retention. Always on; if it fails,
// the session fails loudly.
pub(crate) mod journal;
pub mod logs;
pub mod pty;
pub mod registry;
pub(crate) mod replay;
pub(crate) mod resize;
mod runtime;
pub(crate) mod scan;
#[cfg(not(test))]
mod store;
#[cfg(test)]
pub(crate) mod store;
#[cfg(test)]
pub(crate) use runtime::SequencedChunk;
pub use store::pump::{AttachEvent, AttachPump, PumpCredit};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Instant;
use tokio::sync::broadcast;

use crate::error::{AppError, Result};
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
    pub tags: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: SessionStatus,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub notifications_enabled: bool,
    /// Default foreground colour the child last set via OSC 10, as the raw
    /// colour spec payload. `None` when the session never set one.
    pub foreground_color: Option<String>,
    /// Default background colour the child last set via OSC 11, as the raw
    /// colour spec payload. `None` when the session never set one.
    pub background_color: Option<String>,
}

pub struct StartSpec {
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub notifications_enabled: bool,
}

#[derive(Debug, Clone)]
pub enum SessionError {
    Evicted,
    NotRunning,
    Busy,
    Persistence(String),
    /// A resume cursor named a different journal incarnation than the
    /// session's current one; the client must resnapshot (ADR-0004).
    StaleCursor {
        requested: Option<u64>,
        current: Option<u64>,
    },
    /// An attached client tried to drive geometry/input without holding
    /// the session's control lease (I6; PLAN §8.1).
    NotController,
    /// The attachment id is unknown to the session (stale fencing token).
    StaleAttachment,
}

impl SessionError {
    pub fn message(&self, id: &str) -> String {
        match self {
            Self::Evicted => format!("session evicted from memory: {id}"),
            Self::NotRunning => format!("session not running: {id}"),
            Self::Busy => format!("session input queue is full: {id}"),
            Self::Persistence(message) => message.clone(),
            Self::StaleCursor { requested, current } => format!(
                "stale resume cursor for {id}: cursor names incarnation {requested:?},                  session incarnation is {current:?}; resnapshot instead of resuming"
            ),
            Self::NotController => format!(
                "not the controller of session {id}: attached as observer;                  take over control to send input or resize"
            ),
            Self::StaleAttachment => {
                format!("attachment is no longer registered for session {id}; re-attach")
            }
        }
    }
}

pub struct SessionLiveSummary {
    pub summary: SessionSummary,
    pub last_output_at: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum SessionEvent {
    SessionCreated(SessionSummary),
    SessionUpdated(SessionSummary),
    SessionDeleted {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
    },
    SessionNotification {
        kind: String,
        title: String,
        description: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        navigation_url: Option<String>,
        session_ids: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_rule: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        trigger_detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node: Option<String>,
        last_total_bytes: u64,
        enabled_for_channels: bool,
    },
}

pub type SessionEventTx = broadcast::Sender<SessionEvent>;

pub const MAX_SESSION_TITLE_LEN: usize = 256;

pub fn normalize_session_title(title: Option<String>) -> Option<String> {
    title.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub fn normalize_session_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for tag in tags {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_ascii_lowercase();
        if seen.insert(key) {
            normalized.push(trimmed.to_string());
        }
    }
    normalized
}

pub fn validate_session_metadata(
    title: Option<String>,
    tags: Vec<String>,
) -> Result<(Option<String>, Vec<String>)> {
    let title = validate_normalized_session_title(normalize_session_title(title))?;
    Ok((title, normalize_session_tags(tags)))
}

fn validate_normalized_session_title(title: Option<String>) -> Result<Option<String>> {
    if let Some(title) = title.as_ref()
        && title.chars().count() > MAX_SESSION_TITLE_LEN
    {
        return Err(AppError::Protocol(format!(
            "session title is too long (max {MAX_SESSION_TITLE_LEN} characters)"
        )));
    }

    Ok(title)
}

pub fn validate_session_metadata_update(
    title: Option<String>,
    tags: Option<Vec<String>>,
) -> Result<(Option<String>, Option<Vec<String>>)> {
    let title = validate_normalized_session_title(normalize_session_title(title))?;
    let tags = tags.map(normalize_session_tags);
    Ok((title, tags))
}
