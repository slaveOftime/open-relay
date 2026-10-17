use std::{collections::HashMap, sync::Arc};

use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc, oneshot, watch};

use crate::{
    error::{AppError, Result},
    protocol::{NodeWsMessage, RpcRequest, RpcResponse},
    session::SessionEvent,
};

/// Default deadline for one-shot proxied RPCs: a hung secondary
/// must never stall a gateway caller forever. `LogsRead` overrides this
/// because a bounded journal replay on the owning node is disk- and
/// CPU-bound work whose cost grows with the recording, not with the RPC.
pub const NODE_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for a proxied `logs` read: the worst legitimate case is a
/// cold screen-history replay of a multi-gigabyte recording, which no
/// interactive caller should wait for, but a 30 s default would fail
/// long-running sessions that are merely slow to seek.
pub const NODE_RPC_LOGS_READ_TIMEOUT: Duration = Duration::from_secs(300);

fn rpc_deadline_for(request: &RpcRequest) -> Duration {
    match request {
        RpcRequest::LogsRead { .. } => NODE_RPC_LOGS_READ_TIMEOUT,
        _ => NODE_RPC_TIMEOUT,
    }
}

/// A pending RPC response sender — either single-shot or streaming.
pub enum PendingRpc {
    /// Single request/response — resolved once.
    OneShot(oneshot::Sender<Result<RpcResponse>>),
    /// Streaming — multiple frames delivered until stream ends (bounded).
    Stream(mpsc::Sender<Result<RpcResponse>>),
}

/// A live connection to a secondary node.
pub struct NodeHandle {
    /// Send relay-bound messages here (RPC envelopes and mid-stream
    /// stream messages) for delivery over the node's WS connection.
    pub send_tx: mpsc::Sender<NodeWsMessage>,
    /// Pending response channels, keyed by `rpc_id`.
    pub pending: Arc<Mutex<HashMap<String, PendingRpc>>>,
    /// Cancels socket IO even while the relay is blocked in another await.
    pub stop_tx: tokio::sync::watch::Sender<bool>,
}

/// Stream and cancellation belong to the same connection generation.
pub struct ProxyStream {
    pub id: String,
    pub responses: mpsc::Receiver<Result<RpcResponse>>,
    pub cancelled: watch::Receiver<bool>,
}

struct RegisteredNode {
    generation: uuid::Uuid,
    handle: NodeHandle,
}

impl std::ops::Deref for RegisteredNode {
    type Target = NodeHandle;
    fn deref(&self) -> &NodeHandle {
        &self.handle
    }
}

/// Tracks all connected secondary nodes on the primary.
pub struct NodeRegistry {
    /// Serializes persisted mutations with final inbound authorization/insertion.
    pub(crate) config_lock: Mutex<()>,
    nodes: Mutex<HashMap<String, RegisteredNode>>,
    join_failures: Mutex<HashMap<std::net::IpAddr, (std::time::Instant, u32)>>,
    /// Deadline applied to one-shot proxied RPCs (test-overridable).
    rpc_timeout: Duration,
    /// Session-event broadcaster used to publish `node_state` when the
    /// pool changes. Stored behind a `OnceLock` so the bind is idempotent
    /// and the `bind_event_tx` call can land after the registry has already
    /// been wrapped in `Arc::<NodeRegistry>`.
    event_tx: std::sync::OnceLock<broadcast::Sender<SessionEvent>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            config_lock: Mutex::new(()),
            nodes: Mutex::new(HashMap::new()),
            join_failures: Mutex::new(HashMap::new()),
            rpc_timeout: NODE_RPC_TIMEOUT,
            event_tx: std::sync::OnceLock::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_rpc_timeout(rpc_timeout: Duration) -> Self {
        Self {
            config_lock: Mutex::new(()),
            nodes: Mutex::new(HashMap::new()),
            join_failures: Mutex::new(HashMap::new()),
            rpc_timeout,
            event_tx: std::sync::OnceLock::new(),
        }
    }

    /// Wire `event_tx` so future `connect`/`disconnect` calls publish a
    /// [`SessionEvent::NodeState`] on the broadcast. The relay path then
    /// fans it out to subscribers without double-counting the source.
    /// Idempotent: only the first call sticks.
    pub fn bind_event_tx(&self, event_tx: broadcast::Sender<SessionEvent>) {
        let _ = self.event_tx.set(event_tx);
    }

    fn event_tx(&self) -> Option<&broadcast::Sender<SessionEvent>> {
        self.event_tx.get()
    }

    /// Atomic check-and-insert. Returns the connection's removal authority.
    pub async fn connect(&self, name: String, handle: NodeHandle) -> Result<uuid::Uuid> {
        let mut nodes = self.nodes.lock().await;
        if nodes.contains_key(&name) {
            return Err(AppError::Protocol(format!(
                "name '{name}' is already connected"
            )));
        }
        let generation = uuid::Uuid::new_v4();
        nodes.insert(name.clone(), RegisteredNode { generation, handle });
        if let Some(tx) = self.event_tx() {
            let _ = tx.send(SessionEvent::NodeState {
                node: name,
                connected: true,
                last_seen: Some(chrono::Utc::now().timestamp()),
            });
        }
        Ok(generation)
    }

    /// A stale task must not remove a newer connection under the same name.
    pub async fn disconnect(&self, name: &str, generation: uuid::Uuid) {
        let mut nodes = self.nodes.lock().await;
        if nodes
            .get(name)
            .is_none_or(|node| node.generation != generation)
        {
            return;
        }
        nodes.remove(name);
        if let Some(tx) = self.event_tx() {
            let _ = tx.send(SessionEvent::NodeState {
                node: name.to_string(),
                connected: false,
                last_seen: None,
            });
        }
    }

    /// Revoke the live generation immediately; stale teardown cannot remove a
    /// later generation. In-flight RPCs are failed by the cancelled relay.
    pub(crate) async fn revoke(&self, name: &str) {
        let mut nodes = self.nodes.lock().await;
        if let Some(node) = nodes.remove(name) {
            let _ = node.stop_tx.send(true);
            if let Some(tx) = self.event_tx() {
                let _ = tx.send(SessionEvent::NodeState {
                    node: name.into(),
                    connected: false,
                    last_seen: None,
                });
            }
        }
    }

    /// Bounded failure window independent of optional HTTP password auth.
    pub async fn join_rate_limited(&self, ip: std::net::IpAddr) -> bool {
        let mut failures = self.join_failures.lock().await;
        failures.retain(|_, (start, _)| start.elapsed() < Duration::from_secs(60));
        failures.get(&ip).is_some_and(|(_, count)| *count >= 8)
            || (failures.len() >= 1024 && !failures.contains_key(&ip))
    }

    pub async fn record_join_failure(&self, ip: std::net::IpAddr) {
        let mut failures = self.join_failures.lock().await;
        failures.retain(|_, (start, _)| start.elapsed() < Duration::from_secs(60));
        if failures.len() < 1024 || failures.contains_key(&ip) {
            let (_, count) = failures.entry(ip).or_insert((std::time::Instant::now(), 0));
            *count = count.saturating_add(1);
        }
    }

    /// Forward `request` to the named secondary and await a single response.
    pub async fn proxy_rpc(
        &self,
        node: &str,
        request: &crate::protocol::RpcRequest,
    ) -> Result<RpcResponse> {
        let (send_tx, pending) = {
            let nodes = self.nodes.lock().await;
            let handle = nodes
                .get(node)
                .ok_or_else(|| AppError::NodeNotConnected(node.to_string()))?;
            (handle.send_tx.clone(), Arc::clone(&handle.pending))
        };

        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();

        {
            let mut pending_map = pending.lock().await;
            pending_map.insert(id.clone(), PendingRpc::OneShot(tx));
        }

        let request_json = serde_json::to_value(request)?;
        send_tx
            .send(NodeWsMessage::Rpc {
                id: id.clone(),
                request: request_json,
            })
            .await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?;

        match tokio::time::timeout(self.rpc_deadline_for(request), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AppError::NodeNotConnected(node.to_string())),
            Err(_) => {
                // Deadline hit: drop the pending entry so a late response
                // has nowhere to land, then fail loudly.
                let mut pm = pending.lock().await;
                pm.remove(&id);
                Err(AppError::Protocol(format!(
                    "node '{node}' did not answer '{}' within {:?}",
                    request.name(),
                    self.rpc_deadline_for(request)
                )))
            }
        }
    }

    fn rpc_deadline_for(&self, request: &RpcRequest) -> Duration {
        match request {
            RpcRequest::LogsRead { .. } => rpc_deadline_for(request),
            _ => self.rpc_timeout,
        }
    }

    /// Forward `request` to the named secondary and return a stream receiver
    /// for multi-frame responses.  Also returns the `rpc_id` so callers can
    /// explicitly clean up the pending entry on early exit.
    pub async fn proxy_rpc_stream(
        &self,
        node: &str,
        request: &crate::protocol::RpcRequest,
    ) -> Result<ProxyStream> {
        let (send_tx, pending, cancelled) = {
            let nodes = self.nodes.lock().await;
            let handle = nodes
                .get(node)
                .ok_or_else(|| AppError::NodeNotConnected(node.to_string()))?;
            (
                handle.send_tx.clone(),
                Arc::clone(&handle.pending),
                handle.stop_tx.subscribe(),
            )
        };

        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::channel(256);

        {
            let mut pending_map = pending.lock().await;
            pending_map.insert(id.clone(), PendingRpc::Stream(tx));
        }

        let request_json = serde_json::to_value(request)?;
        send_tx
            .send(NodeWsMessage::Rpc {
                id: id.clone(),
                request: request_json,
            })
            .await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?;

        Ok(ProxyStream {
            id,
            responses: rx,
            cancelled,
        })
    }

    /// Send one mid-stream client message to an open streaming RPC on
    /// the named secondary. Attach input, resize, applied-cursor credits
    /// and detach travel this way so remote clients get the same
    /// fencing and credit gate as local ones.
    pub async fn proxy_rpc_stream_message(
        &self,
        node: &str,
        rpc_id: &str,
        request: &crate::protocol::RpcRequest,
    ) -> Result<()> {
        let send_tx = {
            let nodes = self.nodes.lock().await;
            nodes
                .get(node)
                .ok_or_else(|| AppError::NodeNotConnected(node.to_string()))?
                .send_tx
                .clone()
        };
        let request_json = serde_json::to_value(request)?;
        send_tx
            .send(NodeWsMessage::RpcStreamMessage {
                id: rpc_id.to_string(),
                request: request_json,
            })
            .await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))
    }

    /// Returns `true` if `name` is currently connected.
    #[cfg(test)]
    pub async fn is_connected(&self, name: &str) -> bool {
        let nodes = self.nodes.lock().await;
        nodes.contains_key(name)
    }

    /// Returns the names of all currently-connected nodes.
    pub async fn connected_names(&self) -> Vec<String> {
        let nodes = self.nodes.lock().await;
        nodes.keys().cloned().collect()
    }

    /// Remove a pending RPC entry for the named node.  Called when the caller
    /// drops its stream receiver early (e.g. client disconnect) so the entry
    /// does not linger in the pending map.
    pub async fn remove_pending(&self, node: &str, rpc_id: &str) {
        let nodes = self.nodes.lock().await;
        if let Some(handle) = nodes.get(node) {
            let mut pm = handle.pending.lock().await;
            pm.remove(rpc_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{NodeWsMessage, RpcRequest};

    fn fake_handle() -> (NodeHandle, mpsc::Receiver<NodeWsMessage>) {
        let (send_tx, send_rx) = mpsc::channel(8);
        (
            NodeHandle {
                send_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                stop_tx: tokio::sync::watch::channel(false).0,
            },
            send_rx,
        )
    }

    /// A hung secondary must fail the caller loudly at the deadline,
    /// not stall it forever; the pending entry is removed so a late
    /// response has nowhere to land.
    #[tokio::test]
    async fn proxied_rpc_times_out_loudly_when_the_node_hangs() {
        let registry = NodeRegistry::with_rpc_timeout(Duration::from_millis(50));
        let (handle, _send_rx) = fake_handle(); // receiver held, never answered
        let pending = Arc::clone(&handle.pending);
        registry.connect("worker".into(), handle).await.unwrap();

        let err = registry
            .proxy_rpc("worker", &RpcRequest::Health)
            .await
            .expect_err("hung node must fail the caller");
        let msg = err.to_string();
        assert!(msg.contains("did not answer"), "unexpected error: {msg}");
        assert!(
            pending.lock().await.is_empty(),
            "timed-out pending entry must be removed"
        );
    }

    #[tokio::test]
    async fn proxy_stream_cancellation_is_bound_to_its_generation() {
        let registry = NodeRegistry::new();
        let (handle, mut messages) = fake_handle();
        let first = registry.connect("worker".into(), handle).await.unwrap();
        let mut stream = registry
            .proxy_rpc_stream("worker", &RpcRequest::Health)
            .await
            .unwrap();
        messages.recv().await.unwrap();
        assert!(!*stream.cancelled.borrow());
        registry.revoke("worker").await;
        stream.cancelled.changed().await.unwrap();
        assert!(*stream.cancelled.borrow());
        let (replacement, _messages) = fake_handle();
        let replacement_signal = replacement.stop_tx.subscribe();
        registry
            .connect("worker".into(), replacement)
            .await
            .unwrap();
        registry.disconnect("worker", first).await;
        assert!(registry.is_connected("worker").await);
        assert!(
            !*replacement_signal.borrow(),
            "old teardown cancelled its replacement"
        );
    }

    /// Disconnecting a node fails every waiter (one-shot and stream)
    /// loudly — no caller hangs on a dead generation.
    #[tokio::test]
    async fn disconnect_fences_pending_waiters_and_streams() {
        let registry = NodeRegistry::new();
        let (handle, _send_rx) = fake_handle();
        let pending = Arc::clone(&handle.pending);
        let generation = registry.connect("worker".into(), handle).await.unwrap();

        let (one_shot_tx, one_shot_rx) = oneshot::channel();
        pending
            .lock()
            .await
            .insert("rpc-1".into(), PendingRpc::OneShot(one_shot_tx));
        let (stream_tx, mut stream_rx) = mpsc::channel(4);
        pending
            .lock()
            .await
            .insert("rpc-2".into(), PendingRpc::Stream(stream_tx));

        registry.disconnect("worker", generation).await;
        // Mirror the relay-loop drain (http/nodes.rs): waiters are failed
        // loudly on disconnect.
        {
            let mut pm = pending.lock().await;
            for (_, sender) in pm.drain() {
                match sender {
                    PendingRpc::OneShot(tx) => {
                        let _ = tx.send(Err(AppError::NodeNotConnected("worker".into())));
                    }
                    PendingRpc::Stream(tx) => {
                        let _ = tx
                            .send(Err(AppError::NodeNotConnected("worker".into())))
                            .await;
                    }
                }
            }
        }

        let err = one_shot_rx
            .await
            .expect("one-shot waiter resolved")
            .expect_err("one-shot waiter must fail");
        assert!(matches!(err, AppError::NodeNotConnected(_)));
        let stream_msg = stream_rx.recv().await.expect("stream waiter resolved");
        assert!(matches!(stream_msg, Err(AppError::NodeNotConnected(_))));
        assert!(
            stream_rx.recv().await.is_none(),
            "stream must close after the fencing error"
        );
    }

    /// Reconnecting under the same name replaces the handle — new
    /// RPCs route to the new generation, never to the stale one.
    #[tokio::test]
    async fn reconnect_routes_to_the_new_generation() {
        let registry = NodeRegistry::new();
        let (old_handle, mut old_rx) = fake_handle();
        let generation = registry.connect("worker".into(), old_handle).await.unwrap();
        registry.disconnect("worker", generation).await;
        assert!(
            old_rx.try_recv().is_err(),
            "stale generation must not receive traffic"
        );

        let (new_handle, mut new_rx) = fake_handle();
        registry.connect("worker".into(), new_handle).await.unwrap();
        // Fire-and-forget: the message must land on the new generation's
        // channel even though nothing answers it.
        let registry_ref = &registry;
        let send = registry_ref.proxy_rpc("worker", &RpcRequest::Health);
        tokio::pin!(send);
        tokio::select! {
            msg = new_rx.recv() => {
                assert!(matches!(msg, Some(NodeWsMessage::Rpc { .. })));
            }
            _ = &mut send => panic!("RPC resolved before any response"),
        }
        assert!(old_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn duplicate_insert_is_atomic_and_stale_cleanup_is_inert() {
        let registry = NodeRegistry::new();
        let (events, mut rx) = broadcast::channel(16);
        registry.bind_event_tx(events);
        let (a, _) = fake_handle();
        let (b, _) = fake_handle();
        let (one, two) = tokio::join!(
            registry.connect("worker".into(), a),
            registry.connect("worker".into(), b)
        );
        assert_ne!(one.is_ok(), two.is_ok());
        let generation = one.or(two).unwrap();
        assert!(matches!(
            rx.recv().await.unwrap(),
            SessionEvent::NodeState {
                connected: true,
                ..
            }
        ));
        assert!(
            rx.try_recv().is_err(),
            "duplicate cannot publish another success"
        );
        registry.disconnect("worker", generation).await;
        let (next, _) = fake_handle();
        let next_generation = registry.connect("worker".into(), next).await.unwrap();
        registry.disconnect("worker", generation).await;
        assert!(
            registry.is_connected("worker").await,
            "stale task removed new owner"
        );
        registry.disconnect("worker", next_generation).await;
        assert!(!registry.is_connected("worker").await);
    }

    #[tokio::test]
    async fn federation_failure_limit_does_not_need_browser_auth() {
        let registry = NodeRegistry::new();
        let ip = "127.0.0.1".parse().unwrap();
        for _ in 0..8 {
            assert!(!registry.join_rate_limited(ip).await);
            registry.record_join_failure(ip).await;
        }
        assert!(registry.join_rate_limited(ip).await);
        assert!(
            !registry
                .join_rate_limited("127.0.0.2".parse().unwrap())
                .await
        );
    }

    /// Journal-backed reads get the extended deadline; everything else
    /// keeps the interactive default.
    #[test]
    fn logs_read_gets_the_replay_deadline() {
        let read = RpcRequest::LogsRead {
            id: "s".into(),
            mode: "frames".into(),
            count: 10,
            from: None,
            keep_color: false,
            term_cols: 80,
        };
        assert_eq!(rpc_deadline_for(&read), NODE_RPC_LOGS_READ_TIMEOUT);
        assert_eq!(rpc_deadline_for(&RpcRequest::Health), NODE_RPC_TIMEOUT);
    }
}
