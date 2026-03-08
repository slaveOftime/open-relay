use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    error::{AppError, Result},
    protocol::RpcResponse,
};

/// A live connection to a secondary node.
pub struct NodeHandle {
    /// Send `(rpc_id, serialised_request_value)` here to forward an RPC over WS.
    pub send_tx: mpsc::Sender<(String, Value)>,
    /// Pending one-shot response channels, keyed by `rpc_id`.
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<Result<RpcResponse>>>>>,
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

    /// Forward `request` to the named secondary and await its response.
    ///
    /// Returns `Err(NodeNotConnected)` if the node is not currently online.
    pub async fn proxy_rpc(
        &self,
        node: &str,
        request: &crate::protocol::RpcRequest,
    ) -> Result<RpcResponse> {
        // Clones of channels only — the lock is held only briefly.
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
            pending_map.insert(id.clone(), tx);
        }

        let request_json = serde_json::to_value(request)?;
        send_tx
            .send((id.clone(), request_json))
            .await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?;

        rx.await
            .map_err(|_| AppError::NodeNotConnected(node.to_string()))?
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
