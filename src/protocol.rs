use chrono::{DateTime, Utc};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::Instant;
use tracing::debug;

use crate::session::SessionEvent;

// 12: attach streams carry PTY output as binary length-delimited frames
// after the JSON init line (M6-3, ADR-0004); pre-12 peers expect base64
// JSON chunks and are rejected.
// 13: `AttachInput.data` is raw bytes (base64 in JSON) instead of a JSON
// string — PTY input is byte-exact end to end, so invalid UTF-8 (binary
// paste, `hex:` specs, piped files) can no longer be mangled by a
// lossy UTF-8 round-trip. Mode frames also carry mouse/focus flags.
pub const PROTOCOL_VERSION: u16 = 13;
pub const NODE_WS_BINARY_COMPRESS_MIN_BYTES: usize = 256;
const NODE_WS_BINARY_MAGIC: &[u8; 4] = b"ONW1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
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
        NodeWsMessage::Joined => "joined",
        NodeWsMessage::Error { .. } => "error",
        NodeWsMessage::Rpc { .. } => "rpc",
        NodeWsMessage::RpcResponse { .. } => "rpc_response",
        NodeWsMessage::RpcStreamFrame { .. } => "rpc_stream_frame",
        NodeWsMessage::RpcStreamMessage { .. } => "rpc_stream_message",
        NodeWsMessage::Notification { .. } => "notification",
        NodeWsMessage::SessionEvent { .. } => "session_event",
        NodeWsMessage::Ping => "ping",
        NodeWsMessage::Pong => "pong",
    }
}

fn default_api_key_scopes() -> String {
    "node".to_string()
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
    SessionMetadataSet {
        id: String,
        title: Option<String>,
        tags: Option<Vec<String>>,
        #[serde(default)]
        notifications_enabled: Option<bool>,
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
        /// Whether this stream reports applied-cursor credits and is
        /// therefore gated on them (M5-1, I7). Local interactive clients
        /// set `true` and ack; node-relayed subscriptions must set `false`
        /// because the relay cannot forward mid-stream credits (fail-open
        /// until direct remote streams land in M5-2). Defaults to `false`
        /// (safe: no enforcement) when absent.
        #[serde(default)]
        credited: bool,
        /// Incarnation the `from_byte_offset` cursor was issued by. Resume is
        /// only valid within the same incarnation; a mismatch is rejected with
        /// a precise stale-cursor error (PLAN §7.3). Required when
        /// `from_byte_offset` is set.
        #[serde(default)]
        incarnation: Option<u64>,
        /// Requested control role: "observer" | "controller" (default) |
        /// "takeover" (PLAN §8.1). Unknown values are rejected.
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
    },
    AttachInput {
        id: String,
        /// Raw PTY input bytes (base64 on the JSON wire): input is
        /// byte-exact end to end — no UTF-8 validation or lossy
        /// conversion anywhere on the path (PLAN §5.1).
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        wait_for_change: bool,
        /// Fencing token of the sending attachment (streaming attach input);
        /// `None` is the ungated operator one-shot path (`oly send`).
        #[serde(default)]
        attachment_id: Option<u64>,
    },
    /// Machine-readable session cursor (M4 agent surface): incarnation,
    /// current filtered-stream offset, and liveness in one cheap call.
    SessionCursor {
        id: String,
    },
    /// Bounded window read of the filtered stream (M4): never unbounded,
    /// resumable via the returned `next_offset`.
    ObserveWindow {
        id: String,
        from: u64,
        max_bytes: u32,
    },
    /// Verify sealed-part journal manifests (M4 doctor): `None` = all sessions.
    Doctor {
        id: Option<String>,
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
    /// Take over the session's control lease from an attached observer
    /// position (streaming attach connections only).
    AttachAcquireControl {
        id: String,
    },
    /// Report the applied-cursor credit for this attach connection (M3-5,
    /// I7): the highest stream offset the client has rendered. Handled on
    /// the streaming path only.
    AttachAppliedCursor {
        id: String,
        cursor: u64,
    },
    Stop {
        id: String,
        grace_seconds: u64,
    },
    Restart {
        id: String,
        #[serde(default)]
        force: bool,
    },
    Kill {
        id: String,
    },
    /// Delete a session: remove its DB row, on-disk directory, and any
    /// in-memory runtime.  `force` also terminates a still-running session
    /// before deleting; without it, a running session is rejected.
    Remove {
        id: String,
        #[serde(default)]
        force: bool,
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
        /// Comma-separated scope list (ADR-0007). Defaults to `node` to
        /// preserve the historical node-join use of API keys.
        #[serde(default = "default_api_key_scopes")]
        scopes: String,
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
            RpcRequest::SessionMetadataSet { .. } => "session_metadata_set",
            RpcRequest::NotifySet { .. } => "notify_set",
            RpcRequest::NotifySend { .. } => "notify_send",
            RpcRequest::AttachSubscribe { .. } => "attach_subscribe",
            RpcRequest::AttachInput { .. } => "attach_input",
            RpcRequest::SessionCursor { .. } => "session_cursor",
            RpcRequest::ObserveWindow { .. } => "observe_window",
            RpcRequest::Doctor { .. } => "doctor",
            RpcRequest::AttachBusy { .. } => "attach_busy",
            RpcRequest::UploadFile { .. } => "upload_file",
            RpcRequest::AttachResize { .. } => "attach_resize",
            RpcRequest::AttachDetach { .. } => "attach_detach",
            RpcRequest::AttachAcquireControl { .. } => "attach_acquire_control",
            RpcRequest::AttachAppliedCursor { .. } => "attach_applied_cursor",
            RpcRequest::Stop { .. } => "stop",
            RpcRequest::Restart { .. } => "restart",
            RpcRequest::Kill { .. } => "kill",
            RpcRequest::Remove { .. } => "remove",
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
    /// Machine-readable session cursor answer (M4).
    SessionCursor {
        running: bool,
        exit_code: Option<i32>,
        /// Canonical filtered-stream length (bytes available to read).
        offset: u64,
        incarnation: Option<u64>,
    },
    /// One bounded filtered-stream window (M4).
    ObserveWindow {
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        next_offset: u64,
        running: bool,
        exit_code: Option<i32>,
        incarnation: Option<u64>,
    },
    /// Journal verification report (M4 doctor).
    Doctor {
        results: Vec<DoctorReport>,
    },
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
    Session {
        summary: SessionSummary,
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
        /// Child has mouse reporting enabled (any of 1000/1002/1003).
        #[serde(default)]
        mouse_report: bool,
        /// Child negotiated SGR (1006) mouse encoding.
        #[serde(default)]
        sgr_mouse: bool,
        /// Child has focus in/out reporting (1004) enabled.
        #[serde(default)]
        focus_events: bool,
        /// Rendered scrolled-off rows (color, `\n`-terminated), at most the
        /// client's screen height, which a CLI client prints before the
        /// snapshot so the terminal scrollbar covers pre-attach history.
        /// The visible screen is excluded (the snapshot covers it).  Empty
        /// for alternate-screen sessions, sessions with no scrolled-off rows,
        /// piped attaches, and older daemons.
        #[serde(default, with = "base64_bytes")]
        scrollback: Vec<u8>,
        /// Journal incarnation the snapshot and `end_offset` cursor belong
        /// to; 0 when the session predates journaling. A later resume must
        /// present the same incarnation (ADR-0004).
        #[serde(default)]
        incarnation: u64,
        /// This attachment's fencing token (M3-4).
        #[serde(default)]
        attachment_id: u64,
        /// Granted control role: "controller" or "observer".
        #[serde(default)]
        role: String,
    },
    /// Control handoff notice pushed mid-stream: this attachment's role
    /// after the change ("controller" or "observer").
    AttachControlChanged {
        role: String,
    },
    /// Stream chunk of new canonical filtered PTY output, ready to write to the terminal.
    AttachStreamChunk {
        /// Canonical filtered-stream offset of the first byte in `data`.
        offset: u64,
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
    },
    /// Terminal mode changed (bracketed-paste / app-cursor-keys /
    /// mouse reporting / focus events) mid-stream.
    AttachModeChanged {
        bracketed_paste_mode: bool,
        #[serde(default)]
        app_cursor_keys: bool,
        /// The child application enabled mouse reporting (any of
        /// 1000/1002/1003). Attach clients should capture and forward
        /// mouse events while set.
        #[serde(default)]
        mouse_report: bool,
        /// The child negotiated SGR (1006) mouse encoding; otherwise
        /// legacy X11 encoding applies.
        #[serde(default)]
        sgr_mouse: bool,
        /// The child application enabled focus in/out reporting (1004).
        #[serde(default)]
        focus_events: bool,
    },
    /// Another attached client resized the PTY; receivers should adapt.
    AttachResized {
        rows: u16,
        cols: u16,
    },
    /// Session ended; attach stream is done.
    AttachStreamDone {
        exit_code: Option<i32>,
        /// Canonical filtered-stream offset of the end of the stream; the
        /// client must have applied exactly up to here (I2 completion).
        #[serde(default)]
        final_offset: u64,
    },
    Stop {
        stopped: bool,
    },
    Restart {
        source_id: String,
        session_id: String,
    },
    Kill {
        killed: bool,
    },
    Remove {
        removed: bool,
    },
    LogsTail {
        output: Vec<u8>,
        #[serde(default)]
        resizes: Vec<LogResize>,
        #[serde(default)]
        status: Option<String>,
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
    /// Comma-separated scope list (ADR-0007).
    #[serde(default)]
    pub scopes: String,
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
    /// Primary → Secondary: one mid-stream client message for an open
    /// streaming RPC (M5-2). Carries attach input, resize,
    /// applied-cursor credits, control takeover, and detach to the owning
    /// node's stream task, so remote attachments get the same
    /// attachment-scoped fencing and enforced credits as local ones.
    RpcStreamMessage {
        id: String,
        request: serde_json::Value,
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
        enabled_for_channels: bool,
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
    use super::{
        NodeWsMessage, RpcRequest, RpcResponse, decode_node_ws_payload, encode_node_ws_payload,
    };

    #[test]
    fn restart_protocol_round_trips_and_names_request() {
        let request = RpcRequest::Restart {
            id: "old123".into(),
            force: true,
        };
        assert_eq!(request.name(), "restart");
        let json = serde_json::to_string(&request).unwrap();
        let decoded: RpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, RpcRequest::Restart { id, force } if id == "old123" && force));

        let response = RpcResponse::Restart {
            source_id: "old123".into(),
            session_id: "new456".into(),
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: RpcResponse = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(decoded, RpcResponse::Restart { source_id, session_id } if source_id == "old123" && session_id == "new456")
        );
    }

    #[test]
    fn attach_stream_init_without_scrollback_defaults_to_empty() {
        // Daemons older than the scrollback-seeding change omit the field;
        // mixed-version federation setups must still decode the frame.
        let json = r#"{
            "type": "attach_stream_init",
            "data": "",
            "end_offset": 7,
            "running": true,
            "bracketed_paste_mode": false,
            "app_cursor_keys": false
        }"#;

        let response: super::RpcResponse = serde_json::from_str(json).expect("decode init frame");
        match response {
            super::RpcResponse::AttachStreamInit {
                scrollback,
                end_offset,
                ..
            } => {
                assert!(scrollback.is_empty());
                assert_eq!(end_offset, 7);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

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
            enabled_for_channels: false,
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
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub attach_count: usize,
    /// Default foreground colour the session last set via OSC 10 (raw colour
    /// spec, e.g. `rgb:ffff/ffff/ffff` or `#ffffff`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground_color: Option<String>,
    /// Default background colour the session last set via OSC 11 (raw colour
    /// spec).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_color: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ListSortField {
    Id,
    Title,
    Command,
    Cwd,
    Status,
    Pid,
    #[default]
    CreatedAt,
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
#[derive(Default)]
pub enum SortOrder {
    Asc,
    #[default]
    Desc,
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

/// Per-session journal verification result (M4 doctor).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub id: String,
    /// Number of sealed parts covered by the manifest.
    pub sealed_parts: usize,
    /// Integrity issues found (empty = clean).
    pub issues: Vec<String>,
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
