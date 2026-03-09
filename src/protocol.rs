use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcEnvelope<T> {
    pub version: u16,
    pub payload: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcRequest {
    Health,
    DaemonStop,
    List {
        query: ListQuery,
    },
    Start {
        title: Option<String>,
        cmd: String,
        args: Vec<String>,
        cwd: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
    },
    AttachSnapshot {
        id: String,
    },
    AttachPoll {
        id: String,
        cursor: usize,
    },
    AttachInput {
        id: String,
        data: String,
    },
    AttachResize {
        id: String,
        rows: u16,
        cols: u16,
    },
    Stop {
        id: String,
        grace_seconds: u64,
    },
    LogsSnapshot {
        id: String,
        tail: usize,
    },
    LogsPoll {
        id: String,
        cursor: usize,
    },
    /// Block until the session emits an `InputNeeded` notification (or exits /
    /// times out), then return a snapshot.  Response is `LogsSnapshot`.
    LogsWait {
        id: String,
        tail: usize,
        timeout_secs: u64,
    },
    // ── Node federation ──────────────────────────────────────────────────────
    /// Proxy an inner request to a named secondary node.
    NodeProxy {
        node: String,
        inner: Box<RpcRequest>,
    },
    /// Register a new named API key on the primary; the daemon generates and
    /// returns the one-time plaintext key.
    ApiKeyAdd {
        name: String,
    },
    /// List all registered API keys.
    ApiKeyList,
    /// Remove a named API key.
    ApiKeyRemove {
        name: String,
    },
    /// Signal the daemon to start a persistent outbound join connector.
    JoinStart {
        url: String,
        name: String,
        key: String,
    },
    /// Signal the daemon to stop and remove an outbound join connector.
    JoinStop {
        name: String,
    },
    /// List active join connectors on this (secondary) daemon.
    JoinList,
    /// List all secondary nodes currently connected to this (primary) daemon.
    NodeList,
}

impl RpcRequest {
    pub fn name(&self) -> &'static str {
        match self {
            RpcRequest::Health => "health",
            RpcRequest::DaemonStop => "daemon_stop",
            RpcRequest::List { .. } => "list",
            RpcRequest::Start { .. } => "start",
            RpcRequest::AttachSnapshot { .. } => "attach_snapshot",
            RpcRequest::AttachPoll { .. } => "attach_poll",
            RpcRequest::AttachInput { .. } => "attach_input",
            RpcRequest::AttachResize { .. } => "attach_resize",
            RpcRequest::Stop { .. } => "stop",
            RpcRequest::LogsSnapshot { .. } => "logs_snapshot",
            RpcRequest::LogsPoll { .. } => "logs_poll",
            RpcRequest::LogsWait { .. } => "logs_wait",
            RpcRequest::NodeProxy { .. } => "node_proxy",
            RpcRequest::ApiKeyAdd { .. } => "api_key_add",
            RpcRequest::ApiKeyList => "api_key_list",
            RpcRequest::ApiKeyRemove { .. } => "api_key_remove",
            RpcRequest::JoinStart { .. } => "join_start",
            RpcRequest::JoinStop { .. } => "join_stop",
            RpcRequest::JoinList => "join_list",
            RpcRequest::NodeList => "node_list",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcResponse {
    Health {
        daemon_pid: u32,
    },
    DaemonStop {
        stopped: bool,
    },
    List {
        sessions: Vec<SessionSummary>,
    },
    Start {
        session_id: String,
    },
    AttachSnapshot {
        lines: Vec<String>,
        cursor: usize,
        running: bool,
        bracketed_paste_mode: bool,
        #[serde(default)]
        app_cursor_keys: bool,
    },
    AttachPoll {
        lines: Vec<String>,
        cursor: usize,
        running: bool,
        bracketed_paste_mode: bool,
        #[serde(default)]
        app_cursor_keys: bool,
    },
    Stop {
        stopped: bool,
    },
    LogsSnapshot {
        lines: Vec<String>,
        cursor: usize,
        running: bool,
    },
    LogsPoll {
        lines: Vec<String>,
        cursor: usize,
        running: bool,
    },
    Ack,
    Error {
        message: String,
    },
    // ── Node federation ──────────────────────────────────────────────────────
    /// Response to `ApiKeyAdd`: the one-time plaintext key.
    ApiKeyAdd {
        plaintext_key: String,
    },
    ApiKeyList {
        keys: Vec<ApiKeySummary>,
    },
    ApiKeyRemove {
        removed: bool,
    },
    JoinList {
        joins: Vec<JoinSummary>,
    },
    NodeList {
        nodes: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Federation types
// ---------------------------------------------------------------------------

/// A connected secondary node as seen by the primary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSummary {
    pub name: String,
    pub connected: bool,
}

/// A registered API key as reported by `oly api-key list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeySummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}

/// A persisted join config as reported to `oly join list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinSummary {
    pub name: String,
    pub primary_url: String,
    pub connected: bool,
}

/// Messages exchanged over the `/api/nodes/join` WebSocket connection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeWsMessage {
    /// Secondary → Primary: authentication handshake.
    Join {
        name: String,
        key: String,
    },
    /// Primary → Secondary: handshake accepted.
    Joined,
    /// Primary → Secondary: handshake rejected or fatal error.
    Error {
        message: String,
    },
    /// Primary → Secondary: forward an RPC request.
    Rpc {
        id: String,
        request: serde_json::Value,
    },
    /// Secondary → Primary: RPC response.
    RpcResponse {
        id: String,
        response: serde_json::Value,
    },
    /// Secondary -> Primary: notification event produced by the secondary daemon.
    Notification {
        kind: String,
        summary: String,
        body: String,
        session_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger_rule: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger_detail: Option<String>,
    },
    Ping,
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub pid: Option<u32>,
    pub status: String,
    pub age: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub input_needed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListQuery {
    pub search: Option<String>,
    pub statuses: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscriptionKeys {
    pub auth: String,
    pub p256dh: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscriptionInput {
    pub endpoint: String,
    pub keys: PushSubscriptionKeys,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct PushSubscriptionRecord {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}

impl ListQuery {
    pub fn matches(&self, summary: &SessionSummary) -> bool {
        if let Some(search) = self.search.as_deref() {
            let needle = search.to_ascii_lowercase();
            let id_match = summary.id.to_ascii_lowercase().contains(&needle);
            let title_match = summary
                .title
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&needle);
            let command_match = summary.command.to_ascii_lowercase().contains(&needle);
            let args_match = summary
                .args
                .join(" ")
                .to_ascii_lowercase()
                .contains(&needle);
            if !id_match && !title_match && !command_match && !args_match {
                return false;
            }
        }

        if !self.statuses.is_empty()
            && !self
                .statuses
                .iter()
                .any(|status| status.eq_ignore_ascii_case(&summary.status))
        {
            return false;
        }

        if let Some(since) = self.since {
            if summary.created_at < since {
                return false;
            }
        }

        if let Some(until) = self.until {
            if summary.created_at > until {
                return false;
            }
        }

        true
    }

    pub fn apply(&self, mut sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
        sessions.retain(|session| self.matches(session));
        sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let limit = self.limit.max(1);
        sessions.truncate(limit);
        sessions.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn make_summary(
        id: &str,
        title: Option<&str>,
        command: &str,
        args: &[&str],
        status: &str,
        created_at: DateTime<Utc>,
    ) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            title: title.map(|s| s.to_string()),
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            pid: None,
            status: status.to_string(),
            age: "0s".to_string(),
            created_at,
            cwd: None,
            input_needed: false,
        }
    }

    fn default_query() -> ListQuery {
        ListQuery {
            search: None,
            statuses: vec![],
            since: None,
            until: None,
            limit: 100,
        }
    }

    fn ts(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn test_matches_no_filter_accepts_all() {
        let summary = make_summary(
            "abc1234",
            None,
            "echo",
            &["hello"],
            "running",
            ts(2026, 1, 1),
        );
        assert!(default_query().matches(&summary));
    }

    #[test]
    fn test_matches_search_by_command() {
        let summary = make_summary(
            "abc1234",
            None,
            "echo",
            &["hello"],
            "running",
            ts(2026, 1, 1),
        );
        let q = ListQuery {
            search: Some("echo".to_string()),
            ..default_query()
        };
        assert!(q.matches(&summary));
        let q_no = ListQuery {
            search: Some("nonexistent".to_string()),
            ..default_query()
        };
        assert!(!q_no.matches(&summary));
    }

    #[test]
    fn test_matches_search_by_id() {
        let summary = make_summary("abc1234", None, "sh", &[], "running", ts(2026, 1, 1));
        let q = ListQuery {
            search: Some("abc12".to_string()),
            ..default_query()
        };
        assert!(q.matches(&summary));
    }

    #[test]
    fn test_matches_search_by_title() {
        let summary = make_summary(
            "abc1234",
            Some("my session"),
            "sh",
            &[],
            "running",
            ts(2026, 1, 1),
        );
        let q = ListQuery {
            search: Some("MY SESSION".to_string()),
            ..default_query()
        };
        assert!(q.matches(&summary));
    }

    #[test]
    fn test_matches_search_by_args() {
        let summary = make_summary(
            "abc1234",
            None,
            "git",
            &["commit", "-m", "fix"],
            "running",
            ts(2026, 1, 1),
        );
        let q = ListQuery {
            search: Some("commit".to_string()),
            ..default_query()
        };
        assert!(q.matches(&summary));
    }

    #[test]
    fn test_matches_status_filter() {
        let summary = make_summary("abc1234", None, "sh", &[], "running", ts(2026, 1, 1));
        let q_match = ListQuery {
            statuses: vec!["running".to_string()],
            ..default_query()
        };
        assert!(q_match.matches(&summary));
        let q_no = ListQuery {
            statuses: vec!["stopped".to_string()],
            ..default_query()
        };
        assert!(!q_no.matches(&summary));
    }

    #[test]
    fn test_matches_status_case_insensitive() {
        let summary = make_summary("abc1234", None, "sh", &[], "running", ts(2026, 1, 1));
        let q = ListQuery {
            statuses: vec!["RUNNING".to_string()],
            ..default_query()
        };
        assert!(q.matches(&summary));
    }

    #[test]
    fn test_matches_since_filter() {
        let summary = make_summary("abc1234", None, "sh", &[], "running", ts(2026, 3, 1));
        let q_pass = ListQuery {
            since: Some(ts(2026, 2, 1)),
            ..default_query()
        };
        assert!(q_pass.matches(&summary));
        let q_fail = ListQuery {
            since: Some(ts(2026, 4, 1)),
            ..default_query()
        };
        assert!(!q_fail.matches(&summary));
    }

    #[test]
    fn test_matches_until_filter() {
        let summary = make_summary("abc1234", None, "sh", &[], "running", ts(2026, 3, 1));
        let q_pass = ListQuery {
            until: Some(ts(2026, 4, 1)),
            ..default_query()
        };
        assert!(q_pass.matches(&summary));
        let q_fail = ListQuery {
            until: Some(ts(2026, 2, 1)),
            ..default_query()
        };
        assert!(!q_fail.matches(&summary));
    }

    #[test]
    fn test_apply_sorts_oldest_first() {
        let sessions = vec![
            make_summary("b", None, "sh", &[], "running", ts(2026, 3, 1)),
            make_summary("a", None, "sh", &[], "running", ts(2026, 1, 1)),
            make_summary("c", None, "sh", &[], "running", ts(2026, 2, 1)),
        ];
        let result = default_query().apply(sessions);
        assert_eq!(result[0].id, "a");
        assert_eq!(result[1].id, "c");
        assert_eq!(result[2].id, "b");
    }

    #[test]
    fn test_apply_respects_limit() {
        let sessions = (0..10)
            .map(|i| {
                make_summary(
                    &format!("id{i}"),
                    None,
                    "sh",
                    &[],
                    "running",
                    ts(2026, 1, i as u32 + 1),
                )
            })
            .collect::<Vec<_>>();
        let q = ListQuery {
            limit: 3,
            ..default_query()
        };
        // limit keeps the 3 newest (apply: sort desc, truncate, sort asc)
        let result = q.apply(sessions);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_apply_limit_zero_returns_one() {
        let sessions = vec![make_summary(
            "a",
            None,
            "sh",
            &[],
            "running",
            ts(2026, 1, 1),
        )];
        let q = ListQuery {
            limit: 0,
            ..default_query()
        };
        let result = q.apply(sessions);
        assert_eq!(result.len(), 1);
    }
}
