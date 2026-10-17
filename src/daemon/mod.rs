mod auth;
pub(crate) mod components;
mod crash;
pub(crate) mod health;
mod journal_retention;
mod lifecycle;
mod node_manager;
mod reload;
mod rpc;
mod rpc_attach;
mod rpc_logs;
mod rpc_nodes;

use std::{collections::HashMap, sync::Arc};
use tokio::sync::Mutex;

use crate::{
    notification::event::NotificationEvent,
    session::{SessionEvent, SessionStore},
};

pub use lifecycle::{start, status, stop};

pub(crate) type SessionStoreHandle = Arc<SessionStore>;
pub(crate) type NodeConnectors = Arc<Mutex<HashMap<String, rpc_nodes::NodeConnector>>>;
pub(crate) type NotificationTx = tokio::sync::broadcast::Sender<NotificationEvent>;
pub(crate) type SessionEventTx = tokio::sync::broadcast::Sender<SessionEvent>;
pub(crate) type NotifierHandle = crate::notification::dispatcher::SharedNotifier;
