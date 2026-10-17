//! One local authority for peer configuration, connection lifecycle, and queries.
use std::sync::Arc;

use super::{NodeConnectors, SessionEventTx, rpc_nodes::spawn_node_connector};
use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    node::{
        NodeRegistry,
        config::{NodeConfig, NodeRole, NodeStatus, NodeSummary, PrimaryConnection},
    },
    protocol::{RpcRequest, RpcResponse},
};

pub(super) struct NodeManager<'a> {
    pub config: &'a Arc<AppConfig>,
    pub db: &'a Arc<Database>,
    pub registry: &'a Arc<NodeRegistry>,
    pub connectors: &'a NodeConnectors,
    pub events: &'a SessionEventTx,
}

impl NodeManager<'_> {
    /// The lock also fences HTTP handshake registration. No CLI-side writes.
    pub async fn handle(&self, request: RpcRequest) -> Result<RpcResponse> {
        let _guard = self.registry.config_lock.lock().await;
        match request {
            RpcRequest::NodeList { role, status } => {
                let mut items = Vec::new();
                for config in self.db.list_nodes().await? {
                    if role.is_some_and(|role| role != config.role) {
                        continue;
                    }
                    let node = self.summary(config).await;
                    if status.is_none_or(|status| status == node.status) {
                        items.push(node);
                    }
                }
                let total = items.len();
                Ok(RpcResponse::NodeList { items, total })
            }
            RpcRequest::NodeInfo { name } => self.info(&name).await,
            RpcRequest::NodeAdd { mut config } => {
                config.enabled = true;
                config.validate()?;
                if self.db.get_node(&config.name).await?.is_some() {
                    return Err(AppError::Protocol(format!(
                        "node '{}' already exists; use node update",
                        config.name
                    )));
                }
                config.revision = uuid::Uuid::new_v4().to_string();
                self.db.insert_node(&config).await?;
                self.reconcile(&config).await?;
                Ok(RpcResponse::NodeInfo {
                    node: self.summary(config).await,
                })
            }
            RpcRequest::NodeUpdate { name, update } => {
                let mut config = self.require(&name).await?;
                let previous = config.clone();
                update.apply(&mut config)?;
                if config != previous {
                    config.revision = uuid::Uuid::new_v4().to_string();
                    self.db.update_node(&config).await?;
                    self.reconcile(&config).await?;
                }
                Ok(RpcResponse::NodeInfo {
                    node: self.summary(config).await,
                })
            }
            RpcRequest::NodeSetEnabled { name, enabled } => {
                let mut config = self.require(&name).await?;
                if config.enabled != enabled {
                    config.enabled = enabled;
                    config.revision = uuid::Uuid::new_v4().to_string();
                    self.db.update_node(&config).await?;
                    self.reconcile(&config).await?;
                }
                Ok(RpcResponse::NodeInfo {
                    node: self.summary(config).await,
                })
            }
            RpcRequest::NodeRemove { name } => {
                self.require(&name).await?;
                self.db.remove_node(&name).await?;
                self.stop(&name).await;
                Ok(RpcResponse::Ack)
            }
            _ => Err(AppError::Protocol("not a node management request".into())),
        }
    }

    async fn require(&self, name: &str) -> Result<NodeConfig> {
        self.db
            .get_node(name)
            .await?
            .ok_or_else(|| AppError::Protocol(format!("node not found: {name}")))
    }

    async fn info(&self, name: &str) -> Result<RpcResponse> {
        Ok(RpcResponse::NodeInfo {
            node: self.summary(self.require(name).await?).await,
        })
    }

    async fn summary(&self, config: NodeConfig) -> NodeSummary {
        let mut summary = NodeSummary {
            status: if config.enabled {
                NodeStatus::Waiting
            } else {
                NodeStatus::Disabled
            },
            config,
            last_error: None,
            next_retry_at: None,
        };
        if summary.config.enabled {
            match summary.config.role {
                NodeRole::Primary => {
                    if let Some(connector) = self.connectors.lock().await.get(&summary.config.name)
                    {
                        let state = connector.state();
                        summary.status = state.status;
                        summary.last_error = state.last_error;
                        summary.next_retry_at = state.next_retry_at;
                    } else {
                        summary.status = NodeStatus::Retrying;
                        summary.last_error = Some("outbound connector is not running".into());
                    }
                }
                NodeRole::Secondary => {
                    if self
                        .registry
                        .connected_names()
                        .await
                        .contains(&summary.config.name)
                    {
                        summary.status = NodeStatus::Connected;
                    }
                }
            }
        }
        summary
    }

    async fn stop(&self, name: &str) {
        let previous = self.connectors.lock().await.remove(name);
        if let Some(connector) = previous {
            connector.stop().await;
        }
        self.registry.revoke(name).await;
    }

    async fn reconcile(&self, config: &NodeConfig) -> Result<()> {
        self.stop(&config.name).await;
        if config.enabled && config.role == NodeRole::Primary {
            let connector = spawn_node_connector(
                PrimaryConnection::try_from(config)?,
                Arc::clone(self.config),
                self.events.clone(),
            );
            self.connectors
                .lock()
                .await
                .insert(config.name.clone(), connector);
        }
        Ok(())
    }

    pub async fn restore(&self) -> Result<()> {
        let _guard = self.registry.config_lock.lock().await;
        for config in self.db.list_nodes().await? {
            self.reconcile(&config).await?;
        }
        Ok(())
    }
}
