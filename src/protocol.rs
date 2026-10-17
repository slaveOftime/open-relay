use chrono::{DateTime, Utc};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::time::Instant;
use tracing::debug;

use crate::session::SessionEvent;

// 12: attach streams carry PTY output as binary length-delimited frames
// after the JSON init line (ADR-0004); pre-12 peers expect base64
// JSON chunks and are rejected.
// 13: `AttachInput.data` is raw bytes (base64 in JSON) instead of a JSON
// string — PTY input is byte-exact end to end, so invalid UTF-8 (binary
// paste, `hex:` specs, piped files) can no longer be mangled by a
// lossy UTF-8 round-trip. Mode frames also carry mouse/focus flags.
// 14: federation uses the sole nonce-bound node identity handshake; API-key
// RPCs and legacy federation fields are removed.
pub const PROTOCOL_VERSION: u16 = 15;
pub const FEDERATION_VERSION: u16 = 2;
pub const NODE_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub const MAX_NODE_HANDSHAKE_BYTES: usize = crate::federation_crypto::MAX_HANDSHAKE_BYTES;
pub const NODE_WS_BINARY_COMPRESS_MIN_BYTES: usize = 256;
const NODE_WS_BINARY_MAGIC: &[u8; 4] = b"ONW1";

/// A position in a session's canonical output stream, as handed to
/// `--since`. The incarnation is part of the position: a restarted session
/// reuses offset numbering for a different stream, so reads carrying a
/// stale incarnation are rejected instead of silently misaligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamPosition {
    pub incarnation: u64,
    pub offset: u64,
}

/// One captured screen snapshot from the screen-snapshot history
/// (`oly logs --tail-frames`): the visible screen the TUI held at the
/// capture point, rendered at the recorded viewport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFrame {
    /// The stream position the snapshot was captured at: every byte at
    /// or before `position.offset` is guaranteed to have contributed to
    /// this frame, so hand the token back to `--since` to read the next
    /// batch of history without skipping paint states.
    pub position: StreamPosition,
    pub rows: u16,
    pub cols: u16,
    /// Rendered rows (SGR-styled with `keep_color`), newline-joined.
    #[serde(with = "base64_bytes")]
    pub content: Vec<u8>,
}

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
        let mut limited = decoder.take(MAX_NODE_WS_DECOMPRESSED_BYTES + 1);
        let mut json = Vec::new();
        limited.read_to_end(&mut json)?;
        json
    } else {
        payload.to_vec()
    };

    if json.len() as u64 > MAX_NODE_WS_DECOMPRESSED_BYTES {
        return Err(std::io::Error::other(
            "node payload exceeds decompressed limit",
        ));
    }
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

impl crate::federation_crypto::Channel {
    pub fn encode_message(&mut self, message: &NodeWsMessage) -> crate::error::Result<Vec<u8>> {
        self.encode(&encode_node_ws_payload(message)?)
    }

    pub fn decode_message(&mut self, frame: &[u8]) -> crate::error::Result<NodeWsMessage> {
        self.decode(frame, |bytes| {
            decode_node_ws_payload(bytes).map_err(crate::error::AppError::from)
        })
    }
}

fn node_ws_message_type(message: &NodeWsMessage) -> &'static str {
    match message {
        NodeWsMessage::Join { .. } => "join",
        NodeWsMessage::Joined => "joined",
        NodeWsMessage::Hello { .. } => "hello",
        NodeWsMessage::Challenge { .. } => "challenge",
        NodeWsMessage::KeyConfirm { .. } => "key_confirm",
        NodeWsMessage::Ready { .. } => "ready",
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcRequest {
    Health,
    /// Read-only diagnostic summary of this daemon (also proxyable to a
    /// secondary via `NodeProxy`): version, liveness vs readiness,
    /// supervised-component states, disk capacity, failure-record
    /// locations, and affected sessions. Never carries terminal content,
    /// input, environment, or secrets.
    DaemonStatus,
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
    SessionInfo {
        id: String,
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
        /// therefore gated on them (credit-gated). The CLI and web clients
        /// set `true` and ack; node-relayed subscriptions carry the client's
        /// flag and the relay forwards `AttachAppliedCursor` mid-stream, so
        /// the owning node gates remote clients too. Defaults to `false`
        /// (fail-open: no enforcement) when absent.
        #[serde(default)]
        credited: bool,
        /// Incarnation the `from_byte_offset` cursor was issued by. Resume is
        /// only valid within the same incarnation; a mismatch is rejected with
        /// a precise stale-cursor error. Required when `from_byte_offset` is
        /// set.
        #[serde(default)]
        incarnation: Option<u64>,
        /// Requested attach mode: "observe" (view-only) or absent /
        /// "controller" (default: drives input and geometry).
        /// Unknown values are rejected.
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
        /// conversion anywhere on the path.
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        wait_for_change: bool,
        /// Fencing token of the sending attachment (streaming attach input);
        /// `None` is the ungated operator one-shot path (`oly send`).
        #[serde(default)]
        attachment_id: Option<u64>,
    },
    /// Ungated operator mouse input. The owning daemon parses `spec` (the
    /// part after `mouse:`) and resolves live modes and viewport geometry.
    AttachMouse {
        id: String,
        spec: String,
    },
    /// Machine-readable session cursor (agent surface): incarnation,
    /// current filtered-stream offset, and liveness in one cheap call.
    SessionCursor {
        id: String,
    },
    /// Bounded window read of the filtered stream: never unbounded,
    /// resumable via the returned `next_offset`.
    ObserveWindow {
        id: String,
        from: u64,
        max_bytes: u32,
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
    /// Report the applied-cursor credit for this attach connection:
    /// the highest stream offset the client has rendered. Handled on the
    /// streaming path only.
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
    /// One-shot rendered view with an observation cursor, not byte delivery.
    LogsRead {
        id: String,
        /// `tail` | `screen` | `frames`.
        mode: String,
        /// Rows for `tail`, snapshots for `frames`; ignored by the other
        /// modes. 0 prints nothing (wait-only reads).
        count: usize,
        /// Must be None for views. Byte continuation uses ObserveWindow.
        #[serde(default)]
        from: Option<StreamPosition>,
        keep_color: bool,
        term_cols: u16,
    },
    // ── Node federation ──────────────────────────────────────────────────────
    /// Proxy an inner request to a named secondary node.
    NodeProxy {
        node: String,
        inner: Box<RpcRequest>,
    },
    NodeAdd {
        config: crate::node::config::NodeConfig,
    },
    NodeUpdate {
        name: String,
        update: crate::node::config::NodeUpdate,
    },
    NodeSetEnabled {
        name: String,
        enabled: bool,
    },
    NodeRemove {
        name: String,
    },
    NodeInfo {
        name: String,
    },
    /// Configured peers plus live state; never an inventory of sockets only.
    NodeList {
        role: Option<crate::node::config::NodeRole>,
        status: Option<crate::node::config::NodeStatus>,
    },
}

impl RpcRequest {
    pub fn name(&self) -> &'static str {
        match self {
            RpcRequest::Health => "health",
            RpcRequest::DaemonStatus => "daemon_status",
            RpcRequest::DaemonStop { .. } => "daemon_stop",
            RpcRequest::List { .. } => "list",
            RpcRequest::Start { .. } => "start",
            RpcRequest::SessionInfo { .. } => "session_info",
            RpcRequest::SessionMetadataSet { .. } => "session_metadata_set",
            RpcRequest::NotifySet { .. } => "notify_set",
            RpcRequest::NotifySend { .. } => "notify_send",
            RpcRequest::AttachSubscribe { .. } => "attach_subscribe",
            RpcRequest::AttachInput { .. } => "attach_input",
            RpcRequest::AttachMouse { .. } => "attach_mouse",
            RpcRequest::SessionCursor { .. } => "session_cursor",
            RpcRequest::ObserveWindow { .. } => "observe_window",
            RpcRequest::AttachBusy { .. } => "attach_busy",
            RpcRequest::UploadFile { .. } => "upload_file",
            RpcRequest::AttachResize { .. } => "attach_resize",
            RpcRequest::AttachDetach { .. } => "attach_detach",
            RpcRequest::AttachAppliedCursor { .. } => "attach_applied_cursor",
            RpcRequest::Stop { .. } => "stop",
            RpcRequest::Restart { .. } => "restart",
            RpcRequest::Kill { .. } => "kill",
            RpcRequest::Remove { .. } => "remove",
            RpcRequest::LogsTail { .. } => "logs_tail",
            RpcRequest::LogsPagination { .. } => "logs_pagination",
            RpcRequest::LogsRead { .. } => "logs_read",
            RpcRequest::NodeProxy { .. } => "node_proxy",
            RpcRequest::NodeAdd { .. } => "node_add",
            RpcRequest::NodeUpdate { .. } => "node_update",
            RpcRequest::NodeSetEnabled { .. } => "node_set_enabled",
            RpcRequest::NodeRemove { .. } => "node_remove",
            RpcRequest::NodeInfo { .. } => "node_info",
            RpcRequest::NodeList { .. } => "node_list",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
// `Attach` carries a full session snapshot for streaming tail-renders;
// boxing it would move a hot-path buffer behind an extra pointer per RPC
// response for no runtime win at this enum size.
#[allow(clippy::large_enum_variant)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RpcResponse {
    /// Machine-readable session cursor answer.
    SessionCursor {
        running: bool,
        exit_code: Option<i32>,
        /// Canonical filtered-stream length (bytes available to read).
        offset: u64,
        incarnation: Option<u64>,
        /// The session is running and its last output looked like a prompt
        /// awaiting input (`--wait prompt`).
        #[serde(default)]
        input_needed: bool,
    },
    /// One bounded filtered-stream window.
    ObserveWindow {
        #[serde(with = "base64_bytes")]
        data: Vec<u8>,
        next_offset: u64,
        running: bool,
        exit_code: Option<i32>,
        incarnation: Option<u64>,
    },
    Empty,
    Health {
        daemon_pid: u32,
    },
    /// Read-only diagnostic summary: liveness vs readiness, supervised
    /// component states, disk capacity, failure-record locations, and
    /// affected sessions. Never carries terminal content, input, or
    /// secrets.
    DaemonStatus {
        status: DaemonStatusReport,
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
    SessionInfo {
        info: SessionInfo,
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
        /// This attachment's fencing token.
        #[serde(default)]
        attachment_id: u64,
        /// This attachment's role, fixed at register time:
        /// "controller" (default) or "observer" (view-only attach).
        #[serde(default)]
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
        /// Inclusive end-of-stream cursor: a client able to apply a
        /// partial frame at offset `final_offset` has applied everything
        /// the daemon signalled — by induction on contiguous
        /// application, no later chunk exists to wait for.
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
    /// Answer to [`RpcRequest::LogsRead`]: the rendered surface plus the
    /// cursor it was taken at, so a caller can chain `--since` reads.
    LogsRead {
        /// The explicit mode served.
        mode: String,
        /// Rendered text for `tail` / `screen` modes (empty for `frames`).
        #[serde(default, with = "base64_bytes")]
        output: Vec<u8>,
        /// Screen snapshots for `frames` mode, oldest first.
        #[serde(default)]
        frames: Vec<LogFrame>,
        /// Terminal width the read was rendered at.
        width: u16,
        /// Stream position at read time: the cursor a follow-up `--since`
        /// starts from (`None` while nothing has been journaled yet).
        cursor: Option<StreamPosition>,
        running: bool,
        exit_code: Option<i32>,
        #[serde(default)]
        status: Option<String>,
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
    NodeInfo {
        node: crate::node::config::NodeSummary,
    },
    NodeList {
        items: Vec<crate::node::config::NodeSummary>,
        total: usize,
    },
}

// ---------------------------------------------------------------------------
// Federation types
// ---------------------------------------------------------------------------

/// A connected secondary node as seen by the primary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRoute {
    pub name: String,
    pub connected: bool,
}

/// Messages exchanged over the `/api/nodes/join` WebSocket connection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum NodeWsMessage {
    /// Plaintext introduction. Join cannot supply another identity or name.
    Hello {
        version: u16,
        name: String,
        public_key: String,
        nonce: [u8; 32],
    },
    /// Plaintext freshness challenge; never supplies a replacement primary pin.
    Challenge {
        nonce: [u8; 32],
    },
    /// Secondary signature over the full, length-delimited transcript.
    Join {
        signature: String,
    },
    /// Sealed primary key possession proof, bound to this transcript.
    KeyConfirm {
        transcript_hash: [u8; 32],
    },
    /// Sealed secondary key possession proof, before registry insertion.
    Ready {
        transcript_hash: [u8; 32],
    },
    /// Sealed final acknowledgement. Only this permits connector success.
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
    /// streaming RPC. Carries attach input, resize, applied-cursor
    /// credits, and detach to the owning node's stream task so remote
    /// attachments get the same fencing and credit-gate enforcement as
    /// local ones.
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
    fn session_info_protocol_round_trips_and_names_request() {
        let request = RpcRequest::SessionInfo { id: "s1".into() };
        assert_eq!(request.name(), "session_info");
        let json = serde_json::to_string(&request).unwrap();
        let decoded: RpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, RpcRequest::SessionInfo { id } if id == "s1"));
    }

    #[test]
    fn mouse_protocol_round_trips_a_compact_spec() {
        let request = RpcRequest::AttachMouse {
            id: "s".into(),
            spec: "left+2@7,8".into(),
        };
        assert_eq!(request.name(), "attach_mouse");
        let json = serde_json::to_string(&request).unwrap();
        let decoded: RpcRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, RpcRequest::AttachMouse { id, spec }
            if id == "s" && spec == "left+2@7,8"));
    }

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

    /// Wire contract for the private JSON protocol: unknown fields are
    /// ignored (forward-compatible additions), while missing required
    /// fields, unknown type tags and malformed base64 fail loudly. The
    /// federation envelope is stricter, but per serde rules: field-bearing
    /// frames reject unknown fields, while unit variants have no fields to
    /// validate and tolerate extra keys. Both behaviors are
    /// frozen here; the handshake `version` is the compatibility gate.
    #[test]
    fn wire_contract_ignores_unknown_fields_and_enforces_required_ones() {
        // Unknown fields: ignored on both request and response.
        let request: RpcRequest = serde_json::from_str(
            r#"{"type":"session_info","id":"s1","future_field":{"nested":true}}"#,
        )
        .expect("unknown request field must be ignored");
        assert!(matches!(request, RpcRequest::SessionInfo { id } if id == "s1"));
        let response: RpcResponse = serde_json::from_str(r#"{"type":"ack","future_field":1}"#)
            .expect("unknown response field must be ignored");
        assert!(matches!(response, RpcResponse::Ack));
        let response: RpcResponse = serde_json::from_str(
            r#"{"type":"attach_stream_chunk","offset":7,"data":"b2s=","future_field":1}"#,
        )
        .expect("unknown fields must also be ignored on field-bearing responses");
        assert!(
            matches!(response, RpcResponse::AttachStreamChunk { offset: 7, data } if data == b"ok")
        );

        // Missing required fields and unknown tags are hard errors.
        for json in [
            r#"{"type":"session_info"}"#,
            r#"{"type":"attach_input","id":"s1"}"#,
            r#"{"type":"attach_input","id":"s1","data":"%%%","wait_for_change":false}"#,
            r#"{"type":"not_a_real_request","id":"s1"}"#,
            r#"["not","an","object"]"#,
        ] {
            assert!(
                serde_json::from_str::<RpcRequest>(json).is_err(),
                "must be rejected: {json}"
            );
        }

        for json in [
            r#"{"type":"error"}"#,
            r#"{"type":"attach_stream_chunk","offset":0}"#,
            r#"{"type":"attach_stream_chunk","offset":0,"data":"%%%"}"#,
            r#"{"type":"not_a_real_response"}"#,
            r#"["not","an","object"]"#,
        ] {
            assert!(
                serde_json::from_str::<RpcResponse>(json).is_err(),
                "must be rejected: {json}"
            );
        }

        // Federation frames: unknown fields are rejected on field-bearing
        // variants (the handshake binds versions)...
        assert!(
            serde_json::from_str::<NodeWsMessage>(r#"{"type":"error","message":"x","future":1}"#)
                .is_err(),
            "unknown federation fields must be rejected on field-bearing frames"
        );
        // ...but unit variants have no fields to validate. Freeze all
        // three exceptions rather than implying that only Joined allows it.
        for message in [
            NodeWsMessage::Joined,
            NodeWsMessage::Ping,
            NodeWsMessage::Pong,
        ] {
            let mut json = serde_json::to_value(&message).unwrap();
            json["future_field"] = serde_json::json!(1);
            let decoded: NodeWsMessage = serde_json::from_value(json)
                .expect("unit-variant frames tolerate extra keys today");
            assert_eq!(
                serde_json::to_value(decoded).unwrap(),
                serde_json::to_value(message).unwrap()
            );
        }
    }

    #[test]
    fn node_ws_payload_round_trips_uncompressed_binary_json() {
        let message = NodeWsMessage::Join {
            signature: "sig".into(),
        };

        let payload = encode_node_ws_payload(&message).expect("encode payload");
        let decoded = decode_node_ws_payload(&payload).expect("decode payload");

        assert!(matches!(decoded, NodeWsMessage::Join { signature } if signature == "sig"));
    }

    #[test]
    fn node_ws_identity_handshake_round_trip() {
        let hello = NodeWsMessage::Hello {
            version: super::FEDERATION_VERSION,
            name: "worker-b".into(),
            nonce: [1; 32],
            public_key: "ed25519 AAAA".into(),
        };
        let payload = encode_node_ws_payload(&hello).expect("encode payload");
        match decode_node_ws_payload(&payload).expect("decode payload") {
            NodeWsMessage::Hello {
                version,
                name,
                nonce,
                public_key,
            } => {
                assert_eq!(version, super::FEDERATION_VERSION);
                assert_eq!(name, "worker-b");
                assert_eq!(nonce, [1; 32]);
                assert_eq!(public_key, "ed25519 AAAA");
            }
            other => panic!("unexpected decoded message: {other:?}"),
        }

        let join = NodeWsMessage::Join {
            signature: "sig".into(),
        };
        let payload = encode_node_ws_payload(&join).expect("encode payload");
        match decode_node_ws_payload(&payload).expect("decode payload") {
            NodeWsMessage::Join { signature } => assert_eq!(signature, "sig"),
            other => panic!("unexpected decoded message: {other:?}"),
        }
    }

    #[test]
    fn obsolete_auth_union_and_unbound_identity_fields_are_rejected() {
        for json in [
            r#"{"type":"join","name":"worker","auth":{"method":"ssh_key","signature":"sig","public_key":"key"}}"#,
            r#"{"type":"join","signature":"sig","public_key":"another identity"}"#,
            r#"{"type":"join","signature":"sig","name":"another name"}"#,
            r#"{"type":"hello","public_key":"key"}"#,
        ] {
            assert!(serde_json::from_str::<NodeWsMessage>(json).is_err());
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

/// Full session inspection, including fields absent from list summaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    #[serde(flatten)]
    pub summary: SessionSummary,
    pub exit_code: Option<i32>,
    /// Path on the owning daemon, not necessarily on the CLI's machine.
    pub session_dir: String,
    pub journal_incarnation: Option<u64>,
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
    /// Suggested command extracted from the completed session's journal tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Running process needs attention or is ready for another instruction.
    /// Explicit OSC idle/done/blocked/error reports are immediate; working is not.
    /// This is independent of notification delivery, not approval authority.
    #[serde(default)]
    pub input_needed: bool,
    /// Most relevant OSC 7501 report. Untrusted app state, not process status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_status: Option<crate::session::program_status::ProgramStatus>,
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
    /// Bytes currently retained on disk for this session's journal
    /// (sealed parts only). `None` until the first checkpoint-gated
    /// sweep has run. With live deletion disabled in production this
    /// tracks journal growth, not a cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_bytes_retained: Option<u64>,
    /// Cumulative count of journal retention sweeps run for this session.
    /// Increments on every checkpoint (≈ 32 MiB of filtered output). 0
    /// means no sweeps have occurred yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_retention_sweeps: Option<u64>,
    /// Cumulative count of sealed incarnations dropped from this
    /// session's journal across all retention sweeps. May exceed
    /// `journal_retention_sweeps` when a single sweep drops several
    /// incarnations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_incarnations_dropped: Option<u64>,
    /// Legacy preview field retained for wire deserialization compatibility.
    /// Live journal byte caps are unsupported; production emits None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_byte_cap: Option<u64>,
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

/// One supervised daemon component, as reported by [`DaemonStatusReport`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentStatus {
    pub name: String,
    /// `starting` | `running` | `restarting` | `failed` | `disabled`.
    pub state: String,
    /// Whether a non-running state degrades daemon readiness.
    pub ready_critical: bool,
    /// Failed/stopped episodes observed since the daemon started.
    pub failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<DateTime<Utc>>,
}

/// Disk capacity and admission-guard view, from the daemon's cached sample.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskStatus {
    /// Free bytes on the state volume; `None` = unknown/stale (see reason), never 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_unknown_reason: Option<String>,
    /// When the cached sample was taken; `None` = never sampled yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampled_at: Option<DateTime<Utc>>,
    /// `None` until a fresh sample exists; stale sizes are not exported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploads_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sqlite_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs_bytes: Option<u64>,
    /// Set when the size scan was capped or partially failed; sizes are a
    /// lower bound then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakdown_partial_reason: Option<String>,
    /// `disk_guard_enabled` as currently configured (opt-in).
    pub guard_enabled: bool,
    /// `disabled` | `allowed` | `unknown` | `stale` | `below_minimum`.
    pub guard_decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_refusal_reason: Option<String>,
    /// Always-on warning threshold; `warning_active` is set when the
    /// latest sample is below it (independent of the guard).
    pub warn_below_bytes: u64,
    #[serde(default)]
    pub warning_active: bool,
}

/// A session the diagnostic summary wants to point at, with why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AffectedSession {
    pub id: String,
    /// Session status or, for uncommitted starts, `starting`.
    pub status: String,
    pub reason: String,
}

/// Read-only diagnostic summary. Assembled from the live component
/// registry, the cached disk sample, the daemon's own info, and bounded
/// session queries. Deliberately excludes terminal content, input,
/// environment, identity seeds, and credentials.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonStatusReport {
    pub daemon_pid: u32,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Liveness: this report itself proves the daemon answered.
    pub alive: bool,
    /// Readiness: ready-critical tasks are running and an enabled disk
    /// guard permits new starts. This is not proof of individual tick progress.
    pub ready: bool,
    #[serde(default)]
    pub unready_reasons: Vec<String>,
    #[serde(default)]
    pub components: Vec<ComponentStatus>,
    pub disk: DiskStatus,
    /// Paths where failures are recorded on the owning machine (log dir,
    /// crash record, raw stderr capture).
    #[serde(default)]
    pub failure_records: Vec<String>,
    /// In-flight start reservations (id order), then failed sessions
    /// newest first, capped. Reservations do not survive a daemon crash.
    #[serde(default)]
    pub affected_sessions: Vec<AffectedSession>,
    /// Total affected sessions before the cap; unknown if the query failed.
    #[serde(default)]
    pub affected_sessions_total: Option<usize>,
    /// Degraded diagnostic inputs, distinct from task/admission readiness.
    #[serde(default)]
    pub diagnostic_errors: Vec<String>,
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
pub struct PushSubscriptionRecord {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
}
