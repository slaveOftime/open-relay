// App-level types and helpers: status filter, sort strategy, the
// opened-terminal registry, the rate-state tracker, and the comparatively
// tiny helpers (is_active_status, session_is_active, sort_sessions,
// session_search_text, session_key, session_sort_label) used by both the
// App impl block and the renderer when deriving keys/status words/labels.
use std::path::PathBuf;
use std::time::Instant;

use crate::error::Result;
use crate::protocol::SessionSummary;
use chrono::{DateTime, Utc};
use std::collections::VecDeque;

use super::constants::{RATE_HISTORY_LEN, REFRESH_INTERVAL};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StatusFilter {
    #[default]
    All,
    Active,
    Inactive,
}

pub fn is_active_status(status: &str) -> bool {
    matches!(status, "created" | "running" | "stopping")
}

/// A session is "active" for sorting while it is alive (created/running/
/// stopping) or waiting for input, so attention-needed rows never sink below
/// finished ones.
pub fn session_is_active(session: &SessionSummary) -> bool {
    is_active_status(&session.status) || session.input_needed
}

/// Row ordering strategies for the session list, cycled with Ctrl+O.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortStrategy {
    /// Active sessions (running, waiting for input, …) first, then stopped/
    /// failed ones; each group sorted by creation time, newest first.
    #[default]
    ActiveFirst,
    /// All sessions by creation time, newest first.
    CreatedDesc,
    /// All sessions by creation time, oldest first.
    CreatedAsc,
}

impl SortStrategy {
    pub fn label(self) -> &'static str {
        match self {
            Self::ActiveFirst => "active first",
            Self::CreatedDesc => "newest",
            Self::CreatedAsc => "oldest",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::ActiveFirst => Self::CreatedDesc,
            Self::CreatedDesc => Self::CreatedAsc,
            Self::CreatedAsc => Self::ActiveFirst,
        }
    }
}

pub fn sort_sessions(sessions: &mut [SessionSummary], strategy: SortStrategy) {
    match strategy {
        SortStrategy::ActiveFirst => sessions.sort_by(|a, b| {
            session_is_active(b)
                .cmp(&session_is_active(a))
                .then_with(|| b.created_at.cmp(&a.created_at))
        }),
        SortStrategy::CreatedDesc => {
            sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at))
        }
        SortStrategy::CreatedAsc => sessions.sort_by_key(|session| session.created_at),
    }
}

impl StatusFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Inactive => "inactive",
        }
    }
}

#[derive(Debug)]
pub struct OpenedTerminal {
    pub marker: PathBuf,
    pub launched_at: Instant,
}

#[derive(Debug)]
pub struct RateState {
    pub total_bytes: u64,
    pub output_epoch: Option<DateTime<Utc>>,
    pub sampled_at: Instant,
    pub previous_rate: f64,
    pub rate: f64,
    pub history: VecDeque<f64>,
}

impl RateState {
    pub fn new(session: &SessionSummary, now: Instant) -> Self {
        Self {
            total_bytes: session.last_total_bytes,
            output_epoch: session.last_output_epoch,
            sampled_at: now,
            previous_rate: 0.0,
            rate: 0.0,
            history: VecDeque::from([0.0]),
        }
    }

    pub fn sample(&mut self, session: &SessionSummary, now: Instant) {
        let elapsed = now.saturating_duration_since(self.sampled_at).as_secs_f64();
        let bytes = session.last_total_bytes.saturating_sub(self.total_bytes);
        self.previous_rate = self.display_rate(now);
        self.rate = if bytes > 0 && elapsed > 0.0 {
            bytes as f64 / elapsed
        } else {
            0.0
        };
        self.total_bytes = session.last_total_bytes;
        self.output_epoch = session.last_output_epoch;
        self.sampled_at = now;
        self.history.push_back(self.rate);
        if self.history.len() > RATE_HISTORY_LEN {
            self.history.pop_front();
        }
    }

    pub fn display_rate(&self, now: Instant) -> f64 {
        let progress = now.saturating_duration_since(self.sampled_at).as_secs_f64()
            / REFRESH_INTERVAL.as_secs_f64();
        let eased = progress.clamp(0.0, 1.0);
        self.previous_rate + (self.rate - self.previous_rate) * eased
    }
}

pub fn session_search_text(session: &SessionSummary) -> String {
    let mut text = format!("{}\n{}\n{}", session.id, session.command, session.status);
    for argument in &session.args {
        text.push('\n');
        text.push_str(argument);
    }
    for tag in &session.tags {
        text.push('\n');
        text.push_str(tag);
    }
    if let Some(cwd) = &session.cwd {
        text.push('\n');
        text.push_str(cwd);
    }
    if let Some(node) = &session.node {
        text.push('\n');
        text.push_str(node);
    }
    if let Some(title) = &session.title {
        text.push('\n');
        text.push_str(title);
    }
    if session.input_needed {
        text.push_str("\nattention");
    }
    if let Some(pid) = session.pid {
        text.push('\n');
        text.push_str(&pid.to_string());
    }
    text.to_lowercase()
}

pub fn session_key(session: &SessionSummary) -> String {
    session.node.as_ref().map_or_else(
        || session.id.clone(),
        |node| format!("{node}\0{}", session.id),
    )
}

/// Display label used to sort sessions inside a folder. Mirrors what the
/// tree-view renders: command + args + title. Title is appended last so
/// sessions sharing a command line still bubble up in their own alpha
/// order.
pub fn session_sort_label(session: &SessionSummary) -> String {
    let mut label = session.command.clone();
    if !session.args.is_empty() {
        label.push(' ');
        label.push_str(&session.args.join(" "));
    }
    if let Some(title) = &session.title
        && !title.is_empty()
    {
        label.push(' ');
        label.push_str(title);
    }
    label
}

use std::{
    collections::{HashMap, HashSet},
    process::Command,
    time::Duration,
};

use tachyonfx::{EffectManager, RefRect};

use super::dialog::{CloneDialog, RemoveDialog, UpdateDialog};
use super::keys::AppAction;
use super::spawn::{session_command, spawn_session_terminal, terminal_marker};
use super::terminal::{TuiTerminal, wait_for_ctrl_d};
use super::tree::{
    TREE_AUTO_DEPTH, TreeEntry, TreeNode, TreeView, ViewMode, append_tree_node, common_path_prefix,
    ensure_tree_path, walk_tree_branch,
};

#[derive(Default)]
pub struct App {
    pub sessions: Vec<SessionSummary>,
    pub rates: HashMap<String, RateState>,
    pub selected: usize,
    pub opened: HashMap<String, OpenedTerminal>,
    pub next_slot: usize,
    pub message: Option<String>,
    /// True when `message` carries user-action feedback ("started new
    /// session …", "update failed: …") rather than refresh-cycle sync
    /// status. The 250 ms refresh cycle must not clobber action feedback —
    /// otherwise success/error messages vanish before the user can read
    /// them (and a `--node`-scoped Ctrl+N looks like "nothing happened").
    pub message_is_action: bool,
    pub filter: String,
    pub normalized_filter: String,
    pub search_text: Vec<String>,
    pub visible: Vec<usize>,
    pub status_filter: StatusFilter,
    pub sort_strategy: SortStrategy,
    pub clone_dialog: Option<CloneDialog>,
    pub update_dialog: Option<UpdateDialog>,
    pub remove_dialog: Option<RemoveDialog>,
    pub show_node: bool,
    pub view_mode: ViewMode,
    pub tree: TreeView,
    /// Local session storage root used to place cwd-less sessions in the tree.
    pub session_storage_dir: Option<PathBuf>,
    /// Shader-like visual effects (tachyonfx) processed on every frame.
    pub effects: EffectManager<String>,
    /// Timestamp of the previous frame, used to derive the effect tick delta.
    pub last_frame_at: Option<Instant>,
    /// Row rectangles of sessions waiting for input, keyed by session key.
    /// Each gets its own background pulse effect; the [`RefRect`] is updated
    /// every frame so the pulse follows the row across scrolling, reordering
    /// and resizes. The boolean tracks selection: a selected row pulses
    /// toward a different tint, so a selection change re-registers the effect.
    pub attention_rows: HashMap<String, (RefRect, bool)>,
    /// The message text the fade-in effect was last registered for, so the
    /// fade replays only when the message actually changes.
    pub rendered_message: Option<String>,
    /// The dialog the open-fade effect was last registered for.
    pub rendered_dialog: Option<&'static str>,
}

impl App {
    /// Refresh-cycle sync status (warnings / "sync lost"): replaces the
    /// current message only when no action feedback is showing, so a fresh
    /// warning never silently discards feedback the user hasn't read yet.
    pub(super) fn set_refresh_message(&mut self, message: Option<String>) {
        if !self.message_is_action {
            self.message = message;
        }
    }

    /// User-action feedback: always shown, and stays until the next action
    /// or an explicit clear; the refresh cycle leaves it alone.
    pub(super) fn set_action_message(&mut self, message: Option<String>) {
        self.message_is_action = message.is_some();
        self.message = message;
    }

    pub(super) fn replace_sessions(&mut self, mut sessions: Vec<SessionSummary>) {
        sort_sessions(&mut sessions, self.sort_strategy);
        let selected_key = self.sessions.get(self.selected).map(session_key);
        let now = Instant::now();
        let session_keys = sessions.iter().map(session_key).collect::<HashSet<_>>();
        let attach_counts = sessions
            .iter()
            .map(|session| (session_key(session), session.attach_count))
            .collect::<HashMap<_, _>>();
        for session in &sessions {
            let key = session_key(session);
            match self.rates.get_mut(&key) {
                Some(rate) => rate.sample(session, now),
                None => {
                    self.rates.insert(key, RateState::new(session, now));
                }
            }
        }
        self.rates.retain(|key, _| session_keys.contains(key));
        self.opened.retain(|key, opened| {
            let attach_count = attach_counts.get(key).copied();
            let launch_pending = opened.launched_at.elapsed() < Duration::from_secs(5);
            let attached = attach_count.is_some_and(|count| count > 0);
            if opened.marker.exists() && !launch_pending && !attached {
                let _ = std::fs::remove_file(&opened.marker);
            }
            attach_count.is_some() && (launch_pending || opened.marker.exists())
        });
        self.search_text = sessions.iter().map(session_search_text).collect();
        self.sessions = sessions;
        self.selected = selected_key
            .and_then(|key| {
                self.sessions
                    .iter()
                    .position(|item| session_key(item) == key)
            })
            .unwrap_or_else(|| self.selected.min(self.sessions.len().saturating_sub(1)));
        self.rebuild_visible();
        self.rebuild_tree();
        self.sync_update_dialog();
    }

    pub(super) fn sync_update_dialog(&mut self) {
        let Some(target_id) = self
            .update_dialog
            .as_ref()
            .map(|dialog| dialog.target_id.clone())
        else {
            return;
        };
        let summary = self
            .sessions
            .iter()
            .find(|session| {
                session.id == target_id
                    && session.node.as_ref()
                        == self
                            .update_dialog
                            .as_ref()
                            .and_then(|dialog| dialog.target_node.as_ref())
            })
            .cloned();
        if let Some(dialog) = self.update_dialog.as_mut() {
            dialog.sync_summary(summary.as_ref());
        }
    }

    /// Drop a confirmed or optimistically removed session, preserving the
    /// surviving selection and cleaning up state keyed by both node and ID.
    pub(super) fn remove_session_payload(&mut self, id: &str, node: Option<&str>) {
        let Some(position) = self
            .sessions
            .iter()
            .position(|session| session.id == id && session.node.as_deref() == node)
        else {
            return;
        };
        let selected_key = self.sessions.get(self.selected).map(session_key);
        let removed_key = session_key(&self.sessions[position]);
        self.attention_rows.remove(&removed_key);
        self.effects
            .cancel_unique_effect(super::effects::attention_pulse_key(&removed_key));
        self.rates.remove(&removed_key);
        self.opened.remove(&removed_key);
        self.sessions.remove(position);
        self.search_text = self.sessions.iter().map(session_search_text).collect();
        self.selected = selected_key
            .filter(|key| key != &removed_key)
            .and_then(|key| {
                self.sessions
                    .iter()
                    .position(|session| session_key(session) == key)
            })
            .unwrap_or_else(|| position.min(self.sessions.len().saturating_sub(1)));
        self.rebuild_visible();
    }
    pub(super) fn apply_updated_summary(&mut self, summary: SessionSummary) {
        let key = session_key(&summary);
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session_key(session) == key)
        else {
            return;
        };
        self.sessions[index] = summary;
        if let Some(search_text) = self.search_text.get_mut(index) {
            *search_text = session_search_text(&self.sessions[index]);
        } else {
            self.search_text = self.sessions.iter().map(session_search_text).collect();
        }
        self.rebuild_visible();
    }

    pub(super) fn rebuild_visible(&mut self) {
        if self.search_text.len() != self.sessions.len() {
            self.search_text = self.sessions.iter().map(session_search_text).collect();
        }
        self.visible.clear();
        self.visible.extend(
            self.sessions
                .iter()
                .enumerate()
                .filter(|(index, session)| {
                    let active = is_active_status(&session.status);
                    let status_matches = match self.status_filter {
                        StatusFilter::All => true,
                        StatusFilter::Active => active,
                        StatusFilter::Inactive => !active,
                    };
                    status_matches
                        && (self.normalized_filter.is_empty()
                            || self
                                .search_text
                                .get(*index)
                                .is_some_and(|text| text.contains(&self.normalized_filter)))
                })
                .map(|(index, _)| index),
        );
        if !self.visible.contains(&self.selected)
            && let Some(index) = self.visible.first()
        {
            self.selected = *index;
        }
        self.rebuild_tree();
    }

    pub(super) fn selected_session(&self) -> Option<&SessionSummary> {
        self.visible
            .contains(&self.selected)
            .then(|| self.sessions.get(self.selected))
            .flatten()
    }

    /// Return the session the user is currently focused on regardless of
    /// view mode. In list mode, that's `self.selected` (sanity-checked
    /// against `self.visible`). In tree mode, the user's arrow nav
    /// drives `tree.cursor`, so the focused session is the one sitting
    /// under that cursor — *without* any visibility pre-check, because
    /// `visible` is the filtered list and the tree view deliberately
    /// ignores filters (see P3.2 TODO).
    ///
    /// Keeping this in one helper means every dial action (Enter,
    /// Ctrl+D duplicate, Ctrl+U update, inline open) reads the same
    /// "what is the user pointing at" answer, no matter which view is
    /// showing.
    pub(super) fn focused_session(&self) -> Option<&SessionSummary> {
        match self.view_mode {
            ViewMode::List => self.selected_session(),
            ViewMode::Tree => {
                self.tree
                    .visible
                    .get(self.tree.cursor)
                    .and_then(|entry| match entry {
                        TreeEntry::Session { session, .. } => self.sessions.get(*session),
                        TreeEntry::Folder { .. } => None,
                    })
            }
        }
    }

    pub(super) fn select_visible(&mut self, offset: isize) {
        if self.visible.is_empty() {
            return;
        }
        let position = self
            .visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0) as isize;
        let next = (position + offset).rem_euclid(self.visible.len() as isize) as usize;
        if let Some(index) = self.visible.get(next).copied() {
            self.selected = index;
        }
    }

    pub(super) fn previous(&mut self) {
        self.select_visible(-1);
    }

    pub(super) fn next(&mut self) {
        self.select_visible(1);
    }

    /// Rebuild the folder tree from `self.sessions` and recompute its visible
    /// flat row list. Tree state and cursor are preserved: the cursor snaps to
    /// the previously selected session if it survives the rebuild, otherwise
    /// to the first row.
    pub(super) fn rebuild_tree(&mut self) {
        // Snapshot the row the cursor is currently on so we can re-locate
        // the same logical *row* (folder or session) after a refresh
        // re-runs the walker. Folder rows move around whenever a sibling
        // session appears or disappears, so the cursor must remember the
        // folder by *path* — not just by index.
        let previously_focused_session =
            self.tree
                .visible
                .get(self.tree.cursor)
                .and_then(|entry| match entry {
                    TreeEntry::Session { session, .. } => Some(*session),
                    TreeEntry::Folder { .. } => None,
                });
        let previously_focused_folder =
            self.tree
                .visible
                .get(self.tree.cursor)
                .and_then(|entry| match entry {
                    TreeEntry::Folder { node, .. } => Some((
                        self.tree.nodes[*node].node.clone(),
                        self.tree.nodes[*node].path.clone(),
                    )),
                    TreeEntry::Session { .. } => None,
                });

        self.build_tree_nodes();
        let visible = {
            let TreeView {
                ref nodes,
                root,
                auto_depth,
                grouped_nodes,
                auto_expand_all,
                ref drilled,
                ref collapsed,
                ..
            } = self.tree;
            let auto_depth = if auto_expand_all {
                usize::MAX
            } else {
                auto_depth + usize::from(grouped_nodes)
            };
            let mut out: Vec<TreeEntry> = Vec::new();
            // Include cwd-less sessions attached directly to the synthetic
            // root as well as node and folder branches.
            walk_tree_branch(nodes, root, 0, auto_depth, drilled, collapsed, &mut out);
            out
        };
        self.tree.visible = visible;

        // Restore cursor: prefer the same session → the same folder →
        // otherwise the last row. The folder fallback is what keeps
        // arrow-key navigation alive across a daemon refresh tick that
        // happens to land with the cursor on a parent row.
        self.tree.cursor = previously_focused_session
            .and_then(|session_idx| {
                self.tree.visible.iter().position(|entry| {
                    matches!(entry, TreeEntry::Session { session, .. } if *session == session_idx)
                })
            })
            .or_else(|| {
                previously_focused_folder.as_ref().and_then(|path| {
                    self.tree.visible.iter().position(|entry| {
                        matches!(entry, TreeEntry::Folder { node, .. }
                            if self.tree.nodes[*node].node == path.0 && self.tree.nodes[*node].path == path.1)
                    })
                })
            })
            .unwrap_or_else(|| self.tree.visible.len().saturating_sub(1));
    }

    /// Phase 1 of tree rebuild: clear the prior nodes and re-derive them
    /// from `self.sessions`. Sessions without a `cwd` (or whose cwd matches
    /// the shared prefix) are bucketed onto the synthetic root.
    fn session_tree_cwd(&self, session: &SessionSummary) -> PathBuf {
        if let Some(cwd) = session.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            return PathBuf::from(cwd);
        }
        // A secondary node's storage root is opaque to this client. Use a
        // logical sessions/<id> path under that node's tree branch.
        let root = if session.node.is_none() {
            self.session_storage_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("sessions"))
        } else {
            PathBuf::from("sessions")
        };
        root.join(&session.id)
    }

    pub(super) fn build_tree_nodes(&mut self) {
        self.tree.nodes.clear();
        self.tree.nodes.push(TreeNode {
            path: PathBuf::new(),
            cwd: None,
            node: None,
            is_node: false,
            name: String::new(),
            direct_sessions: Vec::new(),
            subfolders: Vec::new(),
        });
        self.tree.root = 0;
        if self.tree.auto_depth == 0 {
            self.tree.auto_depth = TREE_AUTO_DEPTH;
        }
        self.tree.auto_expand_all =
            !self.normalized_filter.is_empty() || self.status_filter != StatusFilter::All;

        // Strip the longest shared path prefix so sessions are bucketed under
        // their first differing ancestor. Sessions with no `cwd` and sessions
        // whose cwd equals the common prefix land directly on the synthetic
        // root as direct_sessions.
        //
        // The tree honours both the active search filter and the status
        // filter so users can scope the tree the same way they scope the
        // flat list. Sessions that don't match are simply not bucketed,
        // and folders that end up empty after filtering collapse
        // gracefully (the walker skips empty leaves).
        let matches_filter = |index: usize, session: &SessionSummary| {
            let active = is_active_status(&session.status);
            let status_ok = match self.status_filter {
                StatusFilter::All => true,
                StatusFilter::Active => active,
                StatusFilter::Inactive => !active,
            };
            if !status_ok {
                return false;
            }
            if self.normalized_filter.is_empty() {
                return true;
            }
            self.search_text
                .get(index)
                .is_some_and(|text| text.contains(&self.normalized_filter))
        };
        let filtered_sessions: Vec<(usize, &SessionSummary)> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(index, session)| matches_filter(*index, session))
            .collect();

        let mut node_names: Vec<Option<String>> = filtered_sessions
            .iter()
            .map(|(_, session)| session.node.clone())
            .collect();
        node_names.sort();
        node_names.dedup();
        self.tree.grouped_nodes = node_names.len() > 1;

        // Prefixes must be computed per node: a remote cwd must never
        // abbreviate a local path (or a path on another remote machine).
        for node_name in node_names {
            let group_root = if self.tree.grouped_nodes {
                let idx = append_tree_node(
                    &mut self.tree,
                    0,
                    PathBuf::new(),
                    None,
                    node_name.clone(),
                    true,
                );
                self.tree.nodes[idx].name =
                    node_name.clone().unwrap_or_else(|| "local".to_string());
                idx
            } else {
                self.tree.root
            };
            let group_sessions: Vec<_> = filtered_sessions
                .iter()
                .copied()
                .filter(|(_, session)| session.node == node_name)
                .collect();
            let cwds: Vec<PathBuf> = group_sessions
                .iter()
                .map(|(_, session)| self.session_tree_cwd(session))
                .filter(|path| !path.as_os_str().is_empty())
                .collect();
            let common = common_path_prefix(&cwds);
            for (index, session) in group_sessions {
                let cwd = self.session_tree_cwd(session);
                let cwd_text = cwd.to_string_lossy().into_owned();
                let effective = if self.tree.auto_expand_all {
                    cwd.clone()
                } else {
                    cwd.strip_prefix(&common)
                        .map_or_else(|_| cwd.clone(), |stripped| stripped.to_path_buf())
                };
                let leaf_idx = ensure_tree_path(
                    &mut self.tree,
                    group_root,
                    &effective,
                    &cwd_text,
                    &node_name,
                );
                self.tree.nodes[leaf_idx].direct_sessions.push(index);
            }
        }
        // Sort children folders and direct sessions alphabetically so the
        // walker traverses them in a stable, easy-to-scan order across
        // refreshes. Folder names and session labels are compared
        // case-insensitively because human-friendly ordering should not
        // depend on whether the cwd had capitals.
        self.sort_tree_nodes();

        // Prune folders that ended up empty after filtering. Without
        // this, a search filter that hides every session under `proj/`
        // would still leave `proj/` (and any empty ancestors above it)
        // visible as zero-content rows. Folders that survive are the
        // ones hosting at least one (possibly indirect) matching
        // session.
        self.prune_empty_tree_nodes();
    }

    /// Drop every folder whose `subfolders` post-prune is empty *and*
    /// whose `direct_sessions` is empty. Repeatedly shrinking the
    /// list-of-children keeps parent folders in line: a folder is only
    /// considered empty once all of its descendants have been
    /// eliminated. The synthetic root (`index 0`) is preserved even if
    /// it ends up empty — the empty-state message still wants to be
    /// anchored somewhere.
    pub(super) fn prune_empty_tree_nodes(&mut self) {
        // Iterate until a pass removes nothing. Removing a child from a
        // parent can empty the parent itself, which the next pass
        // catches, and so on up the chain.
        loop {
            let mut empty_indices: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            for (idx, node) in self.tree.nodes.iter().enumerate() {
                if idx == self.tree.root {
                    continue;
                }
                if node.subfolders.is_empty() && node.direct_sessions.is_empty() {
                    empty_indices.insert(idx);
                }
            }
            if empty_indices.is_empty() {
                break;
            }
            let removed = empty_indices.len();
            for parent in &mut self.tree.nodes {
                parent
                    .subfolders
                    .retain(|child| !empty_indices.contains(child));
            }
            // Invalidate the survivor set every pass so a chain like
            // `root → proj` collapses once `proj` becomes empty.
            let mut still_empty: std::collections::HashSet<usize> =
                std::collections::HashSet::new();
            for (idx, node) in self.tree.nodes.iter().enumerate() {
                if idx == self.tree.root {
                    continue;
                }
                if node.subfolders.is_empty() && node.direct_sessions.is_empty() {
                    still_empty.insert(idx);
                }
            }
            let _ = removed;
            if still_empty == empty_indices {
                break;
            }
        }
    }

    /// Sort every node's `subfolders` (by `name`) and `direct_sessions`
    /// (by cmd + args + title). Both lists are stable-sorted so existing
    /// display ties keep their insertion order.
    pub(super) fn sort_tree_nodes(&mut self) {
        let labels: Vec<String> = (0..self.sessions.len())
            .map(|index| session_sort_label(&self.sessions[index]))
            .collect();
        // Snapshot folder names so the closures can borrow them immutably
        // without re-borrowing the same `nodes` vector being mutated.
        let folder_names: Vec<String> = self
            .tree
            .nodes
            .iter()
            .map(|node| node.name.to_lowercase())
            .collect();
        for node in &mut self.tree.nodes {
            let mut children = node.subfolders.clone();
            children.sort_by(|&a, &b| folder_names[a].cmp(&folder_names[b]));
            node.subfolders = children;
            let mut sessions = node.direct_sessions.clone();
            sessions.sort_by(|&a, &b| labels[a].to_lowercase().cmp(&labels[b].to_lowercase()));
            node.direct_sessions = sessions;
        }
    }

    pub(super) fn toggle_tree_drill(&mut self) {
        let Some(TreeEntry::Folder { node, depth }) =
            self.tree.visible.get(self.tree.cursor).copied()
        else {
            return;
        };
        if self.tree.nodes[node].subfolders.is_empty()
            && self.tree.nodes[node].direct_sessions.is_empty()
        {
            return;
        }

        let key = self.tree.nodes[node].expansion_key();
        if self.tree.is_expanded(node, depth) {
            self.tree.drilled.remove(&key);
            self.tree.collapsed.insert(key);
        } else {
            self.tree.collapsed.remove(&key);
            self.tree.drilled.insert(key);
        }
        self.rebuild_tree();
    }
    /// Switch between list and tree presentations. The flat-list `visible`
    /// continues to reflect current sessions so a Ctrl+G <-> Ctrl+G round
    /// trip leaves selection unchanged.
    pub(super) fn toggle_view_mode(&mut self) {
        // Each mode owns its cursor independently:
        //   * tree mode uses `tree.cursor` and `tree.visible`;
        //   * list mode uses `self.selected` and `self.visible`.
        // We intentionally do **not** splice one cursor into the other
        // here, so a Ctrl+G round-trip leaves the user exactly where
        // they were in each mode last time. `focused_session` knows
        // which cursor to read for the active view.
        let next_view = match self.view_mode {
            ViewMode::List => ViewMode::Tree,
            ViewMode::Tree => ViewMode::List,
        };
        self.view_mode = next_view;
        self.rebuild_tree();
        self.focus_tree_on_focused_session();
        // Track and announce the new mode so users don't get disoriented.
        self.set_action_message(Some(match self.view_mode {
            ViewMode::List => "list view · Ctrl+G tree".to_string(),
            ViewMode::Tree => "tree view · Enter expand/collapse · Ctrl+G list".to_string(),
        }));
    }

    /// Focus the selected list session if it is visible; otherwise focus its
    /// deepest visible folder ancestor without expanding additional levels.
    pub(super) fn focus_tree_on_focused_session(&mut self) {
        if self.view_mode != ViewMode::Tree {
            return;
        }
        let Some(index) = self.visible.get(self.selected).copied() else {
            return;
        };
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        if let Some(position) = self.tree.visible.iter().position(
            |entry| matches!(entry, TreeEntry::Session { session, .. } if *session == index),
        ) {
            self.tree.cursor = position;
            return;
        }
        let cwd = self.session_tree_cwd(session);
        let session_node = session.node.as_deref();
        let common = common_path_prefix(
            &self
                .sessions
                .iter()
                .filter(|candidate| candidate.node.as_deref() == session_node)
                .map(|candidate| self.session_tree_cwd(candidate))
                .collect::<Vec<_>>(),
        );
        let effective = if self.tree.auto_expand_all {
            cwd.clone()
        } else {
            cwd.strip_prefix(&common)
                .map_or_else(|_| cwd.clone(), |stripped| stripped.to_path_buf())
        };
        if let Some((_, position)) = self
            .tree
            .visible
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| match entry {
                TreeEntry::Folder { node, .. } => {
                    let folder = &self.tree.nodes[*node];
                    (folder.node.as_deref() == session_node
                        && (folder.is_node && folder.path.as_os_str().is_empty()
                            || effective.starts_with(&folder.path)))
                    .then_some((folder.path.components().count(), position))
                }
                TreeEntry::Session { .. } => None,
            })
            .max_by_key(|(depth, _)| *depth)
        {
            self.tree.cursor = position;
        }
    }
    /// Move the tree cursor by `offset`. The cursor wraps around the visible
    /// row range (vim-style) so repeated Up/Down in tree mode feels
    /// continuous and matches what the flat-list cursor does in list mode.
    /// No-op in list mode so a stray call from `route_key` (which also drives
    /// the flat-list cursor) is harmless.
    pub(super) fn navigate_tree(&mut self, offset: isize) {
        if self.view_mode != ViewMode::Tree {
            return;
        }
        let visible = self.tree.visible.len();
        if visible == 0 {
            self.tree.cursor = 0;
            return;
        }
        let position = self.tree.cursor as isize;
        let len = visible as isize;
        let next = (position + offset).rem_euclid(len) as usize;
        self.tree.cursor = next;
    }

    pub(super) fn tree_home(&mut self) {
        if self.view_mode == ViewMode::Tree {
            self.tree.cursor = 0;
        }
    }

    pub(super) fn tree_last(&mut self) {
        if self.view_mode == ViewMode::Tree {
            self.tree.cursor = self.tree.visible.len().saturating_sub(1);
        }
    }

    /// Enter toggles a folder's child-folder expansion or opens a session.
    pub(super) fn tree_enter(&mut self) -> AppAction {
        match self.tree.visible.get(self.tree.cursor).copied() {
            Some(TreeEntry::Folder { .. }) => {
                self.toggle_tree_drill();
                AppAction::None
            }
            Some(TreeEntry::Session { session, .. }) => {
                self.selected = session;
                AppAction::OpenInline
            }
            None => AppAction::None,
        }
    }
    pub(super) fn first(&mut self) {
        if let Some(index) = self.visible.first() {
            self.selected = *index;
        }
    }

    pub(super) fn last(&mut self) {
        if let Some(index) = self.visible.last() {
            self.selected = *index;
        }
    }

    pub(super) fn update_text_filter(&mut self) {
        let normalized = self.filter.to_lowercase();
        if normalized != self.normalized_filter {
            self.tree.drilled.clear();
            self.tree.collapsed.clear();
        }
        self.normalized_filter = normalized;
        self.rebuild_visible();
        // Reset the cursor that matches the active view: `self.selected`
        // for list mode (via `first()`), `tree.cursor` for tree mode so
        // breadcrumbs don't drag the user off-screen after a filter
        // change drops rows out from under them.
        if self.view_mode == ViewMode::Tree {
            self.tree.cursor = 0;
        } else {
            self.first();
        }
        self.set_action_message(None);
    }

    pub(super) fn push_filter(&mut self, character: char) {
        self.filter.push(character);
        self.update_text_filter();
    }

    pub(super) fn pop_filter(&mut self) {
        self.filter.pop();
        self.update_text_filter();
    }

    pub(super) fn clear_filter(&mut self) {
        self.filter.clear();
        self.update_text_filter();
    }

    pub(super) fn cycle_sort_strategy(&mut self) {
        self.sort_strategy = self.sort_strategy.next();
        let selected_key = self.sessions.get(self.selected).map(session_key);
        sort_sessions(&mut self.sessions, self.sort_strategy);
        // The search text is indexed parallel to `sessions`, so re-derive it
        // after reordering.
        self.search_text = self.sessions.iter().map(session_search_text).collect();
        self.selected = selected_key
            .and_then(|key| {
                self.sessions
                    .iter()
                    .position(|session| session_key(session) == key)
            })
            .unwrap_or(0);
        self.rebuild_visible();
        self.set_action_message(Some(format!(
            "sorted by {} · Ctrl+O cycle",
            self.sort_strategy.label()
        )));
    }

    pub(super) fn toggle_status_filter(&mut self) {
        self.status_filter = match self.status_filter {
            StatusFilter::All => StatusFilter::Active,
            StatusFilter::Active => StatusFilter::Inactive,
            StatusFilter::Inactive => StatusFilter::All,
        };
        self.tree.drilled.clear();
        self.tree.collapsed.clear();
        self.rebuild_visible();
        if self.view_mode == ViewMode::Tree {
            // `first()` only drives the flat-list cursor, so reset the
            // tree cursor ourselves when the active view is the tree.
            self.tree.cursor = 0;
        } else {
            self.first();
        }
        self.set_action_message(Some(format!(
            "showing {} sessions · Ctrl+S toggle",
            self.status_filter.label()
        )));
    }

    pub(super) fn open_selected_terminal(&mut self, node: Option<&str>) {
        let Some(session) = self.focused_session() else {
            return;
        };
        let attach = is_active_status(&session.status);
        let id = session.id.clone();
        let target_node = session.node.as_deref().or(node).map(str::to_string);
        let key = session_key(session);
        let size = (session.cols.unwrap_or(80), session.rows.unwrap_or(24));
        if attach && self.opened.contains_key(&key) {
            self.set_action_message(Some(format!("{id} is already jacked in")));
            return;
        }

        let marker = attach.then(|| terminal_marker(&id));
        match spawn_session_terminal(
            &id,
            target_node.as_deref(),
            size,
            self.next_slot,
            attach,
            marker.as_deref(),
        ) {
            Ok(()) => {
                if let Some(marker) = marker {
                    self.opened.insert(
                        key,
                        OpenedTerminal {
                            marker,
                            launched_at: Instant::now(),
                        },
                    );
                }
                self.next_slot += 1;
                self.set_action_message(Some(if attach {
                    format!("opened {id} · link established")
                } else {
                    format!("opened {id} · log tail")
                }));
            }
            Err(error) => self.set_action_message(Some(format!("launch failed: {error}"))),
        }
    }
}

pub fn open_selected_inline(
    terminal: &mut TuiTerminal,
    app: &mut App,
    node: Option<&str>,
) -> Result<()> {
    let Some(session) = app.focused_session() else {
        return Ok(());
    };
    let id = session.id.clone();
    let target_node = session.node.as_deref().or(node).map(str::to_string);
    let attach = is_active_status(&session.status);
    open_session_inline(terminal, app, &id, target_node.as_deref(), attach)
}

pub fn open_session_inline(
    terminal: &mut TuiTerminal,
    app: &mut App,
    id: &str,
    node: Option<&str>,
    attach: bool,
) -> Result<()> {
    let (executable, args) = session_command(id, node, attach)?;

    terminal.suspend()?;
    let result = Command::new(executable).args(args).status();
    let wait_result = if attach { Ok(()) } else { wait_for_ctrl_d() };
    terminal.resume()?;
    wait_result?;

    app.set_action_message(Some(match result {
        Ok(status) if status.success() => format!("returned from {id}"),
        Ok(status) => format!("session {id} exited with {status}"),
        Err(error) => format!("open failed: {error}"),
    }));
    Ok(())
}
