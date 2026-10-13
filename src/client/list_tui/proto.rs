// Wire-payload dumps: types carried from the modal dialogs and Ctrl+K
// Ctrl+Delete hotkeys out to the daemon's RPC layer. The companion methods
// (`CloneLaunch::request`, `SessionUpdate::request`) live here too. The
// dialog types it round-trips through call into these accessors.
use crate::protocol::RpcRequest;

#[derive(Debug, Eq, PartialEq)]
pub struct SessionTarget {
    pub id: String,
    pub node: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct SessionUpdate {
    pub id: String,
    pub node: Option<String>,
    pub title: Option<String>,
    pub tags: Option<Vec<String>>,
    pub notifications_enabled: Option<bool>,
}

impl SessionUpdate {
    pub fn request(&self) -> RpcRequest {
        wrap_node(
            self.node.as_deref(),
            RpcRequest::SessionMetadataSet {
                id: self.id.clone(),
                title: self.title.clone(),
                tags: self.tags.clone(),
                notifications_enabled: self.notifications_enabled,
            },
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct CloneLaunch {
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub node: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub disable_notifications: bool,
    pub attach_after_start: bool,
    /// Only set for duplicate sessions when the user opts into removing the source.
    pub remove_source: Option<SessionTarget>,
}

impl CloneLaunch {
    pub fn request(&self) -> RpcRequest {
        wrap_node(
            self.node.as_deref(),
            RpcRequest::Start {
                title: self.title.clone(),
                tags: self.tags.clone(),
                cmd: self.command.clone(),
                args: self.args.clone(),
                cwd: self.cwd.clone(),
                rows: self.rows,
                cols: self.cols,
                disable_notifications: self.disable_notifications,
            },
        )
    }
}

pub fn wrap_node(node: Option<&str>, inner: RpcRequest) -> RpcRequest {
    match node {
        Some(node) => RpcRequest::NodeProxy {
            node: node.to_string(),
            inner: Box::new(inner),
        },
        None => inner,
    }
}
