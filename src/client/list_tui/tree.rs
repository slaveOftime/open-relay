// Tree presentations of the session list. The data shape lives here;
// formatting helpers used by both render_tree and the inline session line
// (TREE_STATUS_WIDTH) stay in render.rs because they're a tighter pairing
// with the actual ratatui Cell rendering than with the tree-state itself.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
pub const TREE_AUTO_DEPTH: usize = 1;

pub fn common_path_prefix(paths: &[PathBuf]) -> PathBuf {
    if paths.is_empty() {
        return Path::new("/").to_path_buf();
    }
    if paths.len() == 1 {
        return paths[0]
            .ancestors()
            .find(|ancestor| ancestor.has_root() && ancestor.parent().is_none())
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
    }
    let mut iter = paths.iter();
    let first = iter.next().expect("non-empty").clone();
    let mut prefix = first.clone();
    for candidate in iter {
        while !candidate.starts_with(&prefix) {
            if !prefix.pop() {
                return Path::new("/").to_path_buf();
            }
        }
    }
    // The basename of the shared path is always shown as a folder, including
    // when one cwd equals that path and the others descend from it.
    prefix.parent().map_or(prefix.clone(), Path::to_path_buf)
}

/// Walk the tree, creating intermediate folder nodes for any portion of
/// `path` that does not yet exist. Returns the index of the deepest node
/// matching `path` (creating it on demand).
pub fn ensure_tree_path(
    tree: &mut TreeView,
    parent: usize,
    path: &Path,
    cwd: &str,
    node: &Option<String>,
) -> usize {
    if path.as_os_str().is_empty() {
        return parent;
    }
    let mut current = parent;
    let mut accumulated = PathBuf::new();
    for component in path.components() {
        // Skip absolute-path roots: they would create an intermediate
        // folder whose `file_name()` is `None` (rendered as "(root)")
        // and contribute nothing to the visible tree. The walker already
        // anchors the synthetic root; cwd paths are stored relative to it.
        if matches!(component, std::path::Component::RootDir) {
            continue;
        }
        accumulated.push(component);
        // Linear-scan the parent's `subfolders` for an existing node with
        // this exact `accumulated` path. Trees are shallow (≤ a handful of
        // folders per branch) so the linear scan is cheaper than the cache
        // bookkeeping it would evict.
        let next = tree.nodes[current]
            .subfolders
            .iter()
            .copied()
            .find(|&idx| tree.nodes[idx].path == accumulated);
        current = match next {
            Some(idx) => idx,
            None => {
                // Pop components from the original spelling of the cwd.
                // A remote Unix path viewed on Windows must keep its slashes.
                let mut absolute = PathBuf::from(cwd);
                for _ in 0..path
                    .components()
                    .filter(|c| !matches!(c, std::path::Component::RootDir))
                    .count()
                    .saturating_sub(accumulated.components().count())
                {
                    absolute.pop();
                }
                append_tree_node(
                    tree,
                    current,
                    accumulated.clone(),
                    Some(absolute.to_string_lossy().into_owned()),
                    node.clone(),
                    false,
                )
            }
        };
    }
    current
}

pub fn append_tree_node(
    tree: &mut TreeView,
    parent: usize,
    path: PathBuf,
    cwd: Option<String>,
    node: Option<String>,
    is_node: bool,
) -> usize {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let idx = tree.nodes.len();
    tree.nodes.push(TreeNode {
        path,
        cwd,
        node,
        is_node,
        name,
        direct_sessions: Vec::new(),
        subfolders: Vec::new(),
    });
    tree.nodes[parent].subfolders.push(idx);
    idx
}

/// Free-function DFS over a tree-node snapshot. Operations that mutate the
/// App live outside this walker; we only emit `TreeEntry` rows into the
/// supplied `out: &mut Vec`.
///
/// Visibility semantics:
///   * A node at depth ≤ `auto_depth` is always auto-visible.
///   * A node at depth > `auto_depth` is visible iff any of its ancestors
///     (whose path is in `drilled`) opens up the subtree below the horizon.
///   * Empty folders with neither sessions nor descendants are skipped —
///     empty labels communicate nothing.
///
/// Drilling is intentionally "open the *whole* subtree": once a folder is
/// drilled, every descendant of it remains visible without further user
/// action (they will be hidden again if the user un-drills the same path).
pub fn walk_tree_branch(
    nodes: &[TreeNode],
    node_idx: usize,
    depth: usize,
    auto_depth: usize,
    drilled: &HashSet<(Option<String>, PathBuf)>,
    out: &mut Vec<TreeEntry>,
) {
    let node = &nodes[node_idx];

    if node.direct_sessions.is_empty() && node.subfolders.is_empty() {
        return;
    }

    if !node.is_visible(depth, auto_depth, drilled) {
        return;
    }

    if depth > 0 {
        out.push(TreeEntry::Folder {
            node: node_idx,
            depth,
        });
    }

    // Subfolders first (file-explorer style: folders grouped at the top,
    // sessions below), then the sessions hosted by this folder. Both
    // lists are sorted alphabetically by `build_tree_nodes` so the walker
    // just iterates them in deterministic order.
    for &child_idx in &node.subfolders {
        walk_tree_branch(nodes, child_idx, depth + 1, auto_depth, drilled, out);
    }

    for session_idx in &node.direct_sessions {
        out.push(TreeEntry::Session {
            session: *session_idx,
            depth: depth + 1,
        });
    }
}

impl TreeNode {
    /// Visibility rule for a single node at the given chain depth. Encoded
    /// into a method so tests can exercise it without rebuilding the whole
    /// tree.
    pub fn is_visible(
        &self,
        depth: usize,
        auto_depth: usize,
        drilled: &HashSet<(Option<String>, PathBuf)>,
    ) -> bool {
        if depth <= auto_depth {
            return true;
        }
        is_in_drill_subtree(&self.path, &self.node, drilled)
    }
}

/// True when this folder's path, or any of its ancestors, was explicitly
/// drilled into. Walks up to the root so Enter-presses at depth auto_depth
/// cascade visibility down to every descendant.
pub fn is_in_drill_subtree(
    path: &Path,
    node: &Option<String>,
    drilled: &HashSet<(Option<String>, PathBuf)>,
) -> bool {
    let mut cursor = Some(path.to_path_buf());
    while let Some(current) = cursor.take() {
        if drilled.contains(&(node.clone(), current.clone())) {
            return true;
        }
        match current.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                cursor = Some(parent.to_path_buf());
            }
            _ => return false,
        }
    }
    false
}

impl TreeEntry {
    pub fn depth(self) -> usize {
        match self {
            Self::Folder { depth, .. } | Self::Session { depth, .. } => depth,
        }
    }
}
