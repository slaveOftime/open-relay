use chrono::{DateTime, Utc};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::Instant;
use tracing::debug;

use crate::session::SessionEvent;

pub const PROTOCOL_VERSION: u16 = 7;
pub const NODE_WS_BINARY_COMPRESS_MIN_BYTES: usize = 256;
const NODE_WS_BINARY_MAGIC: &[u8; 4] = b"ONW1";

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct LogResize {
    pub offset: u64,
    pub rows: u16,
    pub cols: u16,
}

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

pub fn encode_node_ws_payload(message: &NodeWsMessage) -> std::io::Result<Vec<u8>> {
    let start = Instant::now();
    let message_type = node_ws_message_type(message);
    let json = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    let json_len = json.len();
    if json.len() < NODE_WS_BINARY_COMPRESS_MIN_BYTES {
        debug!(
            message_type,
            compressed = false,
            input_bytes = json_len,
            output_bytes = json_len,
            elapsed_us = start.elapsed().as_micros(),
            "encoded node WebSocket payload"
        );
        return Ok(json);
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&json)?;
    let compressed = encoder.finish()?;
    if compressed.len() >= json.len() {
        debug!(
            message_type,
            compressed = false,
            input_bytes = json_len,
            output_bytes = json_len,
            candidate_compressed_bytes = compressed.len(),
            elapsed_us = start.elapsed().as_micros(),
            "encoded node WebSocket payload"
        );
        return Ok(json);
    }

    let mut payload = Vec::with_capacity(NODE_WS_BINARY_MAGIC.len() + compressed.len());
    payload.extend_from_slice(NODE_WS_BINARY_MAGIC);
    payload.extend_from_slice(&compressed);
    debug!(
        message_type,
        compressed = true,
        input_bytes = json_len,
        compressed_bytes = compressed.len(),
        output_bytes = payload.len(),
        elapsed_us = start.elapsed().as_micros(),
        "encoded node WebSocket payload"
    );
    Ok(payload)
}

/// Hard cap on decompressed node WebSocket payload size (64 MB).
/// Prevents gzip-bomb attacks from malicious secondary nodes.
const MAX_NODE_WS_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

pub fn decode_node_ws_payload(payload: &[u8]) -> std::io::Result<NodeWsMessage> {
    let start = Instant::now();
    let payload_len = payload.len();
    let compressed = payload.starts_with(NODE_WS_BINARY_MAGIC);
    let json = if compressed {
        let decoder = GzDecoder::new(&payload[NODE_WS_BINARY_MAGIC.len()..]);
        let mut limited = decoder.take(MAX_NODE_WS_DECOMPRESSED_BYTES);
        let mut json = Vec::new();
        limited.read_to_end(&mut json)?;
        json
    } else {
        payload.to_vec()
    };

    let message = serde_json::from_slice(&json).map_err(std::io::Error::other)?;
    debug!(
        message_type = node_ws_message_type(&message),
        compressed,
        input_bytes = payload_len,
        decoded_json_bytes = json.len(),
        elapsed_us = start.elapsed().as_micros(),
        "decoded node WebSocket payload"
    );
    Ok(message)
}

fn node_ws_message_type(message: &NodeWsMessage) -> &'static str {
    match message {
        NodeWsMessage::Join { .. } => "join",
        NodeWsMessage::Joined { .. } => "joined",
        NodeWsMessage::Error { .. } => "error",
        NodeWsMessage::Rpc { .. } => "rpc",
        NodeWsMessage::RpcResponse { .. } => "rpc_response",
        NodeWsMessage::RpcStreamFrame { .. } => "rpc_stream_frame",
        NodeWsMessage::Notification { .. } => "notification",
        NodeWsMessage::SessionEvent { .. } => "session_event",
        NodeWsMessage::Ping => "ping",
        NodeWsMessage::Pong => "pong",
    }
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
        #[serde(default)]
        tags: Vec<String>,
        cmd: String,
        args: Vec<String>,
        cwd: Option<String>,
        rows: Option<u16>,
        cols: Option<u16>,
        #[serde(default)]
        disable_notifications: bool,
    },
    NotifySet {
        id: String,
        enabled: bool,
    },
    NotifySend {
        source: Option<String>,
        title: String,
        description: Option<String>,
        body: Option<String>,
        url: Option<String>,
    },
    AttachSubscribe {
        id: String,
        from_byte_offset: Option<u64>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
    },
    AttachInput {
        id: String,
        data: String,
        wait_for_change: bool,
    },
    AttachBusy {
        id: String,
    },
    UploadFile {
        id: String,
        path: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        #[serde(default)]
        dedupe: bool,
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
    LogsTail {
        id: String,
        tail: usize,
        keep_color: bool,
        term_cols: u16,
        #[serde(default)]
        from_file: bool,
    },
    LogsPagination {
        id: String,
        offset: Option<usize>,
        limit: usize,
    },
    /// Block until the session emits an `InputNeeded` notification (or exits /
    /// times out), then return a snapshot.  Response is `LogsTail`.
    LogsWait {
        id: String,
        timeout_ms: u64,
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
            RpcRequest::NotifySet { .. } => "notify_set",
            RpcRequest::NotifySend { .. } => "notify_send",
            RpcRequest::AttachSubscribe { .. } => "attach_subscribe",
            RpcRequest::AttachInput { .. } => "attach_input",
            RpcRequest::AttachBusy { .. } => "attach_busy",
            RpcRequest::UploadFile { .. } => "upload_file",
            RpcRequest::AttachResize { .. } => "attach_resize",
            RpcRequest::AttachDetach { .. } => "attach_detach",
            RpcRequest::Stop { .. } => "stop",
            RpcRequest::Kill { .. } => "kill",
            RpcRequest::LogsTail { .. } => "logs_tail",
            RpcRequest::LogsPagination { .. } => "logs_pagination",
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
    Empty,
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
    /// Sent once after AttachSubscribe: a terminal-state snapshot describing the
    /// current visible screen, followed by terminal mode flags.
    AttachStreamInit {
        /// Terminal bytes that recreate the current visible session state when
        /// written into a fresh terminal instance.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        /// Canonical filtered-stream offset immediately after the snapshot point.
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
    LogsTail {
        output: Vec<u8>,
        #[serde(default)]
        resizes: Vec<LogResize>,
    },
    LogsPagination {
        offset: usize,
        lines: Vec<String>,
        total: usize,
        #[serde(default)]
        resizes: Vec<LogResize>,
    },
    UploadFile {
        path: String,
        bytes: usize,
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
        title: String,
        description: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        navigation_url: Option<String>,
        session_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger_rule: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger_detail: Option<String>,
        last_total_bytes: u64,
    },
    /// Secondary -> Primary: node-aware session event produced for SSE delivery.
    SessionEvent {
        payload: SessionEvent,
    },
    Ping,
    Pong,
}

#[cfg(test)]
mod tests {
    use super::{NodeWsMessage, decode_node_ws_payload, encode_node_ws_payload};

    #[test]
    fn node_ws_payload_round_trips_uncompressed_binary_json() {
        let message = NodeWsMessage::Join {
            name: "worker-a".into(),
            key: "secret".into(),
        };

        let payload = encode_node_ws_payload(&message).expect("encode payload");
        let decoded = decode_node_ws_payload(&payload).expect("decode payload");

        match decoded {
            NodeWsMessage::Join { name, key } => {
                assert_eq!(name, "worker-a");
                assert_eq!(key, "secret");
            }
            other => panic!("unexpected decoded message: {other:?}"),
        }
    }

    #[test]
    fn node_ws_payload_round_trips_compressed_binary_json() {
        let message = NodeWsMessage::Notification {
            kind: "input_needed".into(),
            title: "x".repeat(256),
            description: "y".repeat(256),
            body: "z".repeat(512),
            navigation_url: Some("/session/abc".into()),
            session_ids: vec!["abc".into(), "def".into()],
            trigger_rule: Some("always".into()),
            trigger_detail: Some("detail".into()),
            last_total_bytes: 0,
        };

        let payload = encode_node_ws_payload(&message).expect("encode payload");
        let decoded = decode_node_ws_payload(&payload).expect("decode payload");

        match decoded {
            NodeWsMessage::Notification { title, body, .. } => {
                assert_eq!(title.len(), 256);
                assert_eq!(body.len(), 512);
            }
            other => panic!("unexpected decoded message: {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub pid: Option<u32>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub input_needed: bool,
    #[serde(default)]
    pub notifications_enabled: bool,
    pub node: Option<String>,
    pub last_total_bytes: u64,
    pub last_output_epoch: Option<DateTime<Utc>>,
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
    pub tags: Vec<String>,
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
