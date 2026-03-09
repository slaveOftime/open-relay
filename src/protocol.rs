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
        total: usize,
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
    pub offset: usize,
    pub sort: Option<String>,
    pub order: Option<String>,
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
