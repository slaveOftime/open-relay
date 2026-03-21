mod auth;
mod lifecycle;
mod rpc;
mod rpc_attach;
mod rpc_nodes;

use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, watch};

use crate::{
    notification::event::NotificationEvent,
    session::{SessionEvent, SessionStore},
};

pub use lifecycle::{start, stop};

pub(crate) type SessionStoreHandle = Arc<SessionStore>;
pub(crate) type JoinHandles =
    Arc<Mutex<HashMap<String, (tokio::task::AbortHandle, watch::Sender<bool>)>>>;
pub(crate) type NotificationTx = tokio::sync::broadcast::Sender<NotificationEvent>;
pub(crate) type SessionEventTx = tokio::sync::broadcast::Sender<SessionEvent>;
