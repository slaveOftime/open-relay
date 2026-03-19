use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 5;

/// Serde helper: transparently encode `Vec<u8>` as a base64 string in JSON.
/// This reduces wire size from ~4× (JSON integer arrays) to ~1.37× (base64).
mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD as B64};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(data: &Vec<u8>, ser: S) -> Result<S::Ok, S::Error> {
        B64.encode(data).serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = StringOrSeq::deserialize(de)?;
        match s {
            StringOrSeq::Str(s) => B64.decode(&s).map_err(serde::de::Error::custom),
            StringOrSeq::Seq(v) => Ok(v),
        }
    }

    /// Accept either a base64 string (v4+) or a JSON integer array (v3 compat).
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrSeq {
        Str(String),
        Seq(Vec<u8>),
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcEnvelope<T> {
    pub version: u16,
    pub payload: T,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcRequest {
    Health,
    DaemonStop {
        grace_seconds: u64,
    },
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
        #[serde(default)]
        disable_notifications: bool,
    },
    AttachSubscribe {
        id: String,
        from_byte_offset: Option<u64>,
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
    AttachDetach {
        id: String,
    },
    Stop {
        id: String,
        grace_seconds: u64,
    },
    Kill {
        id: String,
    },
    LogsSnapshot {
        id: String,
        tail: usize,
    },
    LogsPoll {
        id: String,
        cursor: u64,
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
    JoinList {
        primary: bool,
    },
    /// List all secondary nodes currently connected to this (primary) daemon.
    NodeList,
}

impl RpcRequest {
    pub fn name(&self) -> &'static str {
        match self {
            RpcRequest::Health => "health",
            RpcRequest::DaemonStop { .. } => "daemon_stop",
            RpcRequest::List { .. } => "list",
            RpcRequest::Start { .. } => "start",
            RpcRequest::AttachSubscribe { .. } => "attach_subscribe",
            RpcRequest::AttachInput { .. } => "attach_input",
            RpcRequest::AttachResize { .. } => "attach_resize",
            RpcRequest::AttachDetach { .. } => "attach_detach",
            RpcRequest::Stop { .. } => "stop",
            RpcRequest::Kill { .. } => "kill",
            RpcRequest::LogsSnapshot { .. } => "logs_snapshot",
            RpcRequest::LogsPoll { .. } => "logs_poll",
            RpcRequest::LogsWait { .. } => "logs_wait",
            RpcRequest::NodeProxy { .. } => "node_proxy",
            RpcRequest::ApiKeyAdd { .. } => "api_key_add",
            RpcRequest::ApiKeyList => "api_key_list",
            RpcRequest::ApiKeyRemove { .. } => "api_key_remove",
            RpcRequest::JoinStart { .. } => "join_start",
            RpcRequest::JoinStop { .. } => "join_stop",
            RpcRequest::JoinList { .. } => "join_list",
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
    /// Sent once after AttachSubscribe: ring tail replay from the canonical
    /// filtered session stream + terminal mode flags.
    AttachStreamInit {
        /// Filtered session-stream bytes, ready to write directly to the terminal.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// Canonical filtered-stream offset immediately after the replay bytes.
        end_offset: u64,
        running: bool,
        bracketed_paste_mode: bool,
        #[serde(default)]
        app_cursor_keys: bool,
    },
    /// Stream chunk of new canonical filtered PTY output, ready to write to the terminal.
    AttachStreamChunk {
        /// Canonical filtered-stream offset of the first byte in `data`.
        offset: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Terminal mode changed (bracketed-paste / app-cursor-keys) mid-stream.
    AttachModeChanged {
        bracketed_paste_mode: bool,
        #[serde(default)]
        app_cursor_keys: bool,
    },
    /// Another attached client resized the PTY; receivers should adapt.
    AttachResized {
        rows: u16,
        cols: u16,
    },
    /// Session ended; attach stream is done.
    AttachStreamDone {
        exit_code: Option<i32>,
    },
    Stop {
        stopped: bool,
    },
    Kill {
        killed: bool,
    },
    LogsSnapshot {
        lines: Vec<String>,
        cursor: u64,
        running: bool,
    },
    LogsPoll {
        lines: Vec<String>,
        cursor: u64,
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
    /// Secondary → Primary: single-shot RPC response (non-streaming).
    RpcResponse {
        id: String,
        response: serde_json::Value,
    },
    /// Secondary → Primary: one frame of a streaming RPC response.
    /// Multiple frames share the same `id`.  `done` is true on the final frame.
    RpcStreamFrame {
        id: String,
        response: serde_json::Value,
        #[serde(default)]
        done: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListSortField {
    Id,
    Title,
    Command,
    Cwd,
    Status,
    Pid,
    CreatedAt,
}

impl Default for ListSortField {
    fn default() -> Self {
        Self::CreatedAt
    }
}

impl ListSortField {
    pub fn sqlite_order_by(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Title => "LOWER(COALESCE(title, ''))",
            Self::Command => "LOWER(command)",
            Self::Cwd => "LOWER(COALESCE(cwd, ''))",
            Self::Status => "LOWER(status)",
            Self::Pid => "COALESCE(pid, -1)",
            Self::CreatedAt => "created_at",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    Asc,
    Desc,
}

impl Default for SortOrder {
    fn default() -> Self {
        Self::Desc
    }
}

impl SortOrder {
    pub fn sql(self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListQuery {
    pub search: Option<String>,
    pub statuses: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: usize,
    pub offset: usize,
    pub sort: ListSortField,
    pub order: SortOrder,
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
