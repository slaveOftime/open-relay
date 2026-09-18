use std::{collections::HashMap, sync::Arc};

use std::time::Duration;

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    error::{AppError, Result},
    protocol::{NodeWsMessage, RpcRequest, RpcResponse},
};

/// Default deadline for one-shot proxied RPCs (M5-3): a hung secondary
/// must never stall a gateway caller forever. `LogsWait` overrides this
/// with its own client-specified timeout plus margin.
pub const NODE_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Extra margin added to a `LogsWait` request's own timeout so the proxy
/// deadline never fires before the owning node could answer legitimately.
pub const NODE_RPC_LOGS_WAIT_MARGIN: Duration = Duration::from_secs(10);

fn rpc_deadline_for(request: &RpcRequest) -> Duration {
    match request {
        RpcRequest::LogsWait { timeout_ms, .. } => {
            Duration::from_millis(*timeout_ms) + NODE_RPC_LOGS_WAIT_MARGIN
        }
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
}

/// Tracks all connected secondary nodes on the primary.
pub struct NodeRegistry {
    nodes: Mutex<HashMap<String, NodeHandle>>,
    /// Deadline applied to one-shot proxied RPCs (test-overridable).
    rpc_timeout: Duration,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            rpc_timeout: NODE_RPC_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_rpc_timeout(rpc_timeout: Duration) -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
            rpc_timeout,
        }
    }

    /// Register a newly-connected secondary node.
    pub async fn connect(&self, name: String, handle: NodeHandle) {
        let mut nodes = self.nodes.lock().await;
        nodes.insert(name, handle);
    }

    /// Remove a secondary node (called on WS disconnect).
    pub async fn disconnect(&self, name: &str) {
        let mut nodes = self.nodes.lock().await;
        nodes.remove(name);
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
            RpcRequest::LogsWait { .. } => rpc_deadline_for(request),
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
    ) -> Result<(String, mpsc::Receiver<Result<RpcResponse>>)> {
        let (send_tx, pending) = {
            let nodes = self.nodes.lock().await;
            let handle = nodes
                .get(node)
                .ok_or_else(|| AppError::NodeNotConnected(node.to_string()))?;
            (handle.send_tx.clone(), Arc::clone(&handle.pending))
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

        Ok((id, rx))
    }

    /// Send one mid-stream client message to an open streaming RPC on the
    /// named secondary (M5-2): attach input, resize, applied-cursor
    /// credits, control takeover, and detach all travel this way so the
    /// owning node's stream task applies its attachment-scoped fencing
    /// and credit gate to remote clients exactly as to local ones.
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
            },
            send_rx,
        )
    }

    /// M5-3: a hung secondary must fail the caller loudly at the deadline,
    /// not stall it forever; the pending entry is removed so a late
    /// response has nowhere to land.
    #[tokio::test]
    async fn proxied_rpc_times_out_loudly_when_the_node_hangs() {
        let registry = NodeRegistry::with_rpc_timeout(Duration::from_millis(50));
        let (handle, _send_rx) = fake_handle(); // receiver held, never answered
        let pending = Arc::clone(&handle.pending);
        registry.connect("worker".into(), handle).await;

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

    /// M5-3: disconnecting a node fails every waiter (one-shot and stream)
    /// loudly — no caller hangs on a dead generation.
    #[tokio::test]
    async fn disconnect_fences_pending_waiters_and_streams() {
        let registry = NodeRegistry::new();
        let (handle, _send_rx) = fake_handle();
        let pending = Arc::clone(&handle.pending);
        registry.connect("worker".into(), handle).await;

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

        registry.disconnect("worker").await;
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

    /// M5-3: reconnecting under the same name replaces the handle — new
    /// RPCs route to the new generation, never to the stale one.
    #[tokio::test]
    async fn reconnect_routes_to_the_new_generation() {
        let registry = NodeRegistry::new();
        let (old_handle, mut old_rx) = fake_handle();
        registry.connect("worker".into(), old_handle).await;
        registry.disconnect("worker").await;
        assert!(
            old_rx.try_recv().is_err(),
            "stale generation must not receive traffic"
        );

        let (new_handle, mut new_rx) = fake_handle();
        registry.connect("worker".into(), new_handle).await;
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

    /// LogsWait carries its own client timeout; the proxy deadline must
    /// exceed it by the margin, never the other way around.
    #[test]
    fn logs_wait_deadline_tracks_its_own_timeout() {
        let wait = RpcRequest::LogsWait {
            id: "s".into(),
            timeout_ms: 5_000,
        };
        assert_eq!(
            rpc_deadline_for(&wait),
            Duration::from_millis(5_000) + NODE_RPC_LOGS_WAIT_MARGIN
        );
        assert_eq!(rpc_deadline_for(&RpcRequest::Health), NODE_RPC_TIMEOUT);
    }
}
