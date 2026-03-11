use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    error::{AppError, Result},
    protocol::RpcResponse,
};

/// A pending RPC response sender — either single-shot or streaming.
pub enum PendingRpc {
    /// Single request/response — resolved once.
    OneShot(oneshot::Sender<Result<RpcResponse>>),
    /// Streaming — multiple frames delivered until stream ends.
    Stream(mpsc::UnboundedSender<Result<RpcResponse>>),
}

/// A live connection to a secondary node.
pub struct NodeHandle {
    /// Send `(rpc_id, serialised_request_value)` here to forward an RPC over WS.
    pub send_tx: mpsc::Sender<(String, Value)>,
    /// Pending response channels, keyed by `rpc_id`.
    pub pending: Arc<Mutex<HashMap<String, PendingRpc>>>,
}

/// Tracks all connected secondary nodes on the primary.
pub struct NodeRegistry {
    nodes: Mutex<HashMap<String, NodeHandle>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            nodes: Mutex::new(HashMap::new()),
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
            .send((id.clone(), request_json))
            .await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?;

        rx.await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?
    }

    /// Forward `request` to the named secondary and return a stream receiver
    /// for multi-frame responses.
    pub async fn proxy_rpc_stream(
        &self,
        node: &str,
        request: &crate::protocol::RpcRequest,
    ) -> Result<mpsc::UnboundedReceiver<Result<RpcResponse>>> {
        let (send_tx, pending) = {
            let nodes = self.nodes.lock().await;
            let handle = nodes
                .get(node)
                .ok_or_else(|| AppError::NodeNotConnected(node.to_string()))?;
            (handle.send_tx.clone(), Arc::clone(&handle.pending))
        };

        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::unbounded_channel();

        {
            let mut pending_map = pending.lock().await;
            pending_map.insert(id.clone(), PendingRpc::Stream(tx));
        }

        let request_json = serde_json::to_value(request)?;
        send_tx
            .send((id.clone(), request_json))
            .await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?;

        Ok(rx)
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
}
