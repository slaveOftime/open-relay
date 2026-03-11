mod auth;
mod lifecycle;
mod rpc;
mod rpc_attach;
mod rpc_nodes;

use std::{collections::HashMap, sync::Arc};
use tokio::sync::{Mutex, watch};

use crate::{notification::event::NotificationEvent, session::SessionStore};

pub use lifecycle::{start, stop};

pub(crate) type SessionStoreHandle = Arc<Mutex<SessionStore>>;
pub(crate) type JoinHandles =
    Arc<Mutex<HashMap<String, (tokio::task::AbortHandle, watch::Sender<bool>)>>>;
pub(crate) type NotificationTx = tokio::sync::broadcast::Sender<NotificationEvent>;
