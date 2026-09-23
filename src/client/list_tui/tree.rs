// Tree presentations of the session list. The data shape lives here;
// formatting helpers used by both render_tree and the inline session line
// (TREE_STATUS_WIDTH) stay in render.rs because they're a tighter pairing
// with the actual ratatui Cell rendering than with the tree-state itself.
use std::collections::HashSet;
use std::path::PathBuf;


impl Default for TreeView {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level presentation of the session list: either the responsive table
/// view (`List`) or a `cwd`-grouped tree (`Tree`). Toggled with Ctrl+G.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ViewMode {
    #[default]
    List,
    Tree,
}

/// One folder node in the tree. Folder rows are emitted by `TreeView`'s
/// `visible` walker; their `direct_sessions` are emitted as siblings at the
/// same effective depth so each session sits visually beneath the deepest
/// folder that still sits above it.
#[derive(Debug)]
pub struct TreeNode {
    pub path: PathBuf,
    /// Full cwd for folders, even when the displayed path is abbreviated.
    pub cwd: Option<String>,
    /// Owner of this branch; keeps identical paths on different nodes distinct.
    pub node: Option<String>,
    pub is_node: bool,
    /// Cached `basename` of `path`; empty for the synthetic root.
    pub name: String,
    /// Sessions whose cwd resolves to this folder.
    pub direct_sessions: Vec<usize>,
    /// Indices of child folder nodes (in DFS order; not sorted).
    pub subfolders: Vec<usize>,
}

/// What `TreeView::visible` emits: a single row that is either a folder
/// header or a session leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeEntry {
    Folder { node: usize, depth: usize },
    Session { session: usize, depth: usize },
}

/// Folder-and-session tree built from `App::sessions`. The walker produces a
/// flat list of `TreeEntry` rows in DFS order, retaining parent folders
/// and respecting `auto_depth` (folders deeper than this are hidden
/// unless `drilled` contains an ancestor path).
#[derive(Debug)]
pub struct TreeView {
    pub nodes: Vec<TreeNode>,
    /// Index of the synthetic root node (path = `PathBuf::new()`).
    pub root: usize,
    /// Auto-drill horizon. Folders at depth ≤ this are visible by default;
    /// deeper folders are hidden unless an ancestor path is in `drilled`.
    pub auto_depth: usize,
    /// Paths the user has explicitly drilled into (overrides `auto_depth`).
    pub drilled: HashSet<(Option<String>, PathBuf)>,
    pub grouped_nodes: bool,
    /// Flat row list produced by `recompute_visible`.
    pub visible: Vec<TreeEntry>,
    /// Cursor index into `visible`.
    pub cursor: usize,
}

impl TreeView {
    pub fn new() -> Self {
        let mut nodes = Vec::with_capacity(64);
        nodes.push(TreeNode {
            path: PathBuf::new(),
            cwd: None,
            node: None,
            is_node: false,
            name: String::new(),
            direct_sessions: Vec::new(),
            subfolders: Vec::new(),
        });
        Self {
            nodes,
            root: 0,
            auto_depth: TREE_AUTO_DEPTH,
            drilled: HashSet::new(),
            grouped_nodes: false,
            visible: Vec::new(),
            cursor: 0,
        }
    }
}

/// Maximum folder depth shown without an explicit drill. Pressing Enter on a
/// folder at this depth reveals one more level of children.
pub const TREE_AUTO_DEPTH: usize = 2;
