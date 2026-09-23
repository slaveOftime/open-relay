use std::{
    any::Any,
    collections::{HashMap, HashSet, VecDeque},
    io::{self, IsTerminal, Write},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use futures_util::FutureExt;

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use tachyonfx::{
    CellFilter, CellIterator, ColorSpace, Duration as FxDuration, EffectManager, EffectTimer,
    Interpolation, RefRect, fx,
};

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Shadow, Sparkline, Table, TableState,
    },
};

use crate::{
    cli::ListArgs,
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse, SessionSummary},
    session::{MAX_SESSION_TITLE_LEN, normalize_session_tags, normalize_session_title},
};
use super::list::ListTarget;

pub(super) mod constants;
pub(super) use constants::{CLONE_DIALOG_HELP, DIALOG_FIELD_BG, DIALOG_LABEL_WIDTH, LIST_WINDOW_TITLE, TITLE_SAVE_BYTES, ATTENTION_PULSE_BG, ATTENTION_PULSE_BG_SELECTED, INPUT_POLL_INTERVAL, REDRAW_INTERVAL, REFRESH_INTERVAL, REFRESH_TIMEOUT, ANIMATION_REDRAW_INTERVAL, RATE_HISTORY_LEN, SPARK_BLOCKS, SPARKLINE_WIDTH, COMPACT_SPARKLINE_WIDTH, STOP_GRACE_SECONDS, UPDATE_DIALOG_HELP};
pub(crate) use constants::{TITLE_RESTORE_BYTES, TUI_RESTORE_BYTES};


struct SessionRefresh {
    sessions: Vec<SessionSummary>,
    failed_nodes: HashSet<Option<String>>,
    failures: Vec<String>,
}

impl SessionRefresh {
    fn warning(&self) -> Option<String> {
        (!self.failures.is_empty()).then(|| format!("sync lost: {}", self.failures.join(" · ")))
    }
}

pub(super) async fn run(
    config: &AppConfig,
    args: &ListArgs,
    targets: Vec<ListTarget>,
) -> Result<()> {
    match AssertUnwindSafe(run_inner(config, args, targets))
        .catch_unwind()
        .await
    {
        Ok(result) => result,
        Err(payload) => Err(AppError::Protocol(format!(
            "interactive session list crashed: {}",
            panic_payload_message(payload.as_ref())
        ))),
    }
}

async fn run_inner(config: &AppConfig, args: &ListArgs, targets: Vec<ListTarget>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::Protocol(
            "--follow requires an interactive terminal".to_string(),
        ));
    }
    #[cfg(windows)]
    crate::client::crash::install();

    let query = super::list::build_list_query(args)?;
    let mut app = App {
        show_node: targets.len() > 1,
        ..Default::default()
    };
    // When the list shows exactly one node's sessions, dialogs that start or
    // update sessions default to that node — Ctrl+N in a `--node worker`
    // view must create the session on `worker`, not locally (where it would
    // never appear in the filtered list).
    let list_node = match targets.as_slice() {
        [target] => target.node.as_deref(),
        _ => None,
    };
    let refresh = fetch_sessions(config, query.clone(), &targets).await?;
    crate::metrics::mark("tui: sessions fetched");
    app.set_refresh_message(refresh.warning());
    app.replace_sessions(refresh.sessions);
    let mut terminal = TuiTerminal::new()?;
    crate::metrics::mark("tui: terminal ready");
    let mut last_refresh = Instant::now();
    let mut last_draw = Instant::now();
    let mut redraw = true;
    // Refreshes run on a spawned task so a slow daemon/node response never
    // stalls input handling; results come back through this channel.
    let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<Result<SessionRefresh>>(1);
    let mut refresh_in_flight = false;

    'input: loop {
        // Wait briefly for the next event, then drain everything already
        // queued before drawing: a fast typing burst (or paste) is applied
        // as one batch and rendered with a single frame instead of one
        // frame per keystroke.
        let mut events = Vec::new();
        if let Some(event) = read_terminal_event(INPUT_POLL_INTERVAL)? {
            events.push(event);
            drain_pending_events(&mut events)?;
        }
        for event in events {
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    match route_key(&mut app, key, list_node) {
                        AppAction::None => {}
                        AppAction::Quit => break 'input,
                        AppAction::OpenInline => {
                            open_selected_inline(&mut terminal, &mut app, None)?
                        }
                        AppAction::Start(launch) => {
                            start_clone(config, &mut terminal, &mut app, launch).await?
                        }
                        AppAction::Update(update) => update_session(config, &mut app, update).await,
                        AppAction::Stop(target) => stop_session(config, &mut app, target),
                        AppAction::Remove(target) => remove_session(config, &mut app, target),
                    }
                    redraw = true;
                }
                Event::Resize(_, _) => redraw = true,
                _ => {}
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL && !refresh_in_flight {
            refresh_in_flight = true;
            last_refresh = Instant::now();
            let tx = refresh_tx.clone();
            let config = config.clone();
            let query = query.clone();
            let targets = targets.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(fetch_sessions(&config, query, &targets).await)
                    .await;
            });
        }
        while let Ok(result) = refresh_rx.try_recv() {
            refresh_in_flight = false;
            apply_refresh(&mut app, &query, result);
            redraw = true;
        }

        // Effects (attention pulse, fade-ins) need a faster frame cadence;
        // the idle list ticks over at the slower refresh rate.
        let redraw_interval = if app.effects.is_running() {
            ANIMATION_REDRAW_INTERVAL
        } else {
            REDRAW_INTERVAL
        };
        if redraw || last_draw.elapsed() >= redraw_interval {
            terminal.draw(|frame| render(frame, &mut app))?;
            last_draw = Instant::now();
            redraw = false;
        }
    }

    terminal.teardown()?;
    Ok(())
}

/// Apply one refresh cycle's outcome: keep the last-known sessions of
/// unreachable nodes visible, then update the list and the (non-action)
/// status message.
fn apply_refresh(
    app: &mut App,
    query: &crate::protocol::ListQuery,
    result: Result<SessionRefresh>,
) {
    match result {
        Ok(mut refresh) => {
            refresh.sessions.extend(
                app.sessions
                    .iter()
                    .filter(|session| refresh.failed_nodes.contains(&session.node))
                    .cloned(),
            );
            refresh
                .sessions
                .sort_by_key(|session| std::cmp::Reverse(session.created_at));
            refresh.sessions.truncate(query.limit);
            let warning = refresh.warning();
            app.replace_sessions(refresh.sessions);
            app.set_refresh_message(warning);
        }
        Err(error) => app.set_refresh_message(Some(format!("sync lost: {error}"))),
    }
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

fn read_terminal_event(timeout: Duration) -> io::Result<Option<Event>> {
    read_terminal_event_with(timeout, event::poll, event::read)
}

/// Drain input events that are already queued, without waiting. Typing or
/// pasting produces bursts; handling the whole burst before the next draw
/// renders it as a single frame instead of one frame per keystroke.
fn drain_pending_events(events: &mut Vec<Event>) -> io::Result<()> {
    drain_pending_events_with(events, event::poll, event::read)
}

fn drain_pending_events_with<P, R>(
    events: &mut Vec<Event>,
    mut poll: P,
    mut read: R,
) -> io::Result<()>
where
    P: FnMut(Duration) -> io::Result<bool>,
    R: FnMut() -> io::Result<Event>,
{
    while let Some(event) = read_terminal_event_with(Duration::ZERO, &mut poll, &mut read)? {
        events.push(event);
    }
    Ok(())
}

fn read_terminal_event_with(
    timeout: Duration,
    poll: impl FnOnce(Duration) -> io::Result<bool>,
    read: impl FnOnce() -> io::Result<Event>,
) -> io::Result<Option<Event>> {
    match poll(timeout) {
        Ok(false) => Ok(None),
        Ok(true) => match read() {
            Ok(event) => Ok(Some(event)),
            Err(error) if is_transient_terminal_error(&error) => Ok(None),
            Err(error) => Err(error),
        },
        Err(error) if is_transient_terminal_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_transient_terminal_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

async fn fetch_sessions(
    config: &AppConfig,
    query: crate::protocol::ListQuery,
    targets: &[ListTarget],
) -> Result<SessionRefresh> {
    let requests = targets.iter().map(|target| {
        let query = query.clone();
        async move {
            let result = async {
                let inner = RpcRequest::List { query };
                let request = match target.node.as_ref() {
                    Some(node) => RpcRequest::NodeProxy {
                        node: node.clone(),
                        inner: Box::new(inner),
                    },
                    None => inner,
                };
                let response = tokio::time::timeout(
                    REFRESH_TIMEOUT,
                    ipc::send_request_checked(config, request),
                )
                .await
                .map_err(|_| AppError::Protocol("session refresh timed out".to_string()))??;
                match response {
                    RpcResponse::List { mut sessions, .. } => {
                        if let Some(node) = target.node.as_ref() {
                            for session in &mut sessions {
                                session.node = Some(node.clone());
                            }
                        }
                        Ok(sessions)
                    }
                    _ => Err(AppError::Protocol("unexpected response type".to_string())),
                }
            }
            .await;
            (target.node.clone(), result)
        }
    });
    let mut sessions = Vec::new();
    let mut failed_nodes = HashSet::new();
    let mut failures = Vec::new();
    let mut successful_targets = 0;
    for (node, result) in futures_util::future::join_all(requests).await {
        match result {
            Ok(target_sessions) => {
                successful_targets += 1;
                sessions.extend(target_sessions);
            }
            Err(error) => {
                failures.push(format!("{}: {error}", node.as_deref().unwrap_or("local")));
                failed_nodes.insert(node);
            }
        }
    }
    if successful_targets == 0 {
        return Err(AppError::Protocol(failures.join(" · ")));
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at));
    sessions.truncate(query.limit);
    Ok(SessionRefresh {
        sessions,
        failed_nodes,
        failures,
    })
}


/// Fire-and-forget `RpcRequest::Remove { force: true }` to the daemon,
/// matches what `oly rm -f <id>` does from the CLI. The row is also
/// dropped locally so the user sees the dismissal immediately; if the
/// daemon refuses, the next refresh tick surfaces the failure via
/// `apply_refresh` warning.
fn remove_session(config: &AppConfig, app: &mut App, target: SessionTarget) {
    let inner = RpcRequest::Remove {
        id: target.id.clone(),
        force: true,
    };
    let request = match target.node.as_deref() {
        Some(node) => RpcRequest::NodeProxy {
            node: node.to_string(),
            inner: Box::new(inner),
        },
        None => inner,
    };
    let config = config.clone();
    tokio::spawn(async move {
        let _ = ipc::send_request_checked(&config, request).await;
    });
    app.remove_session_payload(&target.id, target.node.as_deref());
    app.set_action_message(Some(format!("removing session {}", target.id)));
}

async fn start_clone(
    config: &AppConfig,
    terminal: &mut TuiTerminal,
    app: &mut App,
    launch: CloneLaunch,
) -> Result<()> {
    match ipc::send_request_checked(config, launch.request()).await {
        Ok(RpcResponse::Start { session_id }) => {
            app.clone_dialog = None;
            app.set_action_message(Some(format!("started new session {session_id}")));
            if launch.attach_after_start {
                open_session_inline(terminal, app, &session_id, launch.node.as_deref(), true)?;
            }
        }
        Ok(_) => {
            set_clone_error(app, "unexpected response type".to_string());
        }
        Err(error) => set_clone_error(app, format!("start failed: {error}")),
    }
    Ok(())
}

async fn update_session(config: &AppConfig, app: &mut App, update: SessionUpdate) {
    let target_id = update.id.clone();
    let target_node = update.node.clone();
    let response = ipc::send_request_checked(config, update.request()).await;
    apply_update_response(app, &target_id, target_node.as_deref(), response);
}

fn apply_update_response(
    app: &mut App,
    target_id: &str,
    target_node: Option<&str>,
    response: Result<RpcResponse>,
) {
    match response {
        Ok(RpcResponse::Session { mut summary }) => {
            summary.node = target_node.map(str::to_string);
            app.update_dialog = None;
            app.apply_updated_summary(summary);
            app.set_action_message(Some(format!("updated session {target_id}")));
        }
        Ok(_) => set_update_error(app, "unexpected response type".to_string()),
        Err(error) => set_update_error(app, format!("update failed: {error}")),
    }
}

fn stop_session(config: &AppConfig, app: &mut App, target: SessionTarget) {
    let request = wrap_node(
        target.node.as_deref(),
        RpcRequest::Stop {
            id: target.id.clone(),
            grace_seconds: STOP_GRACE_SECONDS,
        },
    );
    let config = config.clone();
    tokio::spawn(async move {
        let _ = ipc::send_request_checked(&config, request).await;
    });

    // Optimistically update the row so the user sees immediate feedback;
    // the daemon will confirm on the next refresh cycle.
    if let Some(session) = app
        .sessions
        .iter_mut()
        .find(|s| s.id == target.id && s.node == target.node)
    {
        session.status = "stopped".to_string();
        session.ended_at = Some(Utc::now());
    }

    app.set_action_message(Some(format!("stop signal sent to {}", target.id)));
}

fn wrap_node(node: Option<&str>, inner: RpcRequest) -> RpcRequest {
    match node {
        Some(node) => RpcRequest::NodeProxy {
            node: node.to_string(),
            inner: Box::new(inner),
        },
        None => inner,
    }
}

fn set_clone_error(app: &mut App, error: String) {
    if let Some(dialog) = app.clone_dialog.as_mut() {
        dialog.error = Some(error);
    } else {
        app.set_action_message(Some(error));
    }
}

fn set_update_error(app: &mut App, error: String) {
    if let Some(dialog) = app.update_dialog.as_mut() {
        dialog.error = Some(error);
    } else {
        app.set_action_message(Some(error));
    }
}

#[derive(Default)]
struct App {
    sessions: Vec<SessionSummary>,
    rates: HashMap<String, RateState>,
    selected: usize,
    opened: HashMap<String, OpenedTerminal>,
    next_slot: usize,
    message: Option<String>,
    /// True when `message` carries user-action feedback ("started new
    /// session …", "update failed: …") rather than refresh-cycle sync
    /// status. The 250 ms refresh cycle must not clobber action feedback —
    /// otherwise success/error messages vanish before the user can read
    /// them (and a `--node`-scoped Ctrl+N looks like "nothing happened").
    message_is_action: bool,
    filter: String,
    normalized_filter: String,
    search_text: Vec<String>,
    visible: Vec<usize>,
    status_filter: StatusFilter,
    sort_strategy: SortStrategy,
    clone_dialog: Option<CloneDialog>,
    update_dialog: Option<UpdateDialog>,
    show_node: bool,
    view_mode: ViewMode,
    tree: TreeView,
    /// Shader-like visual effects (tachyonfx) processed on every frame.
    effects: EffectManager<String>,
    /// Timestamp of the previous frame, used to derive the effect tick delta.
    last_frame_at: Option<Instant>,
    /// Row rectangles of sessions waiting for input, keyed by session key.
    /// Each gets its own background pulse effect; the [`RefRect`] is updated
    /// every frame so the pulse follows the row across scrolling, reordering
    /// and resizes. The boolean tracks selection: a selected row pulses
    /// toward a different tint, so a selection change re-registers the effect.
    attention_rows: HashMap<String, (RefRect, bool)>,
    /// The message text the fade-in effect was last registered for, so the
    /// fade replays only when the message actually changes.
    rendered_message: Option<String>,
    /// The dialog the open-fade effect was last registered for.
    rendered_dialog: Option<&'static str>,
}

impl Default for TreeView {
    fn default() -> Self {
        Self::new()
    }
}

/// Top-level presentation of the session list: either the responsive table
/// view (`List`) or a `cwd`-grouped tree (`Tree`). Toggled with Ctrl+G.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ViewMode {
    #[default]
    List,
    Tree,
}

/// One folder node in the tree. Folder rows are emitted by `TreeView`'s
/// `visible` walker; their `direct_sessions` are emitted as siblings at the
/// same effective depth so each session sits visually beneath the deepest
/// folder that still sits above it.
#[derive(Debug)]
struct TreeNode {
    path: PathBuf,
    /// Full cwd for folders, even when the displayed path is abbreviated.
    cwd: Option<String>,
    /// Owner of this branch; keeps identical paths on different nodes distinct.
    node: Option<String>,
    is_node: bool,
    /// Cached `basename` of `path`; empty for the synthetic root.
    name: String,
    /// Sessions whose cwd resolves to this folder.
    direct_sessions: Vec<usize>,
    /// Indices of child folder nodes (in DFS order; not sorted).
    subfolders: Vec<usize>,
}

/// What `TreeView::visible` emits: a single row that is either a folder
/// header or a session leaf.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TreeEntry {
    Folder { node: usize, depth: usize },
    Session { session: usize, depth: usize },
}

/// Folder-and-session tree built from `App::sessions`. The walker produces a
/// flat list of `TreeEntry` rows in DFS order, retaining parent folders
/// and respecting `auto_depth` (folders deeper than this are hidden
/// unless `drilled` contains an ancestor path).
#[derive(Debug)]
struct TreeView {
    nodes: Vec<TreeNode>,
    /// Index of the synthetic root node (path = `PathBuf::new()`).
    root: usize,
    /// Auto-drill horizon. Folders at depth ≤ this are visible by default;
    /// deeper folders are hidden unless an ancestor path is in `drilled`.
    auto_depth: usize,
    /// Paths the user has explicitly drilled into (overrides `auto_depth`).
    drilled: HashSet<(Option<String>, PathBuf)>,
    grouped_nodes: bool,
    /// Flat row list produced by `recompute_visible`.
    visible: Vec<TreeEntry>,
    /// Cursor index into `visible`.
    cursor: usize,
}

impl TreeView {
    fn new() -> Self {
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
const TREE_AUTO_DEPTH: usize = 2;

#[derive(Debug, Eq, PartialEq)]
enum AppAction {
    None,
    Quit,
    OpenInline,
    Start(CloneLaunch),
    Update(SessionUpdate),
    Stop(SessionTarget),
    /// Force-remove a session from the daemon (matches `oly rm -f <id>`).
    /// Always forces the removal so stopped/failed/stale sessions can
    /// still be cleaned out of the list without a separate prompt.
    Remove(SessionTarget),
}

#[derive(Debug, Eq, PartialEq)]
struct SessionTarget {
    id: String,
    node: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct SessionUpdate {
    id: String,
    node: Option<String>,
    title: Option<String>,
    tags: Option<Vec<String>>,
    notifications_enabled: Option<bool>,
}

impl SessionUpdate {
    fn request(&self) -> RpcRequest {
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
struct CloneLaunch {
    title: Option<String>,
    tags: Vec<String>,
    command: String,
    args: Vec<String>,
    cwd: Option<String>,
    node: Option<String>,
    rows: Option<u16>,
    cols: Option<u16>,
    disable_notifications: bool,
    attach_after_start: bool,
}

impl CloneLaunch {
    fn request(&self) -> RpcRequest {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloneField {
    Command,
    Args,
    Cwd,
    Title,
    Tags,
    Node,
    Rows,
    Cols,
    DisableNotifications,
    AttachAfterStart,
}

const CLONE_FIELDS: [CloneField; 10] = [
    CloneField::Command,
    CloneField::Args,
    CloneField::Cwd,
    CloneField::Title,
    CloneField::Tags,
    CloneField::Node,
    CloneField::Rows,
    CloneField::Cols,
    CloneField::DisableNotifications,
    CloneField::AttachAfterStart,
];

#[derive(Debug, Default, Eq, PartialEq)]
struct EditText {
    value: String,
    cursor: usize,
}

impl EditText {
    fn new(value: String) -> Self {
        let cursor = value.chars().count();
        Self { value, cursor }
    }

    fn byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map_or(self.value.len(), |(index, _)| index)
    }

    fn insert(&mut self, character: char) {
        let index = self.byte_index();
        self.value.insert(index, character);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.delete();
    }

    fn delete(&mut self) {
        let start = self.byte_index();
        if start == self.value.len() {
            return;
        }
        let end = self.value[start..]
            .char_indices()
            .nth(1)
            .map_or(self.value.len(), |(offset, _)| start + offset);
        self.value.replace_range(start..end, "");
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.value.chars().count());
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CloneDialog {
    source_id: Option<String>,
    active: usize,
    command: EditText,
    args: EditText,
    cwd: EditText,
    title: EditText,
    tags: EditText,
    node: EditText,
    rows: EditText,
    cols: EditText,
    disable_notifications: bool,
    attach_after_start: bool,
    error: Option<String>,
}

impl CloneDialog {
    fn from_session(session: &SessionSummary, list_node: Option<&str>) -> Self {
        Self {
            source_id: Some(session.id.clone()),
            active: 0,
            command: EditText::new(session.command.clone()),
            args: EditText::new(format_terminal_words(&session.args)),
            cwd: EditText::new(session.cwd.clone().unwrap_or_default()),
            title: EditText::new(session.title.clone().unwrap_or_default()),
            tags: EditText::new(format_terminal_words(&session.tags)),
            node: EditText::new(
                session
                    .node
                    .as_deref()
                    .or(list_node)
                    .unwrap_or_default()
                    .to_string(),
            ),
            rows: EditText::new(
                session
                    .rows
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            cols: EditText::new(
                session
                    .cols
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            disable_notifications: !session.notifications_enabled,
            attach_after_start: false,
            error: None,
        }
    }

    fn blank(list_node: Option<&str>) -> Self {
        Self {
            source_id: None,
            active: 0,
            command: EditText::new(String::new()),
            args: EditText::new(String::new()),
            cwd: EditText::new(String::new()),
            title: EditText::new(String::new()),
            tags: EditText::new(String::new()),
            // Prefill the node the list is currently scoped to so the new
            // session lands where the user is looking; still editable.
            node: EditText::new(list_node.unwrap_or_default().to_string()),
            rows: EditText::new(String::new()),
            cols: EditText::new(String::new()),
            disable_notifications: false,
            attach_after_start: false,
            error: None,
        }
    }

    fn active_field(&self) -> CloneField {
        CLONE_FIELDS[self.active]
    }

    fn next(&mut self) {
        self.active = (self.active + 1) % CLONE_FIELDS.len();
        self.error = None;
    }

    fn previous(&mut self) {
        self.active = (self.active + CLONE_FIELDS.len() - 1) % CLONE_FIELDS.len();
        self.error = None;
    }

    fn active_text_mut(&mut self) -> Option<&mut EditText> {
        match self.active_field() {
            CloneField::Command => Some(&mut self.command),
            CloneField::Args => Some(&mut self.args),
            CloneField::Cwd => Some(&mut self.cwd),
            CloneField::Title => Some(&mut self.title),
            CloneField::Tags => Some(&mut self.tags),
            CloneField::Node => Some(&mut self.node),
            CloneField::Rows => Some(&mut self.rows),
            CloneField::Cols => Some(&mut self.cols),
            CloneField::DisableNotifications | CloneField::AttachAfterStart => None,
        }
    }

    fn toggle_active(&mut self) {
        match self.active_field() {
            CloneField::DisableNotifications => {
                self.disable_notifications = !self.disable_notifications
            }
            CloneField::AttachAfterStart => self.attach_after_start = !self.attach_after_start,
            _ => {}
        }
        self.error = None;
    }

    fn launch(&self) -> std::result::Result<CloneLaunch, String> {
        if self.command.value.trim().is_empty() {
            return Err("command is required".to_string());
        }
        let args = parse_terminal_words("args", &self.args.value)?;
        let tags = parse_terminal_words("tags", &self.tags.value)?;
        let rows = parse_dimension("rows", &self.rows.value)?;
        let cols = parse_dimension("cols", &self.cols.value)?;
        Ok(CloneLaunch {
            title: optional_text(&self.title.value),
            tags,
            command: self.command.value.clone(),
            args,
            cwd: optional_text(&self.cwd.value),
            node: optional_text(&self.node.value),
            rows,
            cols,
            disable_notifications: self.disable_notifications,
            attach_after_start: self.attach_after_start,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateField {
    Title,
    Tags,
    Notifications,
}

const UPDATE_FIELDS: [UpdateField; 3] = [
    UpdateField::Title,
    UpdateField::Tags,
    UpdateField::Notifications,
];

#[derive(Debug)]
struct UpdateDialog {
    target_id: String,
    target_node: Option<String>,
    active: usize,
    title: EditText,
    tags: EditText,
    original_title: Option<String>,
    original_tags: Vec<String>,
    notifications_enabled: bool,
    original_notifications_enabled: bool,
    summary: SessionSummary,
    available: bool,
    error: Option<String>,
}

impl UpdateDialog {
    fn from_session(session: &SessionSummary, list_node: Option<&str>) -> Self {
        Self {
            target_id: session.id.clone(),
            target_node: session
                .node
                .clone()
                .or_else(|| list_node.map(str::to_string)),
            active: 0,
            title: EditText::new(session.title.clone().unwrap_or_default()),
            tags: EditText::new(format_terminal_words(&session.tags)),
            original_title: session.title.clone(),
            original_tags: session.tags.clone(),
            notifications_enabled: session.notifications_enabled,
            original_notifications_enabled: session.notifications_enabled,
            summary: session.clone(),
            available: true,
            error: None,
        }
    }

    fn active_field(&self) -> UpdateField {
        UPDATE_FIELDS[self.active]
    }

    fn next(&mut self) {
        self.active = (self.active + 1) % UPDATE_FIELDS.len();
        self.error = None;
    }

    fn previous(&mut self) {
        self.active = (self.active + UPDATE_FIELDS.len() - 1) % UPDATE_FIELDS.len();
        self.error = None;
    }

    fn active_text_mut(&mut self) -> Option<&mut EditText> {
        match self.active_field() {
            UpdateField::Title => Some(&mut self.title),
            UpdateField::Tags => Some(&mut self.tags),
            UpdateField::Notifications => None,
        }
    }

    fn toggle_active(&mut self) {
        if self.active_field() == UpdateField::Notifications {
            self.notifications_enabled = !self.notifications_enabled;
        }
        self.error = None;
    }

    fn sync_summary(&mut self, summary: Option<&SessionSummary>) {
        let unavailable_message = format!(
            "session {} is no longer available in the current list",
            self.target_id
        );
        match summary {
            Some(summary) => {
                self.summary = summary.clone();
                self.available = true;
                if self.error.as_deref() == Some(unavailable_message.as_str()) {
                    self.error = None;
                }
            }
            None => {
                self.available = false;
                self.error = Some(unavailable_message);
            }
        }
    }

    fn update(&self) -> std::result::Result<SessionUpdate, String> {
        if !self.available {
            return Err(format!(
                "session {} is no longer available in the current list",
                self.target_id
            ));
        }

        let normalized_title = normalize_session_title(Some(self.title.value.clone()));
        if normalized_title
            .as_ref()
            .is_some_and(|title| title.chars().count() > MAX_SESSION_TITLE_LEN)
        {
            return Err(format!(
                "session title is too long (max {MAX_SESSION_TITLE_LEN} characters)"
            ));
        }

        let parsed_tags = parse_terminal_words("tags", &self.tags.value)?;
        let normalized_tags = normalize_session_tags(parsed_tags);
        let title = (normalized_title != self.original_title).then(|| self.title.value.clone());
        let tags = (normalized_tags != self.original_tags).then_some(normalized_tags);
        let notifications_enabled = (self.notifications_enabled
            != self.original_notifications_enabled)
            .then_some(self.notifications_enabled);

        Ok(SessionUpdate {
            id: self.target_id.clone(),
            node: self.target_node.clone(),
            title,
            tags,
            notifications_enabled,
        })
    }
}

fn format_terminal_words(words: &[String]) -> String {
    words
        .iter()
        .map(|word| {
            if word.is_empty() {
                return "\"\"".to_string();
            }
            if word
                .chars()
                .all(|character| !character.is_whitespace() && !matches!(character, '\'' | '"'))
            {
                return word.clone();
            }
            let escaped = word
                .chars()
                .flat_map(|character| {
                    if matches!(character, '"' | '\\') {
                        ['\\', character].into_iter().collect::<Vec<_>>()
                    } else {
                        [character].into_iter().collect()
                    }
                })
                .collect::<String>();
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_terminal_words(label: &str, value: &str) -> std::result::Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = None;
    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    match characters.peek().copied() {
                        Some('"' | '\\') => word.push(characters.next().unwrap()),
                        _ => word.push(character),
                    }
                } else {
                    word.push(character);
                }
            }
            Some(_) => unreachable!(),
            None if character.is_whitespace() => {
                if word_started {
                    words.push(std::mem::take(&mut word));
                    word_started = false;
                }
            }
            None if matches!(character, '\'' | '"') => {
                quote = Some(character);
                word_started = true;
            }
            None if character == '\\' => {
                word_started = true;
                match characters.peek().copied() {
                    Some(next) if next.is_whitespace() || matches!(next, '\'' | '"' | '\\') => {
                        word.push(characters.next().unwrap());
                    }
                    _ => word.push(character),
                }
            }
            None => {
                word.push(character);
                word_started = true;
            }
        }
    }

    if quote.is_some() {
        return Err(format!("{label} has an unclosed quote"));
    }
    if word_started {
        words.push(word);
    }
    Ok(words)
}

fn parse_dimension(label: &str, value: &str) -> std::result::Result<Option<u16>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u16>()
        .ok()
        .filter(|dimension| *dimension > 0)
        .map(Some)
        .ok_or_else(|| format!("{label} must be 1-65535"))
}

fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_string())
}

fn route_key(app: &mut App, key: crossterm::event::KeyEvent, list_node: Option<&str>) -> AppAction {
    if matches!(key.code, KeyCode::Char('c' | 'C')) && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return AppAction::Quit;
    }

    if app.clone_dialog.is_some() {
        return route_clone_dialog_key(app, key);
    }
    if app.update_dialog.is_some() {
        return route_update_dialog_key(app, key);
    }

    match key.code {
        _ if is_new_session_dialog_key(key) => {
            let mut dialog = CloneDialog::blank(list_node);
            if app.view_mode == ViewMode::Tree
                && let Some(TreeEntry::Folder { node, .. }) = app.tree.visible.get(app.tree.cursor)
            {
                let folder = &app.tree.nodes[*node];
                if let Some(cwd) = &folder.cwd {
                    dialog.cwd = EditText::new(cwd.clone());
                }
                // A folder in a node group belongs on that node, even in an
                // unscoped multi-node list. A scoped list keeps its scope.
                if list_node.is_none() {
                    dialog.node = EditText::new(folder.node.clone().unwrap_or_default());
                }
            }
            app.clone_dialog = Some(dialog);
            AppAction::None
        }
        _ if is_clone_dialog_key(key) => {
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to clone".to_string()));
                return AppAction::None;
            };
            app.clone_dialog = Some(CloneDialog::from_session(session, list_node));
            AppAction::None
        }
        _ if is_update_dialog_key(key) => {
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to update".to_string()));
                return AppAction::None;
            };
            app.update_dialog = Some(UpdateDialog::from_session(session, list_node));
            AppAction::None
        }
        KeyCode::Char('g' | 'G') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_view_mode();
            AppAction::None
        }
        KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to stop".to_string()));
                return AppAction::None;
            };
            if !matches!(session.status.as_str(), "created" | "running") {
                app.set_action_message(Some(format!(
                    "{} cannot be stopped while {}",
                    session.id, session.status
                )));
                return AppAction::None;
            }
            AppAction::Stop(SessionTarget {
                id: session.id.clone(),
                node: session
                    .node
                    .clone()
                    .or_else(|| list_node.map(str::to_string)),
            })
        }
        KeyCode::Char('r' | 'R') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+R removes the focused session from the daemon
            // (`oly rm -f <id>`). The RPC is fire-and-forget on a spawned
            // task so the UI does not block; a refusal surfaces on the
            // next refresh cycle via `apply_refresh`'s standard
            // "sync lost" warning.
            let Some(session) = app.focused_session() else {
                app.set_action_message(Some("no session in focus to remove".to_string()));
                return AppAction::None;
            };
            let target = SessionTarget {
                id: session.id.clone(),
                node: session
                    .node
                    .clone()
                    .or_else(|| list_node.map(str::to_string)),
            };
            app.remove_session(&target.id, target.node.as_deref());
            AppAction::Remove(target)
        }
        KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_status_filter();
            AppAction::None
        }
        KeyCode::Char('o' | 'O') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.cycle_sort_strategy();
            AppAction::None
        }
        KeyCode::Esc => {
            app.clear_filter();
            AppAction::None
        }
        KeyCode::Backspace => {
            app.pop_filter();
            AppAction::None
        }
        KeyCode::Up => {
            match app.view_mode {
                ViewMode::Tree => app.navigate_tree(-1),
                ViewMode::List => app.previous(),
            }
            AppAction::None
        }
        KeyCode::Down => {
            match app.view_mode {
                ViewMode::Tree => app.navigate_tree(1),
                ViewMode::List => app.next(),
            }
            AppAction::None
        }
        KeyCode::Home => {
            match app.view_mode {
                ViewMode::Tree => app.tree_home(),
                ViewMode::List => app.first(),
            }
            AppAction::None
        }
        KeyCode::End => {
            match app.view_mode {
                ViewMode::Tree => app.tree_last(),
                ViewMode::List => app.last(),
            }
            AppAction::None
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_selected_terminal(list_node);
            AppAction::None
        }
        KeyCode::Enter => match app.view_mode {
            ViewMode::Tree => app.tree_enter(),
            ViewMode::List => AppAction::OpenInline,
        },
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.push_filter(character);
            AppAction::None
        }
        _ => AppAction::None,
    }
}

fn is_clone_dialog_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{4}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('d' | 'D')))
}

fn is_new_session_dialog_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{e}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n' | 'N')))
}

fn is_update_dialog_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{15}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('u' | 'U')))
}

fn route_clone_dialog_key(app: &mut App, key: crossterm::event::KeyEvent) -> AppAction {
    if key.code == KeyCode::Esc {
        let message = if app
            .clone_dialog
            .as_ref()
            .is_some_and(|dialog| dialog.source_id.is_some())
        {
            "duplicate cancelled"
        } else {
            "new session cancelled"
        };
        app.clone_dialog = None;
        app.set_action_message(Some(message.to_string()));
        return AppAction::None;
    }

    let dialog = app.clone_dialog.as_mut().expect("dialog checked above");
    match key.code {
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::CONTROL) => dialog.previous(),
        KeyCode::Tab => dialog.next(),
        KeyCode::BackTab => dialog.previous(),
        KeyCode::Enter => match dialog.launch() {
            Ok(launch) => return AppAction::Start(launch),
            Err(error) => dialog.error = Some(error),
        },
        KeyCode::Char(' ') if dialog.active_text_mut().is_none() => dialog.toggle_active(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(field) = dialog.active_text_mut() {
                field.insert(character);
                dialog.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Some(field) = dialog.active_text_mut() {
                field.backspace();
                dialog.error = None;
            }
        }
        KeyCode::Delete => {
            if let Some(field) = dialog.active_text_mut() {
                field.delete();
                dialog.error = None;
            }
        }
        KeyCode::Left => {
            if let Some(field) = dialog.active_text_mut() {
                field.left();
            }
        }
        KeyCode::Right => {
            if let Some(field) = dialog.active_text_mut() {
                field.right();
            }
        }
        KeyCode::Home => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = 0;
            }
        }
        KeyCode::End => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = field.value.chars().count();
            }
        }
        _ => {}
    }
    AppAction::None
}

fn route_update_dialog_key(app: &mut App, key: crossterm::event::KeyEvent) -> AppAction {
    if key.code == KeyCode::Esc {
        app.update_dialog = None;
        app.set_action_message(Some("update cancelled".to_string()));
        return AppAction::None;
    }

    let dialog = app.update_dialog.as_mut().expect("dialog checked above");
    match key.code {
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::CONTROL) => dialog.previous(),
        KeyCode::Tab => dialog.next(),
        KeyCode::BackTab => dialog.previous(),
        KeyCode::Enter => match dialog.update() {
            Ok(update) => return AppAction::Update(update),
            Err(error) => dialog.error = Some(error),
        },
        KeyCode::Char(' ') if dialog.active_text_mut().is_none() => dialog.toggle_active(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(field) = dialog.active_text_mut() {
                field.insert(character);
                dialog.error = None;
            }
        }
        KeyCode::Backspace => {
            if let Some(field) = dialog.active_text_mut() {
                field.backspace();
                dialog.error = None;
            }
        }
        KeyCode::Delete => {
            if let Some(field) = dialog.active_text_mut() {
                field.delete();
                dialog.error = None;
            }
        }
        KeyCode::Left => {
            if let Some(field) = dialog.active_text_mut() {
                field.left();
            }
        }
        KeyCode::Right => {
            if let Some(field) = dialog.active_text_mut() {
                field.right();
            }
        }
        KeyCode::Home => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = 0;
            }
        }
        KeyCode::End => {
            if let Some(field) = dialog.active_text_mut() {
                field.cursor = field.value.chars().count();
            }
        }
        _ => {}
    }
    AppAction::None
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum StatusFilter {
    #[default]
    All,
    Active,
    Inactive,
}

fn is_active_status(status: &str) -> bool {
    matches!(status, "created" | "running" | "stopping")
}

/// A session is "active" for sorting while it is alive (created/running/
/// stopping) or waiting for input, so attention-needed rows never sink below
/// finished ones.
fn session_is_active(session: &SessionSummary) -> bool {
    is_active_status(&session.status) || session.input_needed
}

/// Row ordering strategies for the session list, cycled with Ctrl+O.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SortStrategy {
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
    fn label(self) -> &'static str {
        match self {
            Self::ActiveFirst => "active first",
            Self::CreatedDesc => "newest",
            Self::CreatedAsc => "oldest",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::ActiveFirst => Self::CreatedDesc,
            Self::CreatedDesc => Self::CreatedAsc,
            Self::CreatedAsc => Self::ActiveFirst,
        }
    }
}

fn sort_sessions(sessions: &mut [SessionSummary], strategy: SortStrategy) {
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
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Inactive => "inactive",
        }
    }
}

#[derive(Debug)]
struct OpenedTerminal {
    marker: PathBuf,
    launched_at: Instant,
}

#[derive(Debug)]
struct RateState {
    total_bytes: u64,
    output_epoch: Option<DateTime<Utc>>,
    sampled_at: Instant,
    previous_rate: f64,
    rate: f64,
    history: VecDeque<f64>,
}

impl RateState {
    fn new(session: &SessionSummary, now: Instant) -> Self {
        Self {
            total_bytes: session.last_total_bytes,
            output_epoch: session.last_output_epoch,
            sampled_at: now,
            previous_rate: 0.0,
            rate: 0.0,
            history: VecDeque::from([0.0]),
        }
    }

    fn sample(&mut self, session: &SessionSummary, now: Instant) {
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

    fn display_rate(&self, now: Instant) -> f64 {
        let progress = now.saturating_duration_since(self.sampled_at).as_secs_f64()
            / REFRESH_INTERVAL.as_secs_f64();
        let eased = progress.clamp(0.0, 1.0);
        self.previous_rate + (self.rate - self.previous_rate) * eased
    }
}

fn session_search_text(session: &SessionSummary) -> String {
    let mut text = format!("{}\n{}", session.id, session.command);
    if let Some(node) = &session.node {
        text.push('\n');
        text.push_str(node);
    }
    if let Some(title) = &session.title {
        text.push('\n');
        text.push_str(title);
    }
    text.to_lowercase()
}

fn session_key(session: &SessionSummary) -> String {
    session.node.as_ref().map_or_else(
        || session.id.clone(),
        |node| format!("{node}\0{}", session.id),
    )
}

/// Display label used to sort sessions inside a folder. Mirrors what the
/// tree-view renders: command + args + title. Title is appended last so
/// sessions sharing a command line still bubble up in their own alpha
/// order.
fn session_sort_label(session: &SessionSummary) -> String {
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

/// Shared parent hidden by the tree. Keep the common cwd itself as a
/// visible folder, so every session's directory can be read from the tree
/// even if some sessions start in that shared directory.
fn common_path_prefix(paths: &[PathBuf]) -> PathBuf {
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
fn ensure_tree_path(
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

fn append_tree_node(
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
fn walk_tree_branch(
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
    fn is_visible(
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
fn is_in_drill_subtree(
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

impl App {
    /// Refresh-cycle sync status (warnings / "sync lost"): replaces the
    /// current message only when no action feedback is showing, so a fresh
    /// warning never silently discards feedback the user hasn't read yet.
    fn set_refresh_message(&mut self, message: Option<String>) {
        if !self.message_is_action {
            self.message = message;
        }
    }

    /// User-action feedback: always shown, and stays until the next action
    /// or an explicit clear; the refresh cycle leaves it alone.
    fn set_action_message(&mut self, message: Option<String>) {
        self.message_is_action = message.is_some();
        self.message = message;
    }

    fn replace_sessions(&mut self, mut sessions: Vec<SessionSummary>) {
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

    fn sync_update_dialog(&mut self) {
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

    /// Drop a session row from the local model so the user sees the
    /// removal immediately; the next refresh tick will confirm with the
    /// daemon. Used by Ctrl+R (`oly rm -f <id>`). The cursor is repaired
    /// when it lands on the now-missing index, and per-session state
    /// (rates, attention rows) is cleared.
    fn remove_session_payload(&mut self, id: &str, node: Option<&str>) {
        let attention_key = format!("{id:?}:{node:?}");
        self.attention_rows.remove(&attention_key);
        self.rates.remove(id);
        let Some(position) = self
            .sessions
            .iter()
            .position(|session| session.id == id && session.node.as_deref() == node)
        else {
            return;
        };
        self.sessions.remove(position);
        self.rebuild_visible();
        if self.selected >= self.visible.len() && !self.visible.is_empty() {
            self.selected = self.visible.len() - 1;
        }
    }

    /// Hook used by the Ctrl+R route-key handler so the action feedback
    /// ("removing session …") and the optimistic drop both happen on the
    /// same call site. Splitting this from `remove_session_payload`
    /// lets refresh::remove_session reuse the same drop logic.
    fn remove_session(&mut self, id: &str, node: Option<&str>) {
        self.remove_session_payload(id, node);
    }

    fn apply_updated_summary(&mut self, summary: SessionSummary) {
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

    fn rebuild_visible(&mut self) {
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

    fn selected_session(&self) -> Option<&SessionSummary> {
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
    fn focused_session(&self) -> Option<&SessionSummary> {
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

    fn select_visible(&mut self, offset: isize) {
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

    fn previous(&mut self) {
        self.select_visible(-1);
    }

    fn next(&mut self) {
        self.select_visible(1);
    }

    /// Rebuild the folder tree from `self.sessions` and recompute its visible
    /// flat row list. Tree state and cursor are preserved: the cursor snaps to
    /// the previously selected session if it survives the rebuild, otherwise
    /// to the first row.
    fn rebuild_tree(&mut self) {
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
                ref drilled,
                ..
            } = self.tree;
            let mut out: Vec<TreeEntry> = Vec::new();
            // Include cwd-less sessions attached directly to the synthetic
            // root as well as node and folder branches.
            walk_tree_branch(
                nodes,
                root,
                0,
                auto_depth + usize::from(self.tree.grouped_nodes),
                drilled,
                &mut out,
            );
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
    fn build_tree_nodes(&mut self) {
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
                .filter_map(|(_, session)| session.cwd.as_deref().map(PathBuf::from))
                .filter(|path| !path.as_os_str().is_empty())
                .collect();
            let common = common_path_prefix(&cwds);
            for (index, session) in group_sessions {
                let cwd = session
                    .cwd
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_default();
                let effective = if cwd.as_os_str().is_empty() {
                    PathBuf::new()
                } else {
                    cwd.strip_prefix(&common)
                        .map_or_else(|_| cwd.clone(), |stripped| stripped.to_path_buf())
                };
                let leaf_idx = ensure_tree_path(
                    &mut self.tree,
                    group_root,
                    &effective,
                    session.cwd.as_deref().unwrap_or_default(),
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
    fn prune_empty_tree_nodes(&mut self) {
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
    fn sort_tree_nodes(&mut self) {
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

    fn toggle_tree_drill(&mut self) {
        let Some(entry) = self.tree.visible.get(self.tree.cursor).copied() else {
            return;
        };
        let TreeEntry::Folder { node, depth } = entry else {
            return;
        };
        // Drill toggles contribute nothing for folders that are already
        // visible by default; toggle only matters for folders whose
        // children sit below the auto-depth horizon.
        if depth < self.tree.auto_depth + usize::from(self.tree.grouped_nodes) {
            return;
        }
        let key = (
            self.tree.nodes[node].node.clone(),
            self.tree.nodes[node].path.clone(),
        );
        if !self.tree.drilled.insert(key.clone()) {
            self.tree.drilled.remove(&key);
        }
        self.rebuild_tree();
    }

    /// Switch between list and tree presentations. The flat-list `visible`
    /// continues to reflect current sessions so a Ctrl+G <-> Ctrl+G round
    /// trip leaves selection unchanged.
    fn toggle_view_mode(&mut self) {
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
        // When entering tree mode from list mode, drill into the path
        // of the currently focused session so it remains visible past
        // the auto-depth horizon. Round-trips back to list keep the
        // drilled set so the same place can be revisited next toggle.
        if next_view == ViewMode::Tree && self.view_mode == ViewMode::List {
            self.drill_to_focused_list_selection();
        }
        self.view_mode = next_view;
        self.rebuild_tree();
        self.focus_tree_on_focused_session();
        // Track and announce the new mode so users don't get disoriented.
        self.set_action_message(Some(match self.view_mode {
            ViewMode::List => "list view · Ctrl+G tree".to_string(),
            ViewMode::Tree => "tree view · Enter to drill · Ctrl+G list".to_string(),
        }));
    }

    /// Drill every ancestor of the list selection's cwd so the focused
    /// session stays visible past the auto-depth horizon when switching
    /// into tree mode. Sessions without a cwd sit on the synthetic root
    /// and need no drilling.
    fn drill_to_focused_list_selection(&mut self) {
        let Some(index) = self.visible.get(self.selected).copied() else {
            return;
        };
        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let Some(cwd) = session.cwd.as_deref() else {
            return;
        };
        // Mirror the pipeline stripping: drop the shared ancestor so
        // drilled paths match what the walker emits under the (optional)
        // per-node branch.
        let common = common_path_prefix(
            &self
                .sessions
                .iter()
                .filter_map(|session| session.cwd.as_deref().map(PathBuf::from))
                .filter(|path| !path.as_os_str().is_empty())
                .collect::<Vec<_>>(),
        );
        let stripped = match Path::new(cwd).strip_prefix(&common) {
            Ok(path) => path.to_path_buf(),
            Err(_) => PathBuf::from(cwd),
        };
        if stripped.as_os_str().is_empty() {
            return;
        }
        let mut accumulated = PathBuf::new();
        for component in stripped.components() {
            if matches!(component, std::path::Component::RootDir) {
                continue;
            }
            accumulated.push(component);
            self.tree
                .drilled
                .insert((session.node.clone(), accumulated.clone()));
        }
    }

    /// Move the tree cursor to the row belonging to the focused session,
    /// if any. Falls back to the existing cursor position when the focused
    /// session is missing or hidden.
    fn focus_tree_on_focused_session(&mut self) {
        if self.view_mode != ViewMode::Tree {
            return;
        }
        let Some(index) = self.visible.get(self.selected).copied() else {
            return;
        };
        if let Some(position) = self.tree.visible.iter().position(|entry| {
            matches!(entry, TreeEntry::Session { session, .. } if *session == index)
        }) {
            self.tree.cursor = position;
        }
    }

    /// Move the tree cursor by `offset`. The cursor wraps around the visible
    /// row range (vim-style) so repeated Up/Down in tree mode feels
    /// continuous and matches what the flat-list cursor does in list mode.
    /// No-op in list mode so a stray call from `route_key` (which also drives
    /// the flat-list cursor) is harmless.
    fn navigate_tree(&mut self, offset: isize) {
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

    fn tree_home(&mut self) {
        if self.view_mode == ViewMode::Tree {
            self.tree.cursor = 0;
        }
    }

    fn tree_last(&mut self) {
        if self.view_mode == ViewMode::Tree {
            self.tree.cursor = self.tree.visible.len().saturating_sub(1);
        }
    }

    /// Enter pressed in tree mode: drill on a folder at depth
    /// `>= auto_depth`, descend into a folder whose children are already
    /// auto-visible, or open a session inline. Returns the action the
    /// caller should fan out to (only `AppAction::OpenInline` is ever
    /// produced; drilling and descending are internal cursor moves).
    fn tree_enter(&mut self) -> AppAction {
        match self.tree.visible.get(self.tree.cursor).copied() {
            Some(TreeEntry::Folder { depth, .. }) => {
                if depth >= self.tree.auto_depth + usize::from(self.tree.grouped_nodes) {
                    // Past the auto-depth horizon the children are
                    // hidden; pressing Enter is the only way to expose
                    // them, so drilling is the right action.
                    self.toggle_tree_drill();
                } else {
                    // The folder's children are already on screen. Treat
                    // Enter as "descend into this folder": move the
                    // cursor one row down so the user lands on the
                    // first item inside. Without this, Enter on a
                    // shallow folder felt like a dead key.
                    let next =
                        (self.tree.cursor + 1).min(self.tree.visible.len().saturating_sub(1));
                    self.tree.cursor = next;
                }
                AppAction::None
            }
            Some(TreeEntry::Session { session, .. }) => {
                // Mirror the focused session into `self.selected` so any
                // follow-on helper that still reads `self.selected`
                // (e.g. terminal open) lands on the right row when the
                // action handler runs.
                self.selected = session;
                AppAction::OpenInline
            }
            None => AppAction::None,
        }
    }

    fn first(&mut self) {
        if let Some(index) = self.visible.first() {
            self.selected = *index;
        }
    }

    fn last(&mut self) {
        if let Some(index) = self.visible.last() {
            self.selected = *index;
        }
    }

    fn update_text_filter(&mut self) {
        self.normalized_filter = self.filter.to_lowercase();
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

    fn push_filter(&mut self, character: char) {
        self.filter.push(character);
        self.update_text_filter();
    }

    fn pop_filter(&mut self) {
        self.filter.pop();
        self.update_text_filter();
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.update_text_filter();
    }

    fn cycle_sort_strategy(&mut self) {
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

    fn toggle_status_filter(&mut self) {
        self.status_filter = match self.status_filter {
            StatusFilter::All => StatusFilter::Active,
            StatusFilter::Active => StatusFilter::Inactive,
            StatusFilter::Inactive => StatusFilter::All,
        };
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

    fn open_selected_terminal(&mut self, node: Option<&str>) {
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

fn open_selected_inline(
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

fn open_session_inline(
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

pub(super) mod terminal;
pub(super) use terminal::{TuiTerminal, enter_list_title, restore_tui_state, wait_for_ctrl_d, write_list_title};
#[derive(Clone, Copy)]
enum LayoutMode {
    /// Below the small-row breakpoint; rows show just status, id, command,
    /// state, age, and a compact rate column.
    Narrow,
    /// Above the small-row breakpoint but below the wide layout threshold:
    /// rows include a normal-width sparkline alongside the rate.
    Medium,
    /// At or above the wide layout threshold; rows gain id, pid, output,
    /// and command columns.
    Wide,
}



fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let mode = if area.width >= 100 {
        LayoutMode::Wide
    } else if area.width >= 62 {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    };
    // The footer is a single compact line so the session table gets every
    // other row of the terminal.
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    let now = Instant::now();
    let running = app
        .sessions
        .iter()
        .filter(|item| item.status == "running")
        .count();
    let throughput = app
        .sessions
        .iter()
        .filter_map(|session| app.rates.get(&session_key(session)))
        .map(|rate| rate.display_rate(now))
        .sum::<f64>();
    let header = Layout::horizontal([Constraint::Min(0), Constraint::Length(18)]).split(chunks[0]);
    let title = Line::from(vec![
        Span::styled(
            match mode {
                LayoutMode::Narrow => " oly ",
                _ => " ◉ OPEN RELAY ",
            },
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " {} sessions · {} live · {} · sort:{}",
                app.sessions.len(),
                running,
                app.status_filter.label(),
                app.sort_strategy.label()
            ),
            Style::default().fg(Color::Gray),
        ),
    ]);
    let header_block = || {
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray))
    };
    frame.render_widget(Paragraph::new(title).block(header_block()), header[0]);
    let animation_age = app
        .rates
        .values()
        .map(|rate| now.saturating_duration_since(rate.sampled_at))
        .min()
        .unwrap_or_default();
    let throughput_header =
        Layout::horizontal([Constraint::Length(7), Constraint::Min(0)]).split(header[1]);
    frame.render_widget(
        Sparkline::default()
            .data(aggregate_sparkline_data(&app.rates, 6))
            .style(Style::default().fg(rate_color(throughput, animation_age)))
            .block(header_block()),
        throughput_header[0],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{:>8}/s", format_bytes(throughput)),
            Style::default().fg(if throughput > 0.0 {
                Color::Cyan
            } else {
                Color::DarkGray
            }),
        ))
        .alignment(Alignment::Right)
        .block(header_block()),
        throughput_header[1],
    );

    let visible = &app.visible;
    let mut attention_row_rects: Vec<(String, Rect, bool)> = Vec::new();
    if app.view_mode == ViewMode::Tree {
        // Tree mode owns its own empty-state, viewport, and per-row geometry.
        // Returning early keeps the linear list path below untouched.
        if app.sessions.is_empty() {
            frame.render_widget(
                Paragraph::new("\n  no signals detected\n  start one: oly start -d <cmd>")
                    .style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        } else {
            let (viewport_len, viewport_start, tree_rows) =
                render_tree(frame, chunks[1], app, mode, now, &mut attention_row_rects);
            // `viewport_len` and `viewport_start` are referenced via shadowed
            // locals in the list-mode path; tree mode only uses them to drive
            // the scrollbar (currently disabled for the tree view, but the
            // hooks stay so adding it later is one line change).
            let _ = (viewport_len, viewport_start, tree_rows);
        }
    } else if app.sessions.is_empty() || visible.is_empty() {
        let empty = if app.sessions.is_empty() {
            "\n  no signals detected\n  start one: oly start -d <cmd>".to_string()
        } else {
            format!("\n  no sessions match ‘{}’", app.filter)
        };
        frame.render_widget(
            Paragraph::new(empty).style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let selected_position = visible
            .iter()
            .position(|index| *index == app.selected)
            .unwrap_or(0);
        let viewport_len = chunks[1].height.saturating_sub(1).max(1) as usize;
        let viewport_start = selected_position
            .saturating_sub(viewport_len / 2)
            .min(visible.len().saturating_sub(viewport_len));
        let rows = visible.iter().enumerate().filter_map(|(position, index)| {
            app.sessions.get(*index).map(|session| {
                session_row(
                    session,
                    app.rates.get(&session_key(session)),
                    mode,
                    now,
                    app.show_node,
                    position == selected_position,
                )
            })
        });
        // Selection styling lives in `session_row` itself: ratatui applies
        // `row_highlight_style` *after* the cells render, which would
        // override the semantic status colours (attention/failure/running).
        let table = Table::new(rows, session_table_widths(mode, app.show_node))
            .header(session_table_header(mode, app.show_node))
            .column_spacing(1)
            .highlight_symbol("▸ ");
        let mut state = TableState::new()
            .with_offset(viewport_start)
            .with_selected(Some(selected_position));
        let show_scrollbar = visible.len() > viewport_len;
        let table_area = if show_scrollbar {
            Rect {
                width: chunks[1].width.saturating_sub(1),
                ..chunks[1]
            }
        } else {
            chunks[1]
        };
        frame.render_stateful_widget(table, table_area, &mut state);

        // Record where each waiting session's row actually landed (one line
        // below the header, offset by the scroll position) so the attention
        // pulse can be scoped to exactly those rows. A session with
        // notifications disabled never pulses, even while it waits for
        // input: the animation is an attention signal, and the user opted
        // out of attention signals for that session.
        for (position, index) in visible.iter().enumerate() {
            let Some(session) = app.sessions.get(*index) else {
                continue;
            };
            if !session.input_needed || !session.notifications_enabled {
                continue;
            }
            let Some(row_offset) = position.checked_sub(viewport_start) else {
                continue;
            };
            if row_offset >= viewport_len {
                continue;
            }
            attention_row_rects.push((
                session_key(session),
                Rect {
                    x: table_area.x,
                    y: table_area.y + 1 + row_offset as u16,
                    width: 3,
                    height: 1,
                },
                position == selected_position,
            ));
        }

        if show_scrollbar {
            let mut scrollbar_state = ScrollbarState::new(visible.len())
                .position(selected_position)
                .viewport_content_length(viewport_len);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("┃");
            frame.render_stateful_widget(scrollbar, chunks[1], &mut scrollbar_state);
        }
    }

    // Footer: compact key hints stay pinned on the left; the latest status
    // message (if any) renders on the right instead of replacing them, so a
    // warning never hides the help. One line, no border — the table keeps
    // the space.
    // Each hint line is sized to fit the *smallest* width of its layout
    // mode, so nothing is ever truncated.
    let help = if app.filter.is_empty() {
        match mode {
            LayoutMode::Narrow => {
                " filter ^N new ^D dup ^K stop ^O sort ⏎ open ^C quit".to_string()
            }
            LayoutMode::Medium => {
                " filter · ^N new · ^D dup · ^K stop · ^O sort · ⏎ open · ^C quit".to_string()
            }
            LayoutMode::Wide => {
                " filter · ^N new · ^D duplicate · ^U update · ^K stop · ^S status · ^O sort · ⏎ open · ^⏎ window · ^C quit"
                    .to_string()
            }
        }
    } else {
        format!(
            " filter: {}_ · status: {} ^S · ⌫ edit · esc clear",
            app.filter,
            app.status_filter.label()
        )
    };
    let message_width = app
        .message
        .as_deref()
        .map(|message| unicode_width::UnicodeWidthStr::width(message) as u16 + 1)
        .unwrap_or(0);
    let footer = Layout::horizontal([Constraint::Min(1), Constraint::Length(message_width)])
        .split(chunks[2]);
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        footer[0],
    );
    if let Some(message) = app.message.as_deref() {
        frame.render_widget(
            Paragraph::new(message)
                .style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
                .alignment(Alignment::Right),
            footer[1],
        );
    }
    let message_area = (message_width > 0).then_some(footer[1]);

    if let Some(dialog) = app.clone_dialog.as_ref() {
        render_clone_dialog(frame, dialog);
    } else if let Some(dialog) = app.update_dialog.as_ref() {
        render_update_dialog(frame, dialog);
    }

    render_effects(frame, app, message_area, attention_row_rects);
}

/// Tick and (un)register the list's shader-like effects.
///
/// All effects preserve cell symbols and only interpolate colours, so a
/// frame rendered at effect-time zero is pixel-identical to one rendered
/// without effects.
fn attention_pulse_key(session_key: &str) -> String {
    format!("attention-pulse:{session_key}")
}

fn render_effects(
    frame: &mut Frame<'_>,
    app: &mut App,
    message_area: Option<Rect>,
    attention_rows: Vec<(String, Rect, bool)>,
) {
    // Pulse the background of every row whose session waits for input. The
    // filter is the row's RefRect alone (updated every frame, so the pulse
    // follows the row across scrolling, reordering and resizes), and only the
    // background is animated — every foreground colour (status semantics,
    // dimming, selection) keeps working on top of the dark amber tint.
    // Nothing outside those rows is ever touched.
    let stale: Vec<String> = app
        .attention_rows
        .keys()
        .filter(|key| !attention_rows.iter().any(|(active, _, _)| active == *key))
        .cloned()
        .collect();
    for key in stale {
        app.effects.cancel_unique_effect(attention_pulse_key(&key));
        app.attention_rows.remove(&key);
    }
    for (key, rect, selected) in attention_rows {
        let existing = app
            .attention_rows
            .get(&key)
            .map(|(row, was_selected)| (row.clone(), *was_selected));
        if let Some((row, was_selected)) = existing {
            row.set(rect);
            if was_selected == selected {
                continue;
            }
            // The selection state changed the pulse target: swap the effect.
            app.effects.cancel_unique_effect(attention_pulse_key(&key));
            app.attention_rows.remove(&key);
        }
        // The filter must be attached to the inner effect: the repeating /
        // ping-pong containers do not apply their own filter to the wrapped
        // effect's cells.
        //
        // tachyonfx has no `fade_to_bg`, so the pulse is a small custom
        // shader that lerps only the background colour of the row's cells
        // (selected by the RefRect filter) towards the attention tint.
        let row = RefRect::new(rect);
        let target = if selected {
            ATTENTION_PULSE_BG_SELECTED
        } else {
            ATTENTION_PULSE_BG
        };
        let pulse = fx::effect_fn(
            (),
            EffectTimer::from_ms(800, Interpolation::SineInOut),
            move |_, context: fx::ShaderFnContext<'_>, cells: CellIterator<'_>| {
                let alpha = context.alpha();
                cells.for_each_cell(|_, cell| {
                    let bg = ColorSpace::Rgb.lerp(&cell.bg, &target, alpha);
                    cell.set_bg(bg);
                });
            },
        )
        .with_filter(CellFilter::RefArea(row.clone()));
        app.effects.add_unique_effect(
            attention_pulse_key(&key),
            fx::repeating(fx::ping_pong(pulse)),
        );
        app.attention_rows.insert(key, (row, selected));
    }

    // Fade in a freshly posted status message.
    if app.rendered_message != app.message {
        app.rendered_message = app.message.clone();
        if let (Some(_), Some(area)) = (app.message.as_ref(), message_area) {
            app.effects.add_unique_effect(
                "message-fade",
                fx::fade_from_fg(
                    Color::DarkGray,
                    EffectTimer::from_ms(400, Interpolation::QuadOut),
                )
                .with_area(area),
            );
        }
    }

    // Fade a clone/update dialog in when it opens.
    let dialog = if app.clone_dialog.is_some() {
        Some(("clone-fade", centered_rect(frame.area(), 96, 14)))
    } else if app.update_dialog.is_some() {
        Some(("update-fade", centered_rect(frame.area(), 110, 19)))
    } else {
        None
    };
    match dialog {
        Some((key, area)) if app.rendered_dialog != Some(key) => {
            app.rendered_dialog = Some(key);
            app.effects.add_unique_effect(
                key,
                fx::fade_from(
                    Color::Reset,
                    Color::Reset,
                    EffectTimer::from_ms(240, Interpolation::QuadOut),
                )
                .with_area(area),
            );
        }
        None => app.rendered_dialog = None,
        _ => {}
    }

    let elapsed = app
        .last_frame_at
        .map(|instant| instant.elapsed())
        .unwrap_or_default();
    app.last_frame_at = Some(Instant::now());
    let fx_elapsed = FxDuration::from_millis(elapsed.as_millis().min(u32::MAX as u128) as u32);
    let area = frame.area();
    app.effects
        .process_effects(fx_elapsed, frame.buffer_mut(), area);
}

fn render_clone_dialog(frame: &mut Frame<'_>, dialog: &CloneDialog) {
    let area = centered_rect(frame.area(), 96, 19);
    let cursor_visible = clone_cursor_visible();
    let field_line = |field| clone_field_line(dialog, field, area.width, cursor_visible);
    let mut lines = vec![section_header("PROCESS")];
    lines.extend(
        [CloneField::Command, CloneField::Args, CloneField::Cwd]
            .into_iter()
            .map(field_line),
    );
    lines.push(Line::default());
    lines.push(section_header("METADATA"));
    lines.extend(
        [CloneField::Title, CloneField::Tags, CloneField::Node]
            .into_iter()
            .map(field_line),
    );
    lines.push(Line::default());
    lines.push(section_header("OPTIONS"));
    lines.extend(
        [
            CloneField::Rows,
            CloneField::Cols,
            CloneField::DisableNotifications,
            CloneField::AttachAfterStart,
        ]
        .into_iter()
        .map(field_line),
    );
    lines.push(tip_separator(area.width));
    lines.push(dialog_footer(dialog.error.as_deref(), CLONE_DIALOG_HELP));
    render_dialog(
        frame,
        area,
        dialog.source_id.as_ref().map_or_else(
            || " ✚ New Session ".to_string(),
            |source_id| format!(" ⧉ Duplicate {source_id} "),
        ),
        Color::Cyan,
        lines,
    );
}

fn render_update_dialog(frame: &mut Frame<'_>, dialog: &UpdateDialog) {
    let area = centered_rect(frame.area(), 110, 22);
    let cursor_visible = clone_cursor_visible();
    let mut lines = vec![section_header("SESSION")];
    lines.extend(
        UPDATE_FIELDS
            .into_iter()
            .map(|field| update_field_line(dialog, field, area.width, cursor_visible)),
    );
    lines.push(Line::default());
    lines.push(section_header("DETAILS"));
    lines.extend(
        update_read_only_values(&dialog.summary)
            .into_iter()
            .map(|(label, value)| update_read_only_line(label, &value, area.width)),
    );
    lines.push(tip_separator(area.width));
    lines.push(dialog_footer(dialog.error.as_deref(), UPDATE_DIALOG_HELP));
    render_dialog(
        frame,
        area,
        format!(" ✎ Update {} ", dialog.target_id),
        if dialog.available {
            Color::Cyan
        } else {
            Color::Red
        },
        lines,
    );
}

fn tip_separator(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "\u{2500}".repeat(width.saturating_sub(2) as usize),
        Style::default().fg(Color::DarkGray),
    ))
}

fn dialog_footer<'a>(error: Option<&'a str>, help: &'static str) -> Line<'a> {
    let errored = error.is_some();
    Line::from(Span::styled(
        error.unwrap_or(help),
        Style::default()
            .fg(if errored { Color::Red } else { Color::DarkGray })
            .add_modifier(if errored {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    ))
}

fn render_dialog<'a>(
    frame: &mut Frame<'_>,
    area: Rect,
    title: String,
    border_color: Color,
    lines: Vec<Line<'a>>,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .shadow(Shadow::dark_shade().style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// A dim uppercase section header grouping rows inside a dialog.
fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
}

/// The width available to a field value inside a dialog of `width` cells.
fn dialog_value_width(width: u16) -> usize {
    (width as usize).saturating_sub(2 + DIALOG_LABEL_WIDTH + 2 + 2)
}

/// Gutter marker + label + gap shared by every dialog field row. The active
/// field is marked with `▸` and a bright label; its value sits on a subtle
/// background so it reads as a focused input box.
fn dialog_field_line(active: bool, label: &str, value_spans: Vec<Span<'static>>) -> Line<'static> {
    let label_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let mut spans = vec![
        Span::styled(
            if active { "▸ " } else { "  " },
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{label:<width$}", width = DIALOG_LABEL_WIDTH),
            label_style,
        ),
        Span::raw("  "),
    ];
    spans.extend(value_spans);
    Line::from(spans)
}

/// Value spans for an editable text field: a focused "input box" while
/// active, the stored value otherwise, and a dim placeholder when empty.
fn text_value_spans(
    field: &EditText,
    active: bool,
    width: usize,
    cursor_visible: bool,
    placeholder: &'static str,
) -> Vec<Span<'static>> {
    if active {
        vec![Span::styled(
            edit_text_viewport(field, width, cursor_visible),
            Style::default()
                .fg(Color::White)
                .bg(DIALOG_FIELD_BG)
                .add_modifier(Modifier::BOLD),
        )]
    } else if field.value.is_empty() {
        vec![Span::styled(
            pad_truncated(placeholder, width),
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        vec![Span::styled(
            pad_truncated(&field.value, width),
            Style::default().fg(Color::Gray),
        )]
    }
}

/// Value spans for a boolean field: a `[x]`/`[ ]` indicator plus an optional
/// dim suffix explaining what the toggle means.
fn checkbox_spans(checked: bool, active: bool, suffix: Option<&'static str>) -> Vec<Span<'static>> {
    let style = if active {
        Style::default()
            .fg(Color::White)
            .bg(DIALOG_FIELD_BG)
            .add_modifier(Modifier::BOLD)
    } else if checked {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut spans = vec![Span::styled(checkbox(checked), style)];
    if let Some(suffix) = suffix {
        spans.push(Span::styled(
            format!("  {suffix}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
}

fn update_field_line(
    dialog: &UpdateDialog,
    field: UpdateField,
    width: u16,
    cursor_visible: bool,
) -> Line<'static> {
    let active = dialog.active_field() == field;
    let value_width = dialog_value_width(width);
    let (label, value_spans) = match field {
        UpdateField::Title => (
            "Title",
            text_value_spans(&dialog.title, active, value_width, cursor_visible, "‹auto›"),
        ),
        UpdateField::Tags => (
            "Tags",
            text_value_spans(&dialog.tags, active, value_width, cursor_visible, "‹none›"),
        ),
        UpdateField::Notifications => (
            "Notifications",
            checkbox_spans(dialog.notifications_enabled, active, None),
        ),
    };
    dialog_field_line(active, label, value_spans)
}

fn update_read_only_line(label: &str, value: &str, width: u16) -> Line<'static> {
    let value_width = dialog_value_width(width);
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{label:<width$}", width = DIALOG_LABEL_WIDTH),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        Span::styled(
            pad_truncated(value, value_width),
            Style::default().fg(Color::Gray),
        ),
    ])
}

fn update_read_only_values(summary: &SessionSummary) -> Vec<(&'static str, String)> {
    vec![
        ("ID", summary.id.clone()),
        (
            "State / PID",
            format!(
                "{} / {}",
                summary.status,
                summary
                    .pid
                    .map_or_else(|| "—".to_string(), |pid| pid.to_string())
            ),
        ),
        ("Command", summary.command.clone()),
        ("Args", display_words(&summary.args)),
        (
            "Cwd",
            summary.cwd.clone().unwrap_or_else(|| "—".to_string()),
        ),
        (
            "Node",
            summary.node.clone().unwrap_or_else(|| "local".to_string()),
        ),
        (
            "Terminal",
            format!(
                "{}x{}",
                summary
                    .cols
                    .map_or_else(|| "—".to_string(), |cols| cols.to_string()),
                summary
                    .rows
                    .map_or_else(|| "—".to_string(), |rows| rows.to_string())
            ),
        ),
        (
            "Created",
            super::list::format_timestamp_local(summary.created_at),
        ),
        (
            "Started",
            format_dialog_timestamp(summary.started_at.as_ref()),
        ),
        ("Ended", format_dialog_timestamp(summary.ended_at.as_ref())),
        (
            "Runtime",
            format!(
                "input={} attaches={}",
                if summary.input_needed {
                    "needed"
                } else {
                    "clear"
                },
                summary.attach_count
            ),
        ),
        (
            "Output",
            format!(
                "{} · last {}",
                format_bytes(summary.last_total_bytes as f64),
                format_dialog_timestamp(summary.last_output_epoch.as_ref())
            ),
        ),
    ]
}

fn display_words(words: &[String]) -> String {
    if words.is_empty() {
        "—".to_string()
    } else {
        format_terminal_words(words)
    }
}

fn format_dialog_timestamp(timestamp: Option<&DateTime<Utc>>) -> String {
    timestamp.map_or_else(
        || "—".to_string(),
        |timestamp| super::list::format_timestamp_local(*timestamp),
    )
}

fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = area.width.saturating_sub(2).min(max_width).max(1);
    let height = area.height.saturating_sub(2).min(max_height).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn clone_field_line(
    dialog: &CloneDialog,
    field: CloneField,
    width: u16,
    cursor_visible: bool,
) -> Line<'static> {
    let active = dialog.active_field() == field;
    let value_width = dialog_value_width(width);
    let (label, value_spans) = match field {
        CloneField::Command => (
            "Command",
            text_value_spans(
                &dialog.command,
                active,
                value_width,
                cursor_visible,
                "‹required›",
            ),
        ),
        CloneField::Args => (
            "Arguments",
            text_value_spans(&dialog.args, active, value_width, cursor_visible, "‹none›"),
        ),
        CloneField::Cwd => (
            "Directory",
            text_value_spans(
                &dialog.cwd,
                active,
                value_width,
                cursor_visible,
                "‹default›",
            ),
        ),
        CloneField::Title => (
            "Title",
            text_value_spans(&dialog.title, active, value_width, cursor_visible, "‹auto›"),
        ),
        CloneField::Tags => (
            "Tags",
            text_value_spans(&dialog.tags, active, value_width, cursor_visible, "‹none›"),
        ),
        CloneField::Node => (
            "Node",
            text_value_spans(&dialog.node, active, value_width, cursor_visible, "‹local›"),
        ),
        CloneField::Rows => (
            "Rows",
            text_value_spans(&dialog.rows, active, value_width, cursor_visible, "‹auto›"),
        ),
        CloneField::Cols => (
            "Columns",
            text_value_spans(&dialog.cols, active, value_width, cursor_visible, "‹auto›"),
        ),
        CloneField::DisableNotifications => (
            "Notifications",
            checkbox_spans(!dialog.disable_notifications, active, None),
        ),
        CloneField::AttachAfterStart => (
            "Attach",
            checkbox_spans(dialog.attach_after_start, active, Some("on start")),
        ),
    };
    dialog_field_line(active, label, value_spans)
}

fn edit_text_viewport(field: &EditText, width: usize, cursor_visible: bool) -> String {
    if width == 0 {
        return String::new();
    }

    let cursor_byte = field.byte_index();
    let cursor_cell = UnicodeWidthStr::width(&field.value[..cursor_byte]);
    let mut content = field.value.clone();
    content.insert(cursor_byte, if cursor_visible { '▏' } else { ' ' });
    let total_width = UnicodeWidthStr::width(content.as_str());
    let viewport_start = cursor_cell
        .saturating_sub(width / 2)
        .min(total_width.saturating_sub(width));
    let viewport_end = viewport_start + width;
    let mut position = 0;
    let mut visible = String::new();

    for character in content.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        let character_end = position + character_width;
        if character_end > viewport_start && position < viewport_end {
            let visible_width = UnicodeWidthStr::width(visible.as_str());
            if visible_width + character_width <= width {
                visible.push(character);
            }
        }
        position = character_end;
        if position >= viewport_end {
            break;
        }
    }

    let padding = width.saturating_sub(UnicodeWidthStr::width(visible.as_str()));
    visible.push_str(&" ".repeat(padding));
    visible
}

fn clone_cursor_visible() -> bool {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(true, |elapsed| (elapsed.as_millis() / 500) % 2 == 0)
}

fn checkbox(checked: bool) -> String {
    if checked {
        "[x]".to_string()
    } else {
        "[ ]".to_string()
    }
}

fn session_table_widths(mode: LayoutMode, show_node: bool) -> Vec<Constraint> {
    let mut widths = match mode {
        LayoutMode::Narrow => vec![
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length(COMPACT_SPARKLINE_WIDTH as u16),
        ],
        LayoutMode::Medium => vec![
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length((SPARKLINE_WIDTH + 9) as u16),
        ],
        // Same column order as the narrower modes (ID before SESSION before
        // STATE before AGE before RATE); PID slots in after SESSION, OUTPUT
        // after RATE and the flexible COMMAND column goes last.
        LayoutMode::Wide => vec![
            Constraint::Length(1),
            Constraint::Length(8),
            Constraint::Length(22),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length((SPARKLINE_WIDTH + 9) as u16),
            Constraint::Length(8),
            Constraint::Fill(1),
        ],
    };
    if show_node {
        widths.insert(1, Constraint::Length(10));
    }
    widths
}

fn session_table_alignments(mode: LayoutMode, show_node: bool) -> Vec<Alignment> {
    let mut alignments = match mode {
        LayoutMode::Narrow | LayoutMode::Medium => vec![Alignment::Left; 6],
        LayoutMode::Wide => vec![
            Alignment::Left,
            Alignment::Left,
            Alignment::Left,
            Alignment::Right,
            Alignment::Left,
            Alignment::Left,
            Alignment::Left,
            Alignment::Right,
            Alignment::Left,
        ],
    };
    if show_node {
        alignments.insert(1, Alignment::Left);
    }
    alignments
}

fn session_table_header(mode: LayoutMode, show_node: bool) -> Row<'static> {
    let mut labels = match mode {
        LayoutMode::Narrow => vec!["", "ID", "SESSION", "STATE", "AGE", "I/O"],
        LayoutMode::Medium => vec!["", "ID", "SESSION", "STATE", "AGE", "RATE"],
        LayoutMode::Wide => vec![
            "", "ID", "SESSION", "PID", "STATE", "AGE", "RATE", "OUTPUT", "COMMAND",
        ],
    };
    if show_node {
        labels.insert(1, "NODE");
    }
    let cells = labels
        .into_iter()
        .zip(session_table_alignments(mode, show_node))
        .map(|(label, alignment)| aligned_cell(label, alignment));
    Row::new(cells).style(
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    )
}

fn aligned_cell(content: impl Into<Line<'static>>, alignment: Alignment) -> Cell<'static> {
    Cell::from(content.into().alignment(alignment))
}

/// Background of the selected session row. Kept dark and muted so the
/// semantic status colours (yellow attention, red failure, green running)
/// stay clearly readable on top of it.
const SELECTED_ROW_BG: Color = Color::Rgb(25, 55, 72);

fn render_tree(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    _mode: LayoutMode,
    now: Instant,
    attention_row_rects: &mut Vec<(String, Rect, bool)>,
) -> (usize, usize, u16) {
    // Render the tree as a single Paragraph: rows are emitted as `Line`s
    // composed of indentation glyphs, the state icon, cmd+args, title, and
    // a final start-time column. No `Table` so the
    // file-explorer look matches what the user typed.
    let total = app.tree.visible.len();
    if total == 0 {
        frame.render_widget(
            Paragraph::new("  (no sessions)").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return (0, 0, 0);
    }

    // Centre the cursor inside the viewport so drill toggles feel snappy.
    let viewport_len = area.height as usize;
    let viewport_start = app
        .tree
        .cursor
        .saturating_sub(viewport_len / 2)
        .min(total.saturating_sub(viewport_len));
    let viewport_end = (viewport_start + viewport_len).min(total);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(viewport_end - viewport_start);
    for (row, entry) in app.tree.visible[viewport_start..viewport_end]
        .iter()
        .copied()
        .enumerate()
    {
        let absolute_index = viewport_start + row;
        let is_selected = absolute_index == app.tree.cursor;
        match entry {
            TreeEntry::Folder { node, .. } => {
                let prefix = tree_connector(&app.tree.visible, absolute_index);
                lines.push(folder_line(&app.tree.nodes[node], &prefix, is_selected));
            }
            TreeEntry::Session { session, .. } => {
                let Some(session_summary) = app.sessions.get(session) else {
                    lines.push(blank_line());
                    continue;
                };
                let line = session_line(
                    session_summary,
                    app.rates.get(&session_key(session_summary)),
                    &tree_connector(&app.tree.visible, absolute_index),
                    is_selected,
                    now,
                );
                lines.push(line);
                if session_summary.input_needed && session_summary.notifications_enabled {
                    let row_y = area.y + row as u16;
                    // Match the table's per-row attention pulse, including
                    // its distinct selected-row background.
                    attention_row_rects.push((
                        session_key(session_summary),
                        Rect {
                            x: area.x + (app.tree.visible[absolute_index].depth() as u16) * 4,
                            y: row_y,
                            width: 1,
                            height: 1,
                        },
                        is_selected,
                    ));
                }
            }
        }
    }

    frame.render_widget(
        Paragraph::new(lines).style(Style::default().fg(Color::White)),
        area,
    );

    (
        viewport_len,
        viewport_start,
        (viewport_end - viewport_start) as u16,
    )
}

impl TreeEntry {
    fn depth(self) -> usize {
        match self {
            Self::Folder { depth, .. } | Self::Session { depth, .. } => depth,
        }
    }
}

/// Draw a real tree edge for each visible ancestor. Looking at the whole
/// visible list (not just the viewport) keeps vertical lines continuous when
/// the user scrolls past a sibling.
fn tree_connector(entries: &[TreeEntry], index: usize) -> String {
    let depth = entries[index].depth();
    let has_next_sibling = |level: usize| {
        entries[index + 1..]
            .iter()
            .find(|entry| entry.depth() <= level)
            .is_some_and(|entry| entry.depth() == level)
    };
    let mut prefix = String::new();
    for level in 1..depth {
        prefix.push_str(if has_next_sibling(level) {
            "│   "
        } else {
            "    "
        });
    }
    prefix.push_str(if has_next_sibling(depth) {
        "├── "
    } else {
        "└── "
    });
    prefix
}

fn blank_line() -> Line<'static> {
    Line::from(String::new())
}

fn folder_line(node: &TreeNode, prefix: &str, selected: bool) -> Line<'static> {
    let label = if node.is_node {
        format!("{} (node)", node.name)
    } else if node.name.is_empty() {
        // Defensive fallback: the build pipeline must not produce empty
        // names anymore (we strip `RootDir` components when inserting),
        // but if anything ever slips past that, render the path so the
        // user still sees a meaningful breadcrumb instead of a bare
        // "(root)" stacked against siblings.
        node.path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string()
    } else {
        format!("{}/", node.name)
    };
    let marker_style = Style::default().fg(if selected {
        Color::Cyan
    } else {
        Color::DarkGray
    });
    let label_style = Style::default()
        .fg(if node.is_node {
            Color::Cyan
        } else if selected {
            Color::White
        } else {
            Color::Gray
        })
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let line = Line::from(vec![
        Span::styled(prefix.to_string(), marker_style),
        Span::styled(label, label_style),
    ]);
    if selected {
        line.style(Style::default().bg(SELECTED_ROW_BG))
    } else {
        line
    }
}

const TREE_STATUS_WIDTH: usize = 9;

fn session_line(
    session: &SessionSummary,
    rate: Option<&RateState>,
    prefix: &str,
    selected: bool,
    now: Instant,
) -> Line<'static> {
    let active = is_active_status(&session.status);
    let (glyph, status_style) = session_status_style(session, selected);
    let muted = if selected { Color::White } else { Color::Gray };
    let dim = Style::default().fg(if selected {
        Color::White
    } else {
        Color::DarkGray
    });
    let cmd_args = if session.args.is_empty() {
        session.command.clone()
    } else {
        format!("{} {}", session.command, session.args.join(" "))
    };
    let current_rate = rate.map(|value| value.display_rate(now)).unwrap_or(0.0);
    let animation_age = rate
        .map(|value| now.saturating_duration_since(value.sampled_at))
        .unwrap_or_default();
    let color = if active {
        rate_color(current_rate, animation_age)
    } else {
        Color::DarkGray
    };
    let started = session.started_at.unwrap_or(session.created_at);
    let title_text = session.title.clone().unwrap_or_default();
    let line = Line::from(vec![
        Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled(glyph.to_string(), status_style),
        Span::raw("  "),
        Span::styled(
            pad_truncated(
                status_label(&session.status, session.input_needed),
                TREE_STATUS_WIDTH,
            ),
            status_style,
        ),
        Span::raw("  "),
        Span::styled(cmd_args, dim),
        Span::raw("  "),
        Span::styled(title_text, Style::default().fg(muted)),
        Span::raw("   "),
        Span::styled(
            if active {
                sparkline(rate, SPARKLINE_WIDTH)
            } else {
                " ".repeat(SPARKLINE_WIDTH)
            },
            Style::default().fg(color),
        ),
        Span::raw(" "),
        Span::styled(format_tree_start(started, Utc::now()), dim),
    ]);
    if selected {
        line.style(Style::default().bg(SELECTED_ROW_BG))
    } else {
        line
    }
}

fn format_tree_start(started: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(started);
    if elapsed.num_seconds() <= 0 {
        "just now".to_string()
    } else if elapsed.num_hours() >= 24 {
        super::list::format_timestamp_local(started)
    } else if elapsed.num_hours() > 0 {
        format!("{}h ago", elapsed.num_hours())
    } else if elapsed.num_minutes() > 0 {
        format!("{}m ago", elapsed.num_minutes())
    } else {
        format!("{}s ago", elapsed.num_seconds())
    }
}

/// Shared semantic styling for the table and the tree. The attention glyph
/// and label stay bold/yellow on a selected or pulsing row.
fn session_status_style(session: &SessionSummary, selected: bool) -> (&'static str, Style) {
    let (glyph, mut color) = status_glyph(&session.status, session.input_needed);
    if !is_active_status(&session.status) {
        color = if selected {
            Color::Gray
        } else {
            Color::DarkGray
        };
    }
    let mut style = Style::default().fg(color);
    if session.input_needed {
        style = style.add_modifier(Modifier::BOLD);
    }
    (glyph, style)
}
fn session_row(
    session: &SessionSummary,
    rate: Option<&RateState>,
    mode: LayoutMode,
    now: Instant,
    show_node: bool,
    selected: bool,
) -> Row<'static> {
    let active = is_active_status(&session.status);
    let (status_glyph, status_style) = session_status_style(session, selected);
    // Purely decorative cells (ids, ages, byte counts) brighten on the
    // selected row; semantic colours (status, rate, node) never change.
    let muted = if selected {
        Color::White
    } else {
        Color::DarkGray
    };
    let status_text = status_label(&session.status, session.input_needed);
    let name = session
        .title
        .clone()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| session.command.clone());
    let age = super::list::format_age(session.created_at, session.started_at, session.ended_at);
    let current_rate = rate.map(|value| value.display_rate(now)).unwrap_or(0.0);
    let animation_age = rate
        .map(|value| now.saturating_duration_since(value.sampled_at))
        .unwrap_or_default();
    let rate_color = if active {
        rate_color(current_rate, animation_age)
    } else {
        Color::DarkGray
    };
    let alignments = session_table_alignments(mode, show_node);
    let node_offset = usize::from(show_node);
    let mut cells = match mode {
        LayoutMode::Narrow => vec![
            aligned_cell(Span::styled(status_glyph, status_style), alignments[0]),
            aligned_cell(session.id.clone(), alignments[1 + node_offset])
                .style(Style::default().fg(muted)),
            aligned_cell(name, alignments[2 + node_offset]),
            aligned_cell(status_text.to_string(), alignments[3 + node_offset]).style(status_style),
            aligned_cell(age, alignments[4 + node_offset]).style(Style::default().fg(muted)),
            aligned_cell(
                if active {
                    sparkline(rate, COMPACT_SPARKLINE_WIDTH)
                } else {
                    " ".repeat(COMPACT_SPARKLINE_WIDTH)
                },
                alignments[5 + node_offset],
            )
            .style(Style::default().fg(rate_color)),
        ],
        LayoutMode::Medium => vec![
            aligned_cell(Span::styled(status_glyph, status_style), alignments[0]),
            aligned_cell(session.id.clone(), alignments[1 + node_offset])
                .style(Style::default().fg(muted)),
            aligned_cell(name, alignments[2 + node_offset]),
            aligned_cell(status_text.to_string(), alignments[3 + node_offset]).style(status_style),
            aligned_cell(age, alignments[4 + node_offset]).style(Style::default().fg(muted)),
            aligned_cell(
                if active {
                    format!(
                        "{} {:>6}/s",
                        sparkline(rate, SPARKLINE_WIDTH),
                        format_bytes(current_rate)
                    )
                } else {
                    format!("{:>13}", " ")
                },
                alignments[5 + node_offset],
            )
            .style(Style::default().fg(rate_color)),
        ],
        LayoutMode::Wide => {
            let command = if session.args.is_empty() {
                session.command.clone()
            } else {
                format!("{} {}", session.command, session.args.join(" "))
            };
            vec![
                aligned_cell(Span::styled(status_glyph, status_style), alignments[0]),
                aligned_cell(session.id.clone(), alignments[1 + node_offset])
                    .style(Style::default().fg(muted)),
                aligned_cell(name, alignments[2 + node_offset]),
                aligned_cell(
                    session.pid.map_or("-".into(), |pid| pid.to_string()),
                    alignments[3 + node_offset],
                )
                .style(Style::default().fg(muted)),
                aligned_cell(status_text.to_string(), alignments[4 + node_offset])
                    .style(status_style),
                aligned_cell(age, alignments[5 + node_offset]).style(Style::default().fg(muted)),
                aligned_cell(
                    if active {
                        format!(
                            "{} {:>6}/s",
                            sparkline(rate, SPARKLINE_WIDTH),
                            format_bytes(current_rate)
                        )
                    } else {
                        format!("{:>13}", " ")
                    },
                    alignments[6 + node_offset],
                )
                .style(Style::default().fg(rate_color)),
                aligned_cell(
                    format_bytes(session.last_total_bytes as f64),
                    alignments[7 + node_offset],
                )
                .style(Style::default().fg(muted)),
                aligned_cell(command, alignments[8 + node_offset]),
            ]
        }
    };
    if show_node {
        cells.insert(
            1,
            aligned_cell(
                session.node.clone().unwrap_or_else(|| "local".to_string()),
                alignments[1],
            )
            .style(Style::default().fg(Color::Cyan)),
        );
    }
    let row = Row::new(cells);
    if selected {
        // The selection paints background, bold and a bright foreground as
        // the row's base style. Cell styles sit on top of it, so the status
        // glyph/label keep their semantic colours even while selected.
        return row.style(
            Style::default()
                .fg(Color::White)
                .bg(SELECTED_ROW_BG)
                .add_modifier(Modifier::BOLD),
        );
    }
    // A session's own terminal colours (reported via OSC 10/11) identify it
    // in the list. They are applied as the row's base style, and the
    // cell-level styles on top keep the status glyph/label colours (yellow
    // attention, red failure, green running) more noticeable than the
    // session foreground. Inactive rows keep their dimmed foreground; only
    // the session background carries over.
    let session_fg = session
        .foreground_color
        .as_deref()
        .and_then(parse_terminal_color);
    let session_bg = session
        .background_color
        .as_deref()
        .and_then(parse_terminal_color);
    let mut base = Style::default();
    if let Some(bg) = session_bg {
        base = base.bg(bg);
    }
    if active {
        if let Some(fg) = session_fg {
            base = base.fg(fg);
        }
        row.style(base)
    } else {
        row.style(base.fg(Color::DarkGray))
    }
}

fn aggregate_sparkline_data(rates: &HashMap<String, RateState>, width: usize) -> Vec<u64> {
    (0..width)
        .map(|index| {
            rates
                .values()
                .filter_map(|rate| rate.history.iter().rev().nth(width - index - 1))
                .sum::<f64>()
        })
        .map(|value| value.ceil().max(0.0) as u64)
        .collect()
}

fn sparkline(rate: Option<&RateState>, width: usize) -> String {
    let Some(rate) = rate else {
        return "▁".repeat(width);
    };
    let values = rate
        .history
        .iter()
        .rev()
        .take(width)
        .copied()
        .collect::<Vec<_>>();
    let max = values.iter().copied().fold(1.0_f64, f64::max);
    let padding = width.saturating_sub(values.len());
    let spark = values
        .iter()
        .rev()
        .map(|value| {
            let index = ((value / max) * 7.0).round() as usize;
            SPARK_BLOCKS[index.min(SPARK_BLOCKS.len() - 1)]
        })
        .collect::<String>();
    format!("{}{}", "▁".repeat(padding), spark)
}

fn rate_color(rate: f64, animation_age: Duration) -> Color {
    if rate <= 0.0 {
        Color::DarkGray
    } else {
        let pulse = ((animation_age.as_millis() / 40) % 5) as u8;
        Color::Rgb(45 + pulse * 8, 190 + pulse * 8, 180 + pulse * 10)
    }
}

fn format_bytes(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0}{}", UNITS[unit])
    } else if value < 10.0 {
        format!("{value:.1}{}", UNITS[unit])
    } else {
        format!("{value:.0}{}", UNITS[unit])
    }
}

fn pad_truncated(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    format!("{value}{}", " ".repeat(padding))
}

fn status_label(status: &str, input_needed: bool) -> &str {
    if input_needed { "attention" } else { status }
}

/// Parse a terminal colour spec (as reported by a session's `OSC 10`/`OSC 11`
/// replies, e.g. `#rrggbb`, X11 `rgb:r/g/b` / `rgbi:r/g/b`, or a colour name)
/// into a ratatui colour. Unrecognised specs fall back to the default style.
fn parse_terminal_color(spec: &str) -> Option<Color> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(body) = spec
        .strip_prefix("rgb:")
        .or_else(|| spec.strip_prefix("RGB:"))
    {
        let mut parts = body.split('/');
        let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        return Some(Color::Rgb(
            scale_hex_component(r)?,
            scale_hex_component(g)?,
            scale_hex_component(b)?,
        ));
    }
    if let Some(body) = spec
        .strip_prefix("rgbi:")
        .or_else(|| spec.strip_prefix("RGBI:"))
    {
        let mut parts = body.split('/');
        let (r, g, b) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let component = |value: &str| -> Option<u8> {
            let value: f32 = value.parse().ok()?;
            (0.0..=1.0)
                .contains(&value)
                .then(|| (value * 255.0).round() as u8)
        };
        return Some(Color::Rgb(component(r)?, component(g)?, component(b)?));
    }
    named_color(&spec.to_ascii_lowercase())
}

/// Parse `#RGB`, `#RRGGBB`, `#RRRGGGBBB` or `#RRRRGGGGBBBB` (X11 hex forms,
/// 1–4 hex digits per component scaled to 8 bits).
fn parse_hex_color(hex: &str) -> Option<Color> {
    if hex.is_empty() || !hex.len().is_multiple_of(3) || hex.len() > 12 {
        return None;
    }
    let width = hex.len() / 3;
    Some(Color::Rgb(
        scale_hex_component(&hex[..width])?,
        scale_hex_component(&hex[width..2 * width])?,
        scale_hex_component(&hex[2 * width..])?,
    ))
}

/// Scale a 1–4 digit X11 hex component to 8 bits (the full intensity range
/// maps to 0–255 regardless of the digit count).
fn scale_hex_component(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some(((value * 0xffff / max) >> 8) as u8)
}

/// A practical subset of the X11 colour names (per `rgb.txt`) covering the
/// names terminal colour schemes typically report.
fn named_color(name: &str) -> Option<Color> {
    let rgb = match name {
        "black" => (0x00, 0x00, 0x00),
        "white" => (0xff, 0xff, 0xff),
        "red" => (0xff, 0x00, 0x00),
        "green" => (0x00, 0x80, 0x00),
        "lime" => (0x00, 0xff, 0x00),
        "blue" => (0x00, 0x00, 0xff),
        "navy" => (0x00, 0x00, 0x80),
        "yellow" => (0xff, 0xff, 0x00),
        "cyan" | "aqua" => (0x00, 0xff, 0xff),
        "teal" => (0x00, 0x80, 0x80),
        "magenta" | "fuchsia" => (0xff, 0x00, 0xff),
        "purple" => (0x80, 0x00, 0x80),
        "maroon" => (0x80, 0x00, 0x00),
        "olive" => (0x80, 0x80, 0x00),
        "orange" => (0xff, 0xa5, 0x00),
        "pink" => (0xff, 0xc0, 0xcb),
        "brown" => (0xa5, 0x2a, 0x2a),
        "gray" | "grey" => (0xbe, 0xbe, 0xbe),
        "silver" => (0xc0, 0xc0, 0xc0),
        "darkgray" | "darkgrey" => (0xa9, 0xa9, 0xa9),
        "lightgray" | "lightgrey" => (0xd3, 0xd3, 0xd3),
        _ => return None,
    };
    Some(Color::Rgb(rgb.0, rgb.1, rgb.2))
}

fn status_glyph(status: &str, input_needed: bool) -> (&'static str, Color) {
    if input_needed {
        ("◆", Color::Yellow)
    } else {
        match status {
            "running" => ("●", Color::Green),
            "failed" | "killed" => ("×", Color::Red),
            _ => ("○", Color::DarkGray),
        }
    }
}

fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value) <= width {
        return value.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }

    let mut result = String::new();
    let mut used = 0;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width - 1 {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result.push('…');
    result
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[cfg(any(windows, test))]
fn arrange_window(
    work: WindowRect,
    anchor: WindowRect,
    size: (u16, u16),
    slot: usize,
) -> WindowRect {
    let cell_width =
        ((size.0 as u32).saturating_mul(9).saturating_add(32)).clamp(480, work.width.max(1));
    let cell_height =
        ((size.1 as u32).saturating_mul(19).saturating_add(48)).clamp(320, work.height.max(1));
    let columns = (work.width / cell_width.max(1)).max(1) as usize;
    let rows = (work.height / cell_height.max(1)).max(1) as usize;
    let cells = columns.saturating_mul(rows).max(1);
    let index = slot % cells;
    let x = work.x + ((index % columns) as u32 * cell_width) as i32;
    let y = work.y + ((index / columns) as u32 * cell_height) as i32;
    let fallback_x = (anchor.x + 28 * slot as i32).clamp(
        work.x,
        work.x + work.width.saturating_sub(cell_width) as i32,
    );
    let fallback_y = (anchor.y + 28 * slot as i32).clamp(
        work.y,
        work.y + work.height.saturating_sub(cell_height) as i32,
    );
    WindowRect {
        x: if cells > 1 { x } else { fallback_x },
        y: if cells > 1 { y } else { fallback_y },
        width: cell_width,
        height: cell_height,
    }
}

fn terminal_marker(id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("oly-list-{id}-{}.open", uuid::Uuid::new_v4()))
}

fn shell_command(
    executable: &str,
    args: &[String],
    marker: Option<&Path>,
    powershell: bool,
) -> String {
    if powershell {
        let command = std::iter::once(executable.to_string())
            .chain(args.iter().cloned())
            .map(|value| format!("'{}'", value.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(" ");
        match marker {
            Some(marker) => format!(
                "$m='{}'; New-Item -ItemType File -Force $m | Out-Null; try {{ & {command} }} finally {{ Remove-Item -Force $m -ErrorAction SilentlyContinue }}",
                marker.display().to_string().replace('\'', "''")
            ),
            None => format!("& {command}"),
        }
    } else {
        let command = std::iter::once(executable.to_string())
            .chain(args.iter().cloned())
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        match marker {
            Some(marker) => format!(
                "m={}; touch \"$m\"; trap 'rm -f \"$m\"' EXIT; {command}",
                shell_quote(marker.display().to_string())
            ),
            None => command,
        }
    }
}

fn session_command(
    id: &str,
    node: Option<&str>,
    attach: bool,
) -> io::Result<(String, Vec<String>)> {
    let executable = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut args = vec![
        if attach { "attach" } else { "logs" }.to_string(),
        id.to_string(),
    ];
    if !attach {
        args.push("--keep-color".to_string());
    }
    if let Some(node) = node {
        args.extend(["--node".to_string(), node.to_string()]);
    }
    Ok((executable, args))
}

#[cfg(windows)]
fn powershell_encoded_command(script: &str) -> String {
    use base64::Engine as _;

    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

#[cfg(windows)]
fn spawn_session_terminal(
    id: &str,
    node: Option<&str>,
    size: (u16, u16),
    slot: usize,
    attach: bool,
    marker: Option<&Path>,
) -> io::Result<()> {
    let (executable, attach_args) = session_command(id, node, attach)?;
    let (work, anchor) = windows_screen_geometry();
    let rect = arrange_window(work, anchor, size, slot);
    let mut command = Command::new("wt.exe");
    command.args([
        "-w",
        "new",
        "--pos",
        &format!("{},{}", rect.x, rect.y),
        "--size",
        &format!("{},{}", size.0, size.1),
        "--title",
        &format!("oly · {id}"),
    ]);
    let script = shell_command(&executable, &attach_args, marker, true);
    let encoded_script = powershell_encoded_command(&script);
    command.arg("powershell.exe");
    command.arg("-NoProfile");
    if !attach {
        command.arg("-NoExit");
    }
    command.args(["-EncodedCommand", &encoded_script]);
    command.spawn().map(|_| ())
}

#[cfg(windows)]
fn windows_screen_geometry() -> (WindowRect, WindowRect) {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        },
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect},
    };

    unsafe {
        let window = GetForegroundWindow();
        let mut current = RECT::default();
        let _ = GetWindowRect(window, &mut current);
        let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let convert = |rect: RECT| WindowRect {
            x: rect.left,
            y: rect.top,
            width: (rect.right - rect.left).max(1) as u32,
            height: (rect.bottom - rect.top).max(1) as u32,
        };
        (convert(info.rcWork), convert(current))
    }
}

#[cfg(target_os = "macos")]
fn spawn_session_terminal(
    id: &str,
    node: Option<&str>,
    size: (u16, u16),
    slot: usize,
    attach: bool,
    marker: Option<&Path>,
) -> io::Result<()> {
    let (executable, args) = session_command(id, node, attach)?;
    let mut shell = shell_command(&executable, &args, marker, false);
    if !attach {
        shell.push_str("; printf '\\nPress Enter to close…'; read _");
    }
    let script = format!(
        "tell application \"Terminal\" to do script \"{}\"",
        shell.replace('"', "\\\"")
    );
    let _ = (size, slot);
    Command::new("osascript")
        .args(["-e", &script])
        .spawn()
        .map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn spawn_session_terminal(
    id: &str,
    node: Option<&str>,
    size: (u16, u16),
    slot: usize,
    attach: bool,
    marker: Option<&Path>,
) -> io::Result<()> {
    let (executable, args) = session_command(id, node, attach)?;
    let geometry = format!(
        "{}x{}+{}+{}",
        size.0,
        size.1,
        24 + slot * 28,
        24 + slot * 28
    );
    let terminal = ["xterm", "gnome-terminal", "konsole", "alacritty", "kitty"]
        .into_iter()
        .find(|candidate| which::which(candidate).is_ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no supported terminal emulator found (xterm, gnome-terminal, konsole, alacritty, or kitty)",
            )
        })?;
    let mut shell = shell_command(&executable, &args, marker, false);
    if !attach {
        shell.push_str("; printf '\\nPress Enter to close…'; read _");
    }
    let title = format!("oly · {id}");
    let mut command = Command::new(terminal);
    match terminal {
        "xterm" => {
            command.args([
                "-geometry",
                &geometry,
                "-T",
                &title,
                "-e",
                "sh",
                "-c",
                &shell,
            ]);
        }
        "gnome-terminal" => {
            command.args([
                "--title",
                &title,
                &format!("--geometry={geometry}"),
                "--",
                "sh",
                "-c",
                &shell,
            ]);
        }
        "konsole" => {
            command.args(["--title", &title, "-e", "sh", "-c", &shell]);
        }
        "alacritty" => {
            command.args([
                "--title",
                &title,
                "--dimensions",
                &size.0.to_string(),
                &size.1.to_string(),
                "-e",
                "sh",
                "-c",
                &shell,
            ]);
        }
        "kitty" => {
            command.args(["--title", &title, "sh", "-c", &shell]);
        }
        _ => unreachable!(),
    }
    command.spawn().map(|_| ())
}

fn shell_quote(value: String) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{
        App, AppAction, CloneField, CloneLaunch, LIST_WINDOW_TITLE, SessionRefresh,
        TITLE_RESTORE_BYTES, TITLE_SAVE_BYTES, TUI_RESTORE_BYTES, WindowRect, apply_refresh,
        arrange_window, enter_list_title, panic_payload_message, restore_tui_state, route_key,
    };
    use crate::{
        error::AppError,
        protocol::{RpcRequest, RpcResponse, SessionSummary},
    };
    use chrono::{Local, TimeZone, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, layout::Alignment};
    use std::collections::HashSet;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn session(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            title: None,
            tags: vec![],
            command: "cmd".to_string(),
            args: vec![],
            pid: None,
            status: "running".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            started_at: None,
            ended_at: None,
            cwd: None,
            input_needed: false,
            // Mirrors the daemon default: notifications on unless disabled.
            notifications_enabled: true,
            node: None,
            last_total_bytes: 0,
            last_output_epoch: None,
            rows: Some(24),
            cols: Some(80),
            attach_count: 0,
            foreground_color: None,
            background_color: None,
            journal_bytes_retained: None,
            journal_retention_sweeps: None,
            journal_incarnations_dropped: None,
            journal_byte_cap: None,
        }
    }

    /// Render a single wide-mode session row into a buffer so cell styles
    /// (fg/bg) can be asserted directly.
    fn render_session_row(session: &SessionSummary) -> ratatui::buffer::Buffer {
        render_session_row_selected(session, false)
    }

    fn render_session_row_selected(
        session: &SessionSummary,
        selected: bool,
    ) -> ratatui::buffer::Buffer {
        use ratatui::{layout::Rect, widgets::Widget};
        let row = super::session_row(
            session,
            None,
            super::LayoutMode::Wide,
            std::time::Instant::now(),
            false,
            selected,
        );
        let table = ratatui::widgets::Table::new(
            vec![row],
            super::session_table_widths(super::LayoutMode::Wide, false),
        )
        .column_spacing(1);
        let area = Rect::new(0, 0, 140, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        Widget::render(table, area, &mut buffer);
        buffer
    }

    /// X position of the first character of `text` on row 0.
    fn find_text(buffer: &ratatui::buffer::Buffer, text: &str) -> Option<u16> {
        let width = buffer.area().width;
        let line: String = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
        line.find(text).map(|byte| byte as u16)
    }

    #[test]
    fn parse_terminal_color_accepts_terminal_colour_specs() {
        assert_eq!(
            super::parse_terminal_color("#1e1e1e"),
            Some(ratatui::style::Color::Rgb(0x1e, 0x1e, 0x1e))
        );
        assert_eq!(
            super::parse_terminal_color("#fff"),
            Some(ratatui::style::Color::Rgb(0xff, 0xff, 0xff))
        );
        assert_eq!(
            super::parse_terminal_color("#ffffffff0000"),
            Some(ratatui::style::Color::Rgb(0xff, 0xff, 0x00))
        );
        assert_eq!(
            super::parse_terminal_color("rgb:ffff/0000/8080"),
            Some(ratatui::style::Color::Rgb(0xff, 0x00, 0x80))
        );
        assert_eq!(
            super::parse_terminal_color("rgb:f/0/8"),
            Some(ratatui::style::Color::Rgb(0xff, 0x00, 0x88))
        );
        assert_eq!(
            super::parse_terminal_color("rgbi:1/0/0.5"),
            Some(ratatui::style::Color::Rgb(0xff, 0x00, 0x80))
        );
        assert_eq!(
            super::parse_terminal_color("red"),
            Some(ratatui::style::Color::Rgb(0xff, 0x00, 0x00))
        );
        assert_eq!(
            super::parse_terminal_color("DarkGray"),
            Some(ratatui::style::Color::Rgb(0xa9, 0xa9, 0xa9))
        );
        assert_eq!(super::parse_terminal_color(""), None);
        assert_eq!(super::parse_terminal_color("rgb:zz/00/00"), None);
        assert_eq!(super::parse_terminal_color("rgbi:2/0/0"), None);
        assert_eq!(super::parse_terminal_color("#12345"), None);
        assert_eq!(super::parse_terminal_color("chartreuse-ish"), None);
    }

    #[test]
    fn session_row_uses_the_sessions_terminal_colours() {
        let mut item = session("coloured");
        item.title = Some("deploy".to_string());
        item.foreground_color = Some("rgb:ffff/ffff/ffff".to_string());
        item.background_color = Some("#1e1e1e".to_string());

        let buffer = render_session_row(&item);

        // The session name cell carries the session's own colours.
        let name_x = find_text(&buffer, "deploy").expect("name rendered");
        let name_cell = &buffer[(name_x, 0)];
        assert_eq!(name_cell.fg, ratatui::style::Color::Rgb(0xff, 0xff, 0xff));
        assert_eq!(name_cell.bg, ratatui::style::Color::Rgb(0x1e, 0x1e, 0x1e));

        // The status glyph keeps its own colour on the session background.
        let glyph = &buffer[(0, 0)];
        assert_eq!(glyph.symbol(), "●");
        assert_eq!(glyph.fg, ratatui::style::Color::Green);
        assert_eq!(glyph.bg, ratatui::style::Color::Rgb(0x1e, 0x1e, 0x1e));
    }

    #[test]
    fn session_row_keeps_attention_status_more_noticeable_than_session_colours() {
        let mut item = session("waiting");
        item.title = Some("build".to_string());
        item.input_needed = true;
        // A loud session foreground must not wash out the attention signal.
        item.foreground_color = Some("#ffff00".to_string());
        item.background_color = Some("#1e1e1e".to_string());

        let buffer = render_session_row(&item);

        // Attention glyph stays yellow with its dedicated emphasis...
        let glyph = &buffer[(0, 0)];
        assert_eq!(glyph.symbol(), "◆");
        assert_eq!(glyph.fg, ratatui::style::Color::Yellow);
        // ...and the status label too.
        let label_x = find_text(&buffer, "attention").expect("status label rendered");
        assert_eq!(buffer[(label_x, 0)].fg, ratatui::style::Color::Yellow);
    }

    #[test]
    fn inactive_session_row_stays_dimmed_but_keeps_session_background() {
        let mut item = session("done");
        item.title = Some("finished".to_string());
        item.status = "stopped".to_string();
        item.ended_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap());
        item.foreground_color = Some("#ffffff".to_string());
        item.background_color = Some("#000040".to_string());

        let buffer = render_session_row(&item);

        let name_x = find_text(&buffer, "finished").expect("name rendered");
        let name_cell = &buffer[(name_x, 0)];
        // Inactive rows keep the dimmed foreground...
        assert_eq!(name_cell.fg, ratatui::style::Color::DarkGray);
        // ...but still show the session's background identity.
        assert_eq!(name_cell.bg, ratatui::style::Color::Rgb(0x00, 0x00, 0x40));
    }

    #[test]
    fn session_row_without_terminal_colours_is_unchanged() {
        let mut item = session("plain");
        item.title = Some("vanilla".to_string());

        let buffer = render_session_row(&item);

        let name_x = find_text(&buffer, "vanilla").expect("name rendered");
        let name_cell = &buffer[(name_x, 0)];
        assert_eq!(name_cell.fg, ratatui::style::Color::Reset);
        assert_eq!(name_cell.bg, ratatui::style::Color::Reset);
    }

    #[test]
    fn selected_row_keeps_status_colours_and_brightens_decorative_cells() {
        let mut item = session("waiting");
        item.title = Some("build".to_string());
        item.input_needed = true;
        item.foreground_color = Some("#ffff00".to_string());
        item.background_color = Some("#1e1e1e".to_string());

        let buffer = render_session_row_selected(&item, true);

        // The attention glyph and label keep their yellow even on the
        // selection band — the highlight must never hide them.
        let glyph = &buffer[(0, 0)];
        assert_eq!(glyph.symbol(), "◆");
        assert_eq!(glyph.fg, ratatui::style::Color::Yellow);
        assert_eq!(glyph.bg, super::SELECTED_ROW_BG);
        let label_x = find_text(&buffer, "attention").expect("status label rendered");
        let label = &buffer[(label_x, 0)];
        assert_eq!(label.fg, ratatui::style::Color::Yellow);
        assert_eq!(label.bg, super::SELECTED_ROW_BG);

        // The session name is bright white on the selection band (its own
        // session colours yield to the selection).
        let name_x = find_text(&buffer, "build").expect("name rendered");
        let name = &buffer[(name_x, 0)];
        assert_eq!(name.fg, ratatui::style::Color::White);
        assert_eq!(name.bg, super::SELECTED_ROW_BG);
    }

    #[test]
    fn selected_row_keeps_failure_status_red() {
        let mut item = session("failed");
        item.title = Some("crashed".to_string());
        item.status = "failed".to_string();
        item.ended_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap());

        let buffer = render_session_row_selected(&item, true);

        let glyph = &buffer[(0, 0)];
        assert_eq!(glyph.symbol(), "×");
        // Inactive statuses stay muted, but must remain readable on the
        // selection band (gray, not the near-invisible dark gray).
        assert_eq!(glyph.fg, ratatui::style::Color::Gray);
        assert_eq!(glyph.bg, super::SELECTED_ROW_BG);
    }

    #[test]
    fn attention_pulse_runs_only_while_a_session_needs_input() {
        let mut app = App::default();
        let mut item = session("waiting");
        item.input_needed = true;
        app.replace_sessions(vec![item]);

        let _ = render_app(&mut app, 120, 12);
        assert_eq!(app.attention_rows.len(), 1);
        assert!(app.effects.is_running());

        app.sessions[0].input_needed = false;
        let _ = render_app(&mut app, 120, 12);
        assert!(app.attention_rows.is_empty());
        assert!(!app.effects.is_running());
    }

    #[test]
    fn attention_pulse_skips_sessions_with_notifications_disabled() {
        let mut app = App::default();
        let mut muted = session("muted");
        muted.input_needed = true;
        muted.notifications_enabled = false;
        let mut waiting = session("waiting");
        waiting.input_needed = true;
        app.replace_sessions(vec![muted, waiting]);

        let _ = render_app(&mut app, 120, 12);
        // The muted session never gets an attention row even though it
        // waits for input; the unmuted one pulses as usual.
        assert_eq!(app.attention_rows.len(), 1);
        assert!(app.attention_rows.contains_key("waiting"));
        assert!(!app.attention_rows.contains_key("muted"));

        // Muting a pulsing session cancels its animation.
        app.sessions[1].notifications_enabled = false;
        let _ = render_app(&mut app, 120, 12);
        assert!(app.attention_rows.is_empty());
        assert!(!app.effects.is_running());
    }

    #[test]
    fn attention_pulse_is_scoped_to_the_waiting_sessions_rows() {
        let mut app = App::default();
        let calm = session("calm");
        let mut waiting = session("waiting");
        waiting.input_needed = true;
        app.replace_sessions(vec![calm, waiting]);
        // The update dialog renders its active field label in yellow: it must
        // never be pulsed just because some session needs attention.
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        let cells = |buffer: &ratatui::buffer::Buffer| {
            let area = *buffer.area();
            (area.y..area.bottom()).flat_map(move |y| (area.x..area.right()).map(move |x| (x, y)))
        };

        app.last_frame_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(400));
        let buffer = render_app_buffer(&mut app, 120, 30);

        // The waiting session's state column pulses (interpolated colour)
        // while the status foreground is preserved...
        let glyph = cells(&buffer)
            .find(|&pos| buffer[pos].symbol() == "◆")
            .expect("attention glyph rendered");
        assert!(matches!(
            buffer[glyph].bg,
            ratatui::style::Color::Rgb(_, _, _)
        ));
        assert_eq!(buffer[glyph].fg, ratatui::style::Color::Yellow);
        // ...the calm session's state column wears the static selection
        // band (since calm is selected) and is not animated...
        let calm_glyph = cells(&buffer)
            .rev()
            .find(|&pos| buffer[pos].symbol() == "●")
            .expect("calm session glyph rendered");
        assert_eq!(buffer[calm_glyph].bg, super::SELECTED_ROW_BG);
        // ...the calm row's body cells (e.g., the command text) are *also*
        // selection-coloured, never amber — the pulse no longer touches them.
        let calm_cmd = cells(&buffer)
            .find(|&(x, y)| {
                buffer[(x, y)].symbol() == "c"
                    && ["a", "l", "m"]
                        .into_iter()
                        .enumerate()
                        .all(|(dx, s)| buffer[(x + dx as u16 + 1, y)].symbol() == s)
            })
            .expect("calm session rendered");
        assert_eq!(buffer[calm_cmd].bg, super::SELECTED_ROW_BG);
        // ...while the dialog's yellow field label keeps its exact colour.
        let label = cells(&buffer)
            .find(|&(x, y)| {
                buffer[(x, y)].symbol() == "T"
                    && ["i", "t", "l", "e"]
                        .into_iter()
                        .enumerate()
                        .all(|(dx, s)| buffer[(x + dx as u16 + 1, y)].symbol() == s)
            })
            .expect("dialog Title label rendered");
        assert_eq!(buffer[label].fg, ratatui::style::Color::Cyan);
        // Only the waiting session's row is tracked for pulsing.
        assert_eq!(app.attention_rows.len(), 1);
        assert!(app.attention_rows.contains_key("waiting"));
    }

    fn render_app_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::render(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Drain a ratatui `Buffer` into one `String` per visual row. Useful
    /// for assertions that need to match display substrings without
    /// caring about the column the substring lives in.
    fn buffer_symbols(buffer: &ratatui::buffer::Buffer, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| {
                let mut line = String::new();
                for x in 0..buffer.area.width {
                    if let Some(cell) = buffer.cell((x, y)) {
                        line.push_str(cell.symbol());
                    }
                }
                line
            })
            .collect()
    }

    #[test]
    fn attention_pulse_animates_the_state_cell_as_time_passes() {
        let mut app = App::default();
        let calm = session("calm");
        let mut waiting = session("waiting");
        waiting.input_needed = true;
        // "calm" stays selected, so the waiting row pulses unselected.
        app.replace_sessions(vec![calm, waiting]);
        let glyph_position = |buffer: &ratatui::buffer::Buffer| {
            let area = *buffer.area();
            (area.y..area.bottom())
                .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
                .find(|&(x, y)| buffer[(x, y)].symbol() == "◆")
                .expect("attention glyph rendered")
        };

        // At effect-time zero the state cell is untouched: default
        // background and the status foreground at its full yellow.
        let buffer = render_app_buffer(&mut app, 120, 12);
        let position = glyph_position(&buffer);
        assert_eq!(buffer[position].fg, ratatui::style::Color::Yellow);
        assert_eq!(buffer[position].bg, ratatui::style::Color::Reset);

        // Part-way through the pulse the state-cell background has lerped
        // towards the amber tint, while adjacent row cells keep their
        // own backgrounds (selection band or default).
        app.last_frame_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(400));
        let buffer = render_app_buffer(&mut app, 120, 12);
        match buffer[position].bg {
            ratatui::style::Color::Rgb(r, g, b) => {
                assert!(
                    r > g && b < 30,
                    "pulse should tint the state cell amber: {r},{g},{b}"
                );
            }
            other => panic!("expected an interpolated rgb background, got {other:?}"),
        }
        assert_eq!(buffer[position].fg, ratatui::style::Color::Yellow);
    }

    #[test]
    fn attention_pulse_retunes_when_the_state_cell_is_selected() {
        let mut app = App::default();
        let mut waiting = session("waiting");
        waiting.input_needed = true;
        // The waiting session starts out selected.
        app.replace_sessions(vec![waiting, session("calm")]);
        let glyph_position = |buffer: &ratatui::buffer::Buffer| {
            let area = *buffer.area();
            (area.y..area.bottom())
                .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
                .find(|&(x, y)| buffer[(x, y)].symbol() == "◆")
                .expect("attention glyph rendered")
        };

        // Selected: the pulse blends the selection band with the amber tint,
        // keeping the blue component of the selection band clearly present.
        app.last_frame_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(400));
        let buffer = render_app_buffer(&mut app, 120, 12);
        match buffer[glyph_position(&buffer)].bg {
            ratatui::style::Color::Rgb(_, _, b) => {
                assert!(
                    b > 20,
                    "selected pulse should keep the selection band: b={b}"
                );
            }
            other => panic!("expected an interpolated rgb background, got {other:?}"),
        }
        assert!(app.attention_rows["waiting"].1);

        // Moving the selection away swaps the pulse back to the plain amber
        // tint (re-registered, so it restarts from the row's own colours).
        // Moving the selection away swaps the pulse back to the plain amber
        // tint. (The swap frame still shows the outgoing effect's final
        // tick, so assert on the frame after it: at any point of the cycle
        // the unselected tint keeps the blue channel near zero, far below
        // the selection-band blend.)
        route_key(&mut app, key(KeyCode::Down), None);
        app.last_frame_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(400));
        let _ = render_app_buffer(&mut app, 120, 12);
        app.last_frame_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(400));
        let buffer = render_app_buffer(&mut app, 120, 12);
        match buffer[glyph_position(&buffer)].bg {
            ratatui::style::Color::Rgb(_, _, b) => {
                assert!(b < 15, "unselected pulse is plain amber: b={b}");
            }
            other => panic!("expected an interpolated rgb background, got {other:?}"),
        }
        assert_eq!(
            buffer[glyph_position(&buffer)].fg,
            ratatui::style::Color::Yellow
        );
        assert!(!app.attention_rows["waiting"].1);
    }

    // ----- Tree view -------------------------------------------------------

    fn session_at(id: &str, cwd: Option<&str>) -> SessionSummary {
        let mut s = session(id);
        s.cwd = cwd.map(str::to_string);
        s
    }

    fn tree_visible_ids(app: &App) -> Vec<String> {
        app.tree
            .visible
            .iter()
            .map(|entry| match entry {
                super::TreeEntry::Folder { node, .. } => {
                    let path = &app.tree.nodes[*node].path;
                    format!("folder:{}", path.to_string_lossy())
                }
                super::TreeEntry::Session { session, .. } => app.sessions[*session].id.clone(),
            })
            .collect()
    }

    #[test]
    fn ctrl_g_toggles_view_mode() {
        let mut app = App::default();
        app.replace_sessions(vec![session("alpha")]);
        assert_eq!(app.view_mode, super::ViewMode::List);
        route_key(&mut app, ctrl(KeyCode::Char('g')), None);
        assert_eq!(app.view_mode, super::ViewMode::Tree);
        route_key(&mut app, ctrl(KeyCode::Char('g')), None);
        assert_eq!(app.view_mode, super::ViewMode::List);
    }

    #[test]
    fn tree_groups_sessions_under_a_shared_ancestor() {
        let mut app = App::default();
        app.replace_sessions(vec![
            session_at("ls", Some("/work/proj")),
            session_at("vim", Some("/work/proj/sub")),
            session_at("build", Some("/home/alice")),
        ]);
        let ids = tree_visible_ids(&app);
        // Top-level folders are the children of the shared ancestor: home
        // and work. (After stripping the leading `/`, the constructed paths
        // are relative.)
        assert!(ids.iter().any(|id| id == "folder:home"), "ids = {ids:?}");
        assert!(ids.iter().any(|id| id == "folder:work"), "ids = {ids:?}");
        // Sessions render after their enclosing folder chain.
        assert!(ids.contains(&"ls".to_string()), "ids = {ids:?}");
        assert!(ids.contains(&"build".to_string()), "ids = {ids:?}");
        // `vim` lives under work/proj/sub, past the depth-2 horizon.
        assert!(
            !ids.contains(&"vim".to_string()),
            "vim should sit past auto-depth: {ids:?}"
        );
    }

    #[test]
    fn tree_arrow_keys_move_tree_cursor_only() {
        let mut app = App::default();
        app.replace_sessions(vec![
            session_at("alpha", Some("/work/a")),
            session_at("bravo", Some("/work/b")),
            session_at("charlie", Some("/home/c")),
        ]);
        app.toggle_view_mode();
        let previous_selected = app.selected;
        let total = app.tree.visible.len();
        assert!(total > 1, "tree needs at least two rows for this test");

        // Place the cursor on the last row, then press Down. The cursor must
        // wrap around to the top, and the flat-list `selected` field must
        // stay untouched while we're in tree mode.
        app.tree.cursor = total - 1;
        route_key(&mut app, key(KeyCode::Down), None);
        assert_eq!(
            app.tree.cursor, 0,
            "Down from last row wraps to top, got {}",
            app.tree.cursor
        );
        assert_eq!(
            app.selected, previous_selected,
            "list-mode cursor must not move while in tree mode"
        );

        // Now from the top, Up must wrap to the bottom.
        app.tree.cursor = 0;
        route_key(&mut app, key(KeyCode::Up), None);
        assert_eq!(app.tree.cursor, total - 1, "Up from row 0 wraps to last");
        assert_eq!(app.selected, previous_selected);

        // Round-trip: a normal Down in the middle of the list moves one
        // step without touching `selected`.
        app.tree.cursor = 1;
        route_key(&mut app, key(KeyCode::Down), None);
        assert_eq!(app.tree.cursor, 2);
        assert_eq!(app.selected, previous_selected);

        // Switching back to list mode reverts Up/Down to the flat-list
        // cursor; the tree cursor stays put.
        app.toggle_view_mode();
        let tree_cursor = app.tree.cursor;
        route_key(&mut app, key(KeyCode::Down), None);
        assert_eq!(app.tree.cursor, tree_cursor);
        assert_ne!(
            app.selected, previous_selected,
            "list-mode Down must move the flat-list selected cursor"
        );
    }

    #[test]
    fn tree_refresh_tick_preserves_focused_folder() {
        // Regression: every `replace_sessions` round-trip (which happens on
        // every daemon refresh tick) calls `rebuild_tree`, and the rebuild
        // used to clobber `tree.cursor` and snap it to the last row when
        // the user's focus was on a folder row. The fix is to remember the
        // folder path so the rebuild can re-locate the same logical
        // position. This test exercises the full refresh path — the same
        // one used by the input loop.
        let mut app = App::default();
        app.replace_sessions(vec![
            session_at("alpha", Some("/work/a")),
            session_at("bravo", Some("/work/b")),
            session_at("charlie", Some("/home/c")),
        ]);
        app.toggle_view_mode();

        // Find the depth-1 `home` folder row and place the cursor there.
        let home_index = app
            .tree
            .visible
            .iter()
            .position(|entry| match entry {
                super::TreeEntry::Folder { node, .. } => {
                    app.tree.nodes[*node].path == std::path::Path::new("home")
                }
                _ => false,
            })
            .expect("home folder should appear");
        app.tree.cursor = home_index;

        // Refresh cycle — `replace_sessions` re-runs `rebuild_tree`.
        app.replace_sessions(vec![
            session_at("alpha", Some("/work/a")),
            session_at("bravo", Some("/work/b")),
            session_at("charlie", Some("/home/c")),
        ]);

        // Cursor must snap back to the same folder row, not to the last
        // row. Without the folder-path-restoration fix this would be
        // `app.tree.visible.len() - 1`.
        let still_home = app
            .tree
            .visible
            .get(app.tree.cursor)
            .copied()
            .map(|entry| match entry {
                super::TreeEntry::Folder { node, .. } => {
                    app.tree.nodes[node].path == std::path::Path::new("home")
                }
                _ => false,
            })
            .unwrap_or(false);
        assert!(
            still_home,
            "cursor should still be on the `home` folder after refresh, \
             got visible[{}] = {:?}",
            app.tree.cursor,
            app.tree.visible.get(app.tree.cursor),
        );
    }

    #[test]
    fn tree_walks_through_empty_middleman_folders() {
        let mut app = App::default();
        // `/a/b/c/something` has three empty middleman folders. With the
        // default auto-depth horizon of 2, every prefix folder leading up to
        // the depth-4 leaf is visible as a navigation row even though none
        // of them holds a session directly — the leaf folder and its
        // session only become visible after drilling.
        app.replace_sessions(vec![session_at("lint", Some("/a/b/c/something"))]);
        let ids = tree_visible_ids(&app);
        // Middleman folders are emitted as rows so the user can keep
        // navigating through them.
        assert!(ids.iter().any(|id| id == "folder:a"), "ids={ids:?}");
        let has_folder = |path: &str| {
            ids.iter().any(|id| {
                std::path::Path::new(id.strip_prefix("folder:").unwrap_or(""))
                    == std::path::Path::new(path)
            })
        };
        assert!(has_folder("a/b"), "ids={ids:?}");
        // With auto-depth=2, the depth-3 folder hides until drilled.
        assert!(
            !has_folder("a/b/c"),
            "depth-3 folder should sit past auto-depth: {ids:?}"
        );
        // The depth-4 leaf folder hides until drilled on `a/b/c`.
        assert!(
            !has_folder("a/b/c/something"),
            "leaf folder should sit past auto-depth, ids={ids:?}"
        );
        assert!(
            !ids.contains(&"lint".to_string()),
            "leaf session should sit past auto-depth, ids={ids:?}"
        );
    }

    #[test]
    fn tree_enter_on_session_emits_open_inline() {
        let mut app = App::default();
        app.replace_sessions(vec![session_at("only", Some("/p"))]);
        app.toggle_view_mode();
        // Cursor lands on the only session.
        let action = route_key(&mut app, key(KeyCode::Enter), None);
        assert_eq!(action, AppAction::OpenInline);
    }

    #[test]
    fn tree_enter_on_deep_folder_toggles_drill() {
        let mut app = App::default();
        // Build a tree where a leaf at depth 5 needs drilling. We drill
        // the path by hand so the auto-drill on toggle doesn't pre-expose
        // `deep`; the assertions below exercise the user-driven drill.
        app.replace_sessions(vec![session_at("deep", Some("/a/b/c/d/e"))]);
        app.toggle_view_mode();
        // Clear any drills the auto-focus path may have applied so we
        // start from the un-drilled default state.
        app.tree.drilled.clear();
        app.rebuild_tree();
        // Before drilling, `deep` and its tail folders at depth > 2 are
        // hidden. Drill on a depth-2 ancestor should expose them.
        let ids_before = tree_visible_ids(&app);
        assert!(
            !ids_before.contains(&"deep".to_string()),
            "deep session should be hidden before drilling: {ids_before:?}"
        );

        // Find the depth-2 folder (`a/b`) by walking visible rows. The
        // walker emits folder rows at their chain depth; drilling the
        // outermost visible folder should expose the leaf session past the
        // horizon.
        let target_position = app
            .tree
            .visible
            .iter()
            .position(|entry| match entry {
                super::TreeEntry::Folder { node, depth } => {
                    *depth == 2 && app.tree.nodes[*node].path == std::path::Path::new("a/b")
                }
                _ => false,
            })
            .expect("depth-2 folder present");
        app.tree.cursor = target_position;
        let action = route_key(&mut app, key(KeyCode::Enter), None);
        assert_eq!(action, AppAction::None);
        let ids_after = tree_visible_ids(&app);
        assert!(
            ids_after.contains(&"deep".to_string()),
            "deep session should be visible after drilling: {ids_after:?}"
        );
    }

    #[test]
    fn tree_navigation_wraps_around_visible_rows() {
        let mut app = App::default();
        let mut sessions: Vec<SessionSummary> = (0..6)
            .map(|i| session_at(&format!("s{i}"), Some("/x")))
            .collect();
        sessions[0].cwd = Some("/x".to_string());
        // Spread the rest across multiple leaves so the tree has folder rows.
        for (i, s) in sessions.iter_mut().enumerate().take(6).skip(1) {
            s.cwd = Some(format!("/x/leaf{}", i));
        }
        app.replace_sessions(sessions);
        app.toggle_view_mode();
        let total = app.tree.visible.len();
        assert!(total > 1, "test needs at least two visible rows");
        app.tree.cursor = 0;
        // Move down past the end — cursor must wrap around to ~start.
        for _ in 0..total + 5 {
            app.navigate_tree(1);
        }
        // After `total` steps we'd land back at cursor 0 with the offset
        // applied five times more. Wraps via rem_euclid so the cursor is
        // deterministic regardless of how many extra presses we pretend.
        assert_eq!(app.tree.cursor, 5usize.rem_euclid(total));
        // Pressing Up wraps the same way.
        for _ in 0..total + 7 {
            app.navigate_tree(-1);
        }
        assert_eq!(
            app.tree.cursor,
            ((5isize) - 7).rem_euclid(total as isize) as usize
        );
    }

    #[test]
    fn tree_enter_on_session_uses_cursor_not_stale_selected() {
        // Regression: `route_key(Enter)` in tree mode used to delegate to
        // `app.selected_session()`, which checks `self.visible.contains
        // (&self.selected)`. In tree mode navigation only updates
        // `tree.cursor`, so a tree-only session (one the user navigated
        // to in tree mode before ever stepping on it in list mode)
        // silently failed to open because `self.selected` stayed
        // pointed at an unrelated session. The fix is `focused_session`,
        // which reads `tree.cursor` in tree mode.
        let mut app = App::default();
        let mut alpha = session_at("alpha", Some("/work/a"));
        alpha.command = "ls".into();
        let mut bravo = session_at("bravo", Some("/work/b"));
        bravo.command = "vim".into();
        app.replace_sessions(vec![alpha, bravo]);

        // Stay in tree mode for the whole test.
        app.toggle_view_mode();
        // The tree view shows both sessions grouped under `work/`.
        assert!(!app.tree.visible.is_empty());

        // Place the cursor on the `vim` session, *without* ever syncing
        // `self.selected` to that row first.
        let target = app
            .tree
            .visible
            .iter()
            .position(|entry| {
                matches!(entry, super::TreeEntry::Session { session, .. }
                    if app.sessions[*session].command == "vim")
            })
            .expect("vim should appear in tree");
        app.tree.cursor = target;
        // Make `self.selected` stale on purpose to prove Enter reads the
        // tree cursor, not `self.selected`.
        app.selected = 0;
        let action = route_key(&mut app, key(KeyCode::Enter), None);
        assert_eq!(
            action,
            AppAction::OpenInline,
            "Enter must open the tree-cursor session even when `self.selected` is stale"
        );
        assert_eq!(app.sessions[app.selected].command, "vim");
    }

    #[test]
    fn ctrl_d_in_tree_mode_uses_tree_cursor_session() {
        // Regression: Ctrl+D used to call `app.selected_session()`, which
        // reads `self.selected`. In tree mode, navigation only writes to
        // `tree.cursor`, so without `focused_session()` the dialog opened
        // pre-filled with whatever session was last visited in list mode
        // — typically an entirely different row than the one the user is
        // staring at.
        let mut app = App::default();
        let mut ls = session_at("ls", Some("/work/a"));
        ls.command = "bash".into();
        let mut vim = session_at("vim", Some("/work/b"));
        vim.command = "vim".into();
        app.replace_sessions(vec![ls, vim]);
        app.toggle_view_mode();
        // Put the cursor on the second session row.
        let vim_row = app
            .tree
            .visible
            .iter()
            .position(|entry| {
                matches!(entry, super::TreeEntry::Session { session, .. }
                    if app.sessions[*session].command == "vim")
            })
            .expect("vim should appear in tree");
        app.tree.cursor = vim_row;

        route_key(&mut app, ctrl(KeyCode::Char('d')), None);
        let dialog = app
            .clone_dialog
            .as_ref()
            .expect("Ctrl+D should open the clone dialog");
        assert_eq!(dialog.command.value.as_str(), "vim");
        assert_eq!(dialog.source_id.as_deref(), Some("vim"));
    }

    #[test]
    fn tree_filter_sessions_by_search_text() {
        // The tree honors the search filter: a non-matching filter
        // should collapse every folder that has no surviving session in
        // its subtree, while a partially-matching filter keeps the
        // path to the surviving session.
        let mut app = App::default();
        let mut lint = session_at("s1", Some("/proj/lint-target"));
        lint.command = "lint".into();
        let mut build = session_at("s2", Some("/proj/build-target"));
        build.command = "build".into();
        app.replace_sessions(vec![lint, build]);
        app.toggle_view_mode();
        let pre_filter_session_count = app
            .tree
            .visible
            .iter()
            .filter(|e| matches!(e, super::TreeEntry::Session { .. }))
            .count();
        assert_eq!(pre_filter_session_count, 2);

        // Apply a filter that matches nothing.
        app.normalized_filter = "no-such-thing".into();
        app.rebuild_visible();
        let post_filter_session_count = app
            .tree
            .visible
            .iter()
            .filter(|e| matches!(e, super::TreeEntry::Session { .. }))
            .count();
        assert_eq!(post_filter_session_count, 0);
        assert_eq!(
            app.tree
                .visible
                .iter()
                .filter(|e| matches!(e, super::TreeEntry::Folder { .. }))
                .count(),
            0,
            "no folders should be visible when filter hides every host session"
        );

        // Apply a filter that matches exactly one session — the path to
        // that session must remain visible.
        app.normalized_filter = "build".into();
        app.rebuild_visible();
        let partial_session_count = app
            .tree
            .visible
            .iter()
            .filter(|e| matches!(e, super::TreeEntry::Session { .. }))
            .count();
        assert_eq!(partial_session_count, 1);
        let partial_folder_count = app
            .tree
            .visible
            .iter()
            .filter(|e| matches!(e, super::TreeEntry::Folder { .. }))
            .count();
        assert!(
            partial_folder_count >= 2,
            "matching path's folder chain (proj/, proj/build-target) should be visible"
        );
    }

    #[test]
    fn tree_groups_by_node_before_cwd_and_keeps_identical_paths_separate() {
        let mut app = App::default();
        let mut remote = session_at("remote", Some("/work/app"));
        remote.node = Some("worker".into());
        let local = session_at("local", Some("/work/app"));
        app.replace_sessions(vec![remote, local]);
        let ids = tree_visible_ids(&app);
        assert_eq!(app.tree.nodes[app.tree.root].subfolders.len(), 2);
        let local_pos = ids.iter().position(|id| id == "local").unwrap();
        let remote_pos = ids.iter().position(|id| id == "remote").unwrap();
        assert!(
            local_pos < remote_pos,
            "node branches must not mix: {ids:?}"
        );
        app.toggle_view_mode();
        let rendered = render_app(&mut app, 120, 30);
        assert!(rendered.contains("local (node)"));
        assert!(rendered.contains("worker (node)"));
    }

    #[test]
    fn tree_keeps_shared_cwd_visible_when_a_session_starts_there() {
        let mut app = App::default();
        app.replace_sessions(vec![
            session_at("parent", Some("/work/app")),
            session_at("child", Some("/work/app/sub")),
        ]);
        let folder = app
            .tree
            .visible
            .iter()
            .find_map(|entry| match entry {
                super::TreeEntry::Folder { node, .. }
                    if app.tree.nodes[*node].cwd.as_deref() == Some("/work/app") =>
                {
                    Some(*node)
                }
                _ => None,
            })
            .expect("shared cwd must have its own folder");
        assert_eq!(app.tree.nodes[folder].name, "app");
        assert!(app.tree.nodes[folder].direct_sessions.contains(&0));
    }

    #[test]
    fn drilling_a_remote_path_does_not_open_the_same_local_path() {
        let mut app = App::default();
        let local = session_at("local", Some("/a/b/c/d/e"));
        let mut remote = session_at("remote", Some("/a/b/c/d/e"));
        remote.node = Some("worker".into());
        app.replace_sessions(vec![local, remote]);
        let target = app
            .tree
            .visible
            .iter()
            .position(|entry| {
                matches!(entry,
            super::TreeEntry::Folder { node, depth: 3 }
                if app.tree.nodes[*node].node.as_deref() == Some("worker"))
            })
            .expect("remote folder at drill horizon");
        app.tree.cursor = target;
        app.toggle_tree_drill();
        assert!(app.tree.visible.iter().any(|entry| matches!(entry,
            super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "remote")));
        assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
            super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "local")));
    }
    #[test]
    fn tree_draws_sibling_edges_and_continuing_ancestor_lines() {
        let mut app = App::default();
        let mut a = session_at("a", Some("/work/config"));
        a.command = "project.yaml".into();
        let mut b = session_at("b", Some("/work/config"));
        b.command = "constraints.sdc".into();
        let mut c = session_at("c", Some("/work/docs"));
        c.command = "README.md".into();
        app.replace_sessions(vec![a, b, c]);
        app.toggle_view_mode();
        let rendered = render_app(&mut app, 120, 30);
        assert!(rendered.contains("├── config/"), "{rendered}");
        assert!(rendered.contains("│   ├──"), "{rendered}");
        assert!(rendered.contains("│   └──"), "{rendered}");
        assert!(rendered.contains("└── docs/"), "{rendered}");
        assert!(
            !rendered.contains("(2)"),
            "folder counts are redundant: {rendered}"
        );
    }

    #[test]
    fn ctrl_n_on_tree_folder_prefills_full_cwd_and_owning_node() {
        let mut app = App::default();
        let mut remote = session_at("remote", Some("/work/app/sub"));
        remote.node = Some("worker".into());
        let local = session_at("local", Some("/home/local"));
        app.replace_sessions(vec![remote, local]);
        app.toggle_view_mode();
        app.tree.cursor = app
            .tree
            .visible
            .iter()
            .position(|entry| {
                matches!(entry, super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].cwd.as_deref() == Some("/work/app"))
            })
            .expect("remote parent folder");
        route_key(&mut app, ctrl(KeyCode::Char('n')), None);
        let dialog = app.clone_dialog.as_ref().unwrap();
        assert_eq!(dialog.cwd.value, "/work/app");
        assert_eq!(dialog.node.value, "worker");
        assert!(dialog.command.value.is_empty());
    }

    #[test]
    fn tree_start_time_uses_relative_then_local_datetime() {
        let now = Utc.with_ymd_and_hms(2026, 9, 23, 12, 0, 0).unwrap();
        assert_eq!(
            super::format_tree_start(now - chrono::Duration::minutes(1), now),
            "1m ago"
        );
        assert_eq!(
            super::format_tree_start(now - chrono::Duration::hours(23), now),
            "23h ago"
        );
        let old = now - chrono::Duration::hours(24);
        assert_eq!(
            super::format_tree_start(old, now),
            super::super::list::format_timestamp_local(old)
        );
        assert_eq!(
            super::format_tree_start(now + chrono::Duration::minutes(1), now),
            "just now"
        );
    }

    #[test]
    fn tree_session_uses_fixed_status_width_and_normal_sparkline() {
        let mut item = session_at("live", Some("/work"));
        item.title = Some("release".into());
        let now = std::time::Instant::now();
        let mut rate = super::RateState::new(&item, now);
        rate.history.extend([4.0, 8.0, 2.0]);
        let line = super::session_line(&item, Some(&rate), "├── ", false, now);
        assert_eq!(line.spans[3].content.len(), super::TREE_STATUS_WIDTH);
        assert_eq!(
            line.spans[9].content.as_ref(),
            super::sparkline(Some(&rate), super::SPARKLINE_WIDTH)
        );
        assert!(!line.spans.iter().any(|span| span.content.contains("/work")));
        item.input_needed = true;
        let attention = super::session_line(&item, Some(&rate), "├── ", true, now);
        assert_eq!(attention.spans[3].content.len(), super::TREE_STATUS_WIDTH);
        assert_eq!(
            attention.spans[3].style.fg,
            Some(ratatui::style::Color::Yellow)
        );
    }
    #[test]
    fn tree_render_shows_status_word_with_matching_color() {
        // Regression: the tree view used to skip the status text entirely,
        // so users could only guess at session state from the icon. Worse,
        // when the status text DID appear, it picked an independent colour
        // from the icon, so the colour and the label could disagree.
        let mut app = App::default();
        let mut s = session_at("only", Some("/a"));
        s.command = "test".into();
        s.status = "running".into();
        app.replace_sessions(vec![s]);
        app.toggle_view_mode();
        let buffer = render_app_buffer(&mut app, 120, 6);
        let symbols = buffer_symbols(&buffer, 4);
        // The status label "running" must appear on the same line as the
        // session command.
        let row_hit = symbols
            .iter()
            .any(|line| line.contains("running") && line.contains("test"));
        assert!(
            row_hit,
            "tree row must contain both the status label and the command. Got: {symbols:?}"
        );
        // Status colour and label must come from the same display cell:
        // glyph at the icon column and the word "running" must share the
        // same foreground colour when the session is active.
        let cell_at = |row: u16, col: u16| -> ratatui::style::Color {
            buffer.cell((col, row)).map(|c| c.fg).unwrap_or_default()
        };
        // Find the row containing the session and the icon column (the
        // first non-space glyph). Use a rough heuristic: the first row
        // containing "test" is our session row.
        let (session_row, _) = symbols
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains("test"))
            .expect("session row present");
        let icon_x = (0..buffer.area().width)
            .find(|&x| buffer[(x, session_row as u16)].symbol() == "●")
            .expect("running glyph");
        let icon_color = cell_at(session_row as u16, icon_x);
        let word_color = cell_at(session_row as u16, icon_x + 3);
        assert_eq!(
            icon_color, word_color,
            "icon and status word must share foreground color"
        );
    }

    #[test]
    fn tree_orders_folders_then_sessions_alphabetically() {
        let mut app = App::default();
        // Five sessions split across two sibling folders under the same
        // cwd-rooted parent. `session_at` only sets `id`+`cwd`, so we
        // patch each session's `command` manually so the sort label has
        // a unique alphabetic key per session.
        let mut sessions = vec![
            session_at("zeta", Some("/proj/zeta-target")),
            session_at("alpha", Some("/proj/zeta-target")),
            session_at("apple", Some("/other-apple-leaves")),
            session_at("mango", Some("/proj/zeta-target")),
            session_at("banana", Some("/other-apple-leaves")),
        ];
        let commands = ["zeta", "alpha", "apple", "mango", "banana"];
        for (session, cmd) in sessions.iter_mut().zip(commands.iter()) {
            session.command = (*cmd).to_string();
        }
        app.replace_sessions(sessions);
        app.toggle_view_mode();

        // Capture each visible row's display label. Folder rows use the
        // folder's `name` (the last basename); session rows use the
        // session's command — which is also the sort key, so the order
        // of labels in the flat list reflects the sorted order directly.
        let path_labels: Vec<String> = app
            .tree
            .nodes
            .iter()
            .map(|node| node.name.clone())
            .collect();
        let mut sequence: Vec<&str> = Vec::new();
        for entry in &app.tree.visible {
            match entry {
                super::TreeEntry::Folder { node, .. } => {
                    sequence.push(path_labels[*node].as_str());
                }
                super::TreeEntry::Session { session, .. } => {
                    sequence.push(app.sessions[*session].command.as_str());
                }
            }
        }

        // Verify folder-before-session ordering per parent: every session
        // row must come after its enclosing folder row, and within each
        // parent, sessions must be alphabetically sorted. The walker also
        // sorts folder names alphabetically across siblings.
        let other_idx = sequence
            .iter()
            .position(|label| *label == "other-apple-leaves")
            .expect("other-apple-leaves folder should appear");
        let proj_idx = sequence
            .iter()
            .position(|label| *label == "proj")
            .expect("proj folder should appear");
        let proj_target_idx = sequence
            .iter()
            .position(|label| *label == "zeta-target")
            .expect("zeta-target folder should appear");
        let apple_idx = sequence.iter().position(|l| *l == "apple").unwrap();
        let banana_idx = sequence.iter().position(|l| *l == "banana").unwrap();
        let alpha_idx = sequence.iter().position(|l| *l == "alpha").unwrap();
        let mango_idx = sequence.iter().position(|l| *l == "mango").unwrap();
        let zeta_idx = sequence.iter().position(|l| *l == "zeta").unwrap();

        // Depth-1 folders sort alphabetically: `o` < `p`.
        assert!(
            other_idx < proj_idx,
            "folders must be sorted alphabetically: {sequence:?}"
        );

        // The `zeta-target` leaf folder sits under `proj`, so it appears
        // after the parent folder.
        assert!(
            proj_idx < proj_target_idx,
            "leaf folder must follow its parent: {sequence:?}"
        );

        // Inside each folder, sessions follow the folder row and are
        // themselves alphabetically ordered.
        assert!(other_idx < apple_idx);
        assert!(other_idx < banana_idx);
        assert!(apple_idx < banana_idx);
        assert!(proj_target_idx < alpha_idx);
        assert!(proj_target_idx < mango_idx);
        assert!(proj_target_idx < zeta_idx);
        assert!(alpha_idx < mango_idx);
        assert!(mango_idx < zeta_idx);
    }

    #[test]
    fn tree_render_emits_no_table_widget() {
        // Smoke-test: the tree render path produces a buffer whose symbol
        // stream contains status glyphs without crashing or panicking.
        let mut app = App::default();
        let mut sessions = vec![
            session_at("ls", Some("/a/b")),
            session_at("vim", Some("/a/b/c")),
            session_at("git", Some("/home/me")),
        ];
        sessions[0].input_needed = true;
        app.replace_sessions(sessions);
        app.toggle_view_mode();
        let buffer = render_app_buffer(&mut app, 120, 24);
        let area = *buffer.area();
        let mut saw_attention = false;
        let mut saw_running = false;
        for x in 0..area.width {
            for y in 0..area.height {
                match buffer[(x, y)].symbol() {
                    "◆" => saw_attention = true,
                    "●" => saw_running = true,
                    _ => {}
                }
            }
        }
        assert!(
            saw_attention,
            "tree view should still render the attention glyph"
        );
        assert!(saw_running, "tree view should render running session glyph");
    }

    #[test]
    fn common_path_prefix_strips_shared_ancestor() {
        let paths = vec![
            std::path::PathBuf::from("/a/b/c"),
            std::path::PathBuf::from("/a/b/d/e"),
            std::path::PathBuf::from("/a/b"),
        ];
        assert_eq!(
            super::common_path_prefix(&paths),
            std::path::PathBuf::from("/a")
        );
    }

    #[test]
    fn common_path_prefix_returns_root_when_no_ancestor() {
        // No component beyond `/` is shared. The prefix is still absolute so
        // callers can pass it straight to `Path::strip_prefix` and receive
        // relative paths (verified indirectly by `tree_groups_sessions_*`).
        let paths = vec![
            std::path::PathBuf::from("/x"),
            std::path::PathBuf::from("/y/z"),
        ];
        assert_eq!(
            super::common_path_prefix(&paths),
            std::path::PathBuf::from("/")
        );
    }

    #[test]
    fn posting_a_message_registers_a_fade_in_once() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        app.message = Some("stop signal sent to source".to_string());

        let _ = render_app(&mut app, 120, 12);
        assert!(app.effects.is_running());
        assert_eq!(
            app.rendered_message.as_deref(),
            Some("stop signal sent to source")
        );
    }

    #[test]
    fn opening_a_dialog_registers_a_fade_in() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        let _ = render_app(&mut app, 120, 30);
        assert!(!app.effects.is_running());
        assert_eq!(app.rendered_dialog, None);

        route_key(&mut app, ctrl(KeyCode::Char('d')), None);
        let _ = render_app(&mut app, 120, 30);
        assert_eq!(app.rendered_dialog, Some("clone-fade"));
        assert!(app.effects.is_running());
    }

    #[test]
    fn footer_keeps_help_visible_alongside_a_message() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        app.message = Some("stop signal sent to source".to_string());

        let rendered = render_app(&mut app, 120, 12);
        let footer = rendered.lines().last().unwrap_or_default().to_string();

        // The warning no longer replaces the key hints — both are visible.
        assert!(footer.contains("stop signal sent to source"));
        assert!(footer.contains("^N new"));
    }

    #[test]
    fn wide_mode_columns_follow_the_same_order_as_narrow_modes() {
        let mut app = App::default();
        let mut item = session("wide1234");
        item.title = Some("ordered".to_string());
        app.replace_sessions(vec![item]);

        let rendered = render_app(&mut app, 120, 12);
        let header = rendered
            .lines()
            .find(|line| line.contains("COMMAND"))
            .expect("wide header rendered");
        let position = |label: &str| header.find(label).expect("column header present");

        // status, ID, SESSION, PID, STATE, AGE, RATE, OUTPUT, COMMAND
        assert!(position("ID") < position("SESSION"));
        assert!(position("SESSION") < position("PID"));
        assert!(position("PID") < position("STATE"));
        assert!(position("STATE") < position("AGE"));
        assert!(position("AGE") < position("RATE"));
        assert!(position("RATE") < position("OUTPUT"));
        assert!(position("OUTPUT") < position("COMMAND"));
    }

    fn render_app(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        (area.y..area.bottom())
            .map(|y| {
                let mut line = (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>();
                line.push('\n');
                line
            })
            .collect()
    }

    #[test]
    fn ctrl_d_opens_complete_prefilled_clone_dialog() {
        let mut app = App::default();
        let mut source = session("source");
        source.title = Some("Agent review".to_string());
        source.tags = vec!["review".to_string(), "night shift".to_string()];
        source.command = "copilot".to_string();
        source.args = vec!["--model".to_string(), "gpt 5".to_string()];
        source.cwd = Some("D:\\work tree".to_string());
        source.notifications_enabled = true;
        source.node = Some("worker-a".to_string());
        source.rows = Some(42);
        source.cols = Some(132);
        app.replace_sessions(vec![source]);

        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('d')), None),
            AppAction::None
        );
        let dialog = app.clone_dialog.as_ref().unwrap();
        assert_eq!(dialog.source_id.as_deref(), Some("source"));
        assert_eq!(dialog.command.value, "copilot");
        assert_eq!(dialog.args.value, r#"--model "gpt 5""#);
        assert_eq!(dialog.cwd.value, "D:\\work tree");
        assert_eq!(dialog.title.value, "Agent review");
        assert_eq!(dialog.tags.value, r#"review "night shift""#);
        assert_eq!(dialog.node.value, "worker-a");
        assert_eq!(dialog.rows.value, "42");
        assert_eq!(dialog.cols.value, "132");
        assert!(!dialog.disable_notifications);
        assert!(!dialog.attach_after_start);
    }

    #[test]
    fn ctrl_n_opens_blank_new_session_dialog_scoped_to_the_viewed_node() {
        let mut app = App::default();
        let mut source = session("source");
        source.title = Some("must not copy".to_string());
        source.node = Some("worker-a".to_string());
        app.replace_sessions(vec![source]);

        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('n')), Some("list-node")),
            AppAction::None
        );
        let dialog = app.clone_dialog.as_ref().unwrap();
        assert!(dialog.source_id.is_none());
        assert!(dialog.command.value.is_empty());
        assert!(dialog.args.value.is_empty());
        assert!(dialog.cwd.value.is_empty());
        assert!(dialog.title.value.is_empty());
        assert!(dialog.tags.value.is_empty());
        // The node field is prefilled from the node the list is scoped to —
        // never copied from the selected session — so the new session lands
        // where the user is looking instead of silently going local.
        assert_eq!(dialog.node.value, "list-node");
        assert!(dialog.rows.value.is_empty());
        assert!(dialog.cols.value.is_empty());
        assert!(!dialog.disable_notifications);
        assert!(!dialog.attach_after_start);

        let rendered = render_app(&mut app, 120, 30);
        assert!(rendered.contains("New Session"));
        assert!(!rendered.contains("Duplicate source"));
    }

    #[test]
    fn ctrl_n_without_node_scope_stays_completely_blank() {
        let mut app = App::default();
        let mut source = session("source");
        source.node = Some("worker-a".to_string());
        app.replace_sessions(vec![source]);

        route_key(&mut app, ctrl(KeyCode::Char('n')), None);
        let dialog = app.clone_dialog.as_ref().unwrap();
        assert!(dialog.node.value.is_empty());
    }

    #[test]
    fn new_session_enter_launches_on_the_viewed_node() {
        let mut app = App::default();
        route_key(&mut app, ctrl(KeyCode::Char('n')), Some("worker-a"));
        for character in "bash".chars() {
            route_key(&mut app, key(KeyCode::Char(character)), Some("worker-a"));
        }
        let AppAction::Start(launch) = route_key(&mut app, key(KeyCode::Enter), Some("worker-a"))
        else {
            panic!("enter must launch the new session dialog");
        };
        assert_eq!(launch.node.as_deref(), Some("worker-a"));
        assert_eq!(launch.command, "bash");
        let request = launch.request();
        let RpcRequest::NodeProxy { node, inner } = request else {
            panic!("node-scoped launch must be wrapped in NodeProxy");
        };
        assert_eq!(node, "worker-a");
        assert!(matches!(*inner, RpcRequest::Start { .. }));
    }

    #[test]
    fn refresh_cycle_preserves_action_feedback_messages() {
        let mut app = App::default();

        // Refresh warnings show when no action feedback is pending.
        app.set_refresh_message(Some("sync lost: worker-a".to_string()));
        assert_eq!(app.message.as_deref(), Some("sync lost: worker-a"));
        // A later refresh replaces (or clears) its own message.
        app.set_refresh_message(None);
        assert_eq!(app.message, None);

        // Action feedback survives the 250 ms refresh cycle...
        app.set_action_message(Some("started new session abc1234".to_string()));
        app.set_refresh_message(None);
        assert_eq!(app.message.as_deref(), Some("started new session abc1234"));
        app.set_refresh_message(Some("sync lost: worker-a".to_string()));
        assert_eq!(app.message.as_deref(), Some("started new session abc1234"));

        // ...until the next action replaces it, after which refresh status
        // is allowed through again.
        app.set_action_message(None);
        app.set_refresh_message(Some("sync lost: worker-a".to_string()));
        assert_eq!(app.message.as_deref(), Some("sync lost: worker-a"));
    }

    #[test]
    fn clone_dialog_uses_sections_placeholders_and_focused_input_styles() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('n')), None);
        let _ = render_app_buffer(&mut app, 100, 30);
        // Let the dialog fade-in finish so style assertions see final colors.
        app.last_frame_at =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1000));
        let buffer = render_app_buffer(&mut app, 100, 30);
        let text: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("PROCESS"));
        assert!(text.contains("METADATA"));
        assert!(text.contains("OPTIONS"));
        assert!(text.contains("‹auto›"));
        assert!(text.contains("‹local›"));
        assert!(text.contains("╭"));

        // The active field row is marked with ▸, a cyan label, and an input-box bg.
        let marker = (5..buffer.area().height)
            .flat_map(|y| (0..buffer.area().width).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer[(x, y)].symbol() == "▸"
                    && buffer[(x + 2, y)].symbol() == "C"
                    && buffer[(x + 3, y)].symbol() == "o"
            })
            .expect("active Command field marker");
        assert_eq!(
            buffer[(marker.0 + 2, marker.1)].fg,
            ratatui::style::Color::Cyan
        );
        let value_x = marker.0 + 2 + super::DIALOG_LABEL_WIDTH as u16 + 2;
        assert_eq!(buffer[(value_x, marker.1)].bg, super::DIALOG_FIELD_BG);

        // Inactive checkboxes read [x]/[ ]; the enabled one is green.
        let checked = (0..buffer.area().height)
            .flat_map(|y| (0..buffer.area().width).map(move |x| (x, y)))
            .find(|&(x, y)| buffer[(x, y)].symbol() == "[" && buffer[(x + 1, y)].symbol() == "x")
            .expect("checked box");
        assert_eq!(buffer[checked].fg, ratatui::style::Color::Green);
    }

    #[test]
    fn ctrl_c_is_the_only_list_exit_and_ctrl_v_no_longer_clones() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('v')), None),
            AppAction::None
        );
        assert!(app.clone_dialog.is_none());
        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('c')), None),
            AppAction::Quit
        );
    }

    #[test]
    fn raw_ctrl_d_opens_clone_dialog() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        assert_eq!(
            route_key(&mut app, key(KeyCode::Char('\u{4}')), None),
            AppAction::None
        );
        assert!(app.clone_dialog.is_some());
    }

    #[test]
    fn raw_ctrl_n_opens_blank_new_session_dialog() {
        let mut app = App::default();

        assert_eq!(
            route_key(&mut app, key(KeyCode::Char('\u{e}')), None),
            AppAction::None
        );
        assert!(
            app.clone_dialog
                .as_ref()
                .is_some_and(|dialog| dialog.source_id.is_none())
        );
    }

    #[test]
    fn ctrl_u_opens_metadata_update_with_only_supported_fields_editable() {
        let mut app = App::default();
        let mut source = session("source");
        source.title = Some("Agent review".to_string());
        source.tags = vec!["review".to_string(), "night shift".to_string()];
        source.command = "copilot".to_string();
        source.args = vec!["--model".to_string(), "gpt 5".to_string()];
        source.cwd = Some("D:\\work tree".to_string());
        source.node = Some("worker-a".to_string());
        source.pid = Some(4242);
        source.notifications_enabled = true;
        source.started_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap());
        source.rows = Some(42);
        source.cols = Some(132);
        app.replace_sessions(vec![source]);

        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('u')), Some("list-node")),
            AppAction::None
        );
        let dialog = app.update_dialog.as_ref().unwrap();
        assert_eq!(dialog.target_id, "source");
        assert_eq!(dialog.target_node.as_deref(), Some("worker-a"));
        assert_eq!(dialog.title.value, "Agent review");
        assert_eq!(dialog.tags.value, r#"review "night shift""#);
        assert!(dialog.notifications_enabled);
        assert_eq!(
            super::UPDATE_FIELDS,
            [
                super::UpdateField::Title,
                super::UpdateField::Tags,
                super::UpdateField::Notifications,
            ]
        );

        let read_only = super::update_read_only_values(&dialog.summary);
        let labels = read_only
            .iter()
            .map(|(label, _)| *label)
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "ID",
                "State / PID",
                "Command",
                "Args",
                "Cwd",
                "Node",
                "Terminal",
                "Created",
                "Started",
                "Ended",
                "Runtime",
                "Output",
            ]
        );
        assert_eq!(dialog.summary.command, "copilot");
        assert_eq!(dialog.summary.pid, Some(4242));
        let editable_line = super::update_field_line(dialog, super::UpdateField::Title, 80, true);
        assert!(editable_line.to_string().contains("Title"));
        assert!(!editable_line.to_string().contains("editable"));
        assert_eq!(
            editable_line.spans[1].style.fg,
            Some(ratatui::style::Color::Cyan)
        );
        let read_only_line = super::update_read_only_line("ID", "source", 80);
        assert!(read_only_line.to_string().contains("ID"));
        assert!(!read_only_line.to_string().contains("read-only"));
        assert!(read_only_line.to_string().contains("source"));
        assert_eq!(
            read_only_line.spans[1].style.fg,
            Some(ratatui::style::Color::DarkGray)
        );
        assert!(app.clone_dialog.is_none());
    }

    #[test]
    fn update_dialog_formats_all_timestamps_locally_without_subseconds() {
        let mut summary = session("source");
        summary.created_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
            + chrono::Duration::milliseconds(123);
        summary.started_at = Some(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap()
                + chrono::Duration::milliseconds(456),
        );
        summary.ended_at = Some(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 2, 0).unwrap()
                + chrono::Duration::milliseconds(789),
        );
        summary.last_output_epoch = Some(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 30).unwrap()
                + chrono::Duration::milliseconds(987),
        );

        let values = super::update_read_only_values(&summary)
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let local_seconds = |timestamp: chrono::DateTime<Utc>| {
            timestamp
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };

        assert_eq!(values["Created"], local_seconds(summary.created_at));
        assert_eq!(
            values["Started"],
            local_seconds(summary.started_at.unwrap())
        );
        assert_eq!(values["Ended"], local_seconds(summary.ended_at.unwrap()));
        assert_eq!(
            values["Output"],
            format!(
                "0B · last {}",
                local_seconds(summary.last_output_epoch.unwrap())
            )
        );
        assert!(
            ["Created", "Started", "Ended", "Output"]
                .iter()
                .all(|label| !values[*label].contains('.'))
        );
    }

    #[test]
    fn update_dialog_uses_consistent_placeholder_for_absent_times() {
        let values = super::update_read_only_values(&session("source"))
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();

        assert_eq!(values["Started"], "—");
        assert_eq!(values["Ended"], "—");
        assert_eq!(values["Output"], "0B · last —");
    }

    #[test]
    fn ctrl_u_handles_empty_selection_and_raw_control_character() {
        let mut app = App::default();
        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('u')), None),
            AppAction::None
        );
        assert_eq!(
            app.message.as_deref(),
            Some("no session in focus to update")
        );

        app.replace_sessions(vec![session("source")]);
        assert_eq!(
            route_key(&mut app, key(KeyCode::Char('\u{15}')), None),
            AppAction::None
        );
        assert!(app.update_dialog.is_some());
    }

    #[test]
    fn update_dialog_navigation_and_cancel_are_isolated_from_list_actions() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        assert_eq!(
            app.update_dialog.as_ref().unwrap().active_field(),
            super::UpdateField::Title
        );

        assert_eq!(
            route_key(&mut app, key(KeyCode::Tab), None),
            AppAction::None
        );
        assert_eq!(
            app.update_dialog.as_ref().unwrap().active_field(),
            super::UpdateField::Tags
        );
        assert_eq!(
            route_key(&mut app, key(KeyCode::Tab), None),
            AppAction::None
        );
        assert_eq!(
            app.update_dialog.as_ref().unwrap().active_field(),
            super::UpdateField::Notifications
        );
        assert!(app.update_dialog.as_ref().unwrap().notifications_enabled);
        assert_eq!(
            route_key(&mut app, key(KeyCode::Char(' ')), None),
            AppAction::None
        );
        assert!(!app.update_dialog.as_ref().unwrap().notifications_enabled);
        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Tab), None),
            AppAction::None
        );
        assert_eq!(
            app.update_dialog.as_ref().unwrap().active_field(),
            super::UpdateField::Tags
        );

        assert_eq!(
            route_key(&mut app, key(KeyCode::Esc), None),
            AppAction::None
        );
        assert!(app.update_dialog.is_none());
        assert_eq!(app.message.as_deref(), Some("update cancelled"));
    }

    #[test]
    fn dialog_boolean_fields_use_box_indicators() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        let dialog = app.update_dialog.as_mut().unwrap();

        let enabled = super::update_field_line(dialog, super::UpdateField::Notifications, 80, true);
        assert!(enabled.to_string().contains("[x]"));
        assert!(!enabled.to_string().contains("enabled"));

        dialog.notifications_enabled = false;
        let disabled =
            super::update_field_line(dialog, super::UpdateField::Notifications, 80, true);
        assert!(disabled.to_string().contains("[ ]"));
        assert!(!disabled.to_string().contains("disabled"));

        assert_eq!(super::checkbox(true), "[x]");
        assert_eq!(super::checkbox(false), "[ ]");
    }

    #[test]
    fn dialog_help_is_concise_and_uses_standard_navigation_terms() {
        assert!(unicode_width::UnicodeWidthStr::width(super::CLONE_DIALOG_HELP) <= 94);
        assert!(unicode_width::UnicodeWidthStr::width(super::UPDATE_DIALOG_HELP) <= 108);
        assert!(super::CLONE_DIALOG_HELP.contains("Tab/Shift+Tab"));
        assert!(super::UPDATE_DIALOG_HELP.contains("Tab/Shift+Tab"));
        assert!(!super::CLONE_DIALOG_HELP.contains("Ctrl+Tab"));
        assert!(!super::UPDATE_DIALOG_HELP.contains("Ctrl+Tab"));
    }

    #[test]
    fn list_tips_are_compact_and_dialog_tips_keep_their_top_divider() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        // The list footer is a single borderless line so the table gets the
        // extra row; it must still carry the key hints.
        let list = render_app(&mut app, 120, 30);
        let last_line = list.lines().last().unwrap_or_default();
        assert!(last_line.contains("^D duplicate"));
        assert!(!last_line.contains('\u{2500}'));

        route_key(&mut app, ctrl(KeyCode::Char('d')), None);
        let dialog = render_app(&mut app, 120, 30);
        let dialog_lines = dialog.lines().collect::<Vec<_>>();
        let help_index = dialog_lines
            .iter()
            .position(|line| line.contains("Enter create"))
            .unwrap();
        assert!(dialog_lines[help_index - 1].contains('\u{2500}'));
    }

    #[test]
    fn clone_and_update_dialogs_render_native_shadows() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        route_key(&mut app, ctrl(KeyCode::Char('d')), None);
        let clone = render_app(&mut app, 120, 30);
        assert!(clone.contains("Duplicate source"));
        assert!(clone.contains('▓'));

        route_key(&mut app, key(KeyCode::Esc), None);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        let update = render_app(&mut app, 120, 30);
        assert!(update.contains("Update source"));
        assert!(update.contains('▓'));
    }

    #[test]
    fn update_dialog_validates_title_and_tag_input() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);

        app.update_dialog.as_mut().unwrap().title =
            super::EditText::new("x".repeat(crate::session::MAX_SESSION_TITLE_LEN + 1));
        assert_eq!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::None
        );
        assert_eq!(
            app.update_dialog.as_ref().unwrap().error.as_deref(),
            Some("session title is too long (max 256 characters)")
        );

        let dialog = app.update_dialog.as_mut().unwrap();
        dialog.title = super::EditText::new("valid".to_string());
        dialog.tags = super::EditText::new("alpha \"unfinished".to_string());
        assert_eq!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::None
        );
        assert_eq!(
            app.update_dialog.as_ref().unwrap().error.as_deref(),
            Some("tags has an unclosed quote")
        );
    }

    #[test]
    fn update_submission_omits_unchanged_fields_and_routes_to_session_node() {
        let mut app = App::default();
        let mut source = session("source");
        source.title = Some("Current title".to_string());
        source.tags = vec!["alpha".to_string(), "night shift".to_string()];
        source.node = Some("worker-a".to_string());
        app.replace_sessions(vec![source]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), Some("list-node"));

        let expected = super::SessionUpdate {
            id: "source".to_string(),
            node: Some("worker-a".to_string()),
            title: None,
            tags: None,
            notifications_enabled: None,
        };
        assert_eq!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::Update(expected)
        );

        let dialog = app.update_dialog.as_mut().unwrap();
        dialog.title = super::EditText::new(String::new());
        dialog.tags = super::EditText::new(r#"beta "two words""#.to_string());
        dialog.notifications_enabled = false;
        let AppAction::Update(update) = route_key(&mut app, key(KeyCode::Enter), None) else {
            panic!("expected update action");
        };
        assert_eq!(update.title.as_deref(), Some(""));
        assert_eq!(
            update.tags,
            Some(vec!["beta".to_string(), "two words".to_string()])
        );
        assert_eq!(update.notifications_enabled, Some(false));
        match update.request() {
            RpcRequest::NodeProxy { node, inner } => {
                assert_eq!(node, "worker-a");
                assert!(matches!(
                    *inner,
                    RpcRequest::SessionMetadataSet {
                        ref id,
                        title: Some(ref title),
                        tags: Some(ref tags),
                        notifications_enabled: Some(false),
                    } if id == "source" && title.is_empty() && tags == &["beta", "two words"]
                ));
            }
            other => panic!("unexpected request: {}", other.name()),
        }
    }

    #[test]
    fn update_dialog_tracks_stop_and_blocks_disappeared_session() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        app.update_dialog.as_mut().unwrap().title = super::EditText::new("draft title".to_string());

        let mut stopped = session("source");
        stopped.status = "stopped".to_string();
        stopped.ended_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 2, 0).unwrap());
        app.replace_sessions(vec![stopped]);
        let dialog = app.update_dialog.as_ref().unwrap();
        assert!(dialog.available);
        assert_eq!(dialog.summary.status, "stopped");
        assert_eq!(dialog.title.value, "draft title");
        assert!(matches!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::Update(_)
        ));

        app.replace_sessions(Vec::new());
        assert!(!app.update_dialog.as_ref().unwrap().available);
        assert_eq!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::None
        );
        assert_eq!(
            app.update_dialog.as_ref().unwrap().error.as_deref(),
            Some("session source is no longer available in the current list")
        );
    }

    #[test]
    fn successful_update_refreshes_row_and_keeps_follow_tui_active() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source"), session("other")]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);

        let mut updated = session("source");
        updated.title = Some("Updated title".to_string());
        updated.tags = vec!["new".to_string()];
        super::apply_update_response(
            &mut app,
            "source",
            None,
            Ok(RpcResponse::Session { summary: updated }),
        );
        assert!(app.update_dialog.is_none());
        assert_eq!(app.sessions[0].title.as_deref(), Some("Updated title"));
        assert_eq!(app.sessions[0].tags, ["new"]);
        assert_eq!(app.message.as_deref(), Some("updated session source"));
        assert_eq!(
            route_key(&mut app, key(KeyCode::Down), None),
            AppAction::None
        );

        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        super::apply_update_response(
            &mut app,
            "other",
            None,
            Err(AppError::Protocol("session disappeared".to_string())),
        );
        assert!(app.update_dialog.is_some());
        assert_eq!(
            app.update_dialog.as_ref().unwrap().error.as_deref(),
            Some("update failed: protocol error: session disappeared")
        );
    }

    #[test]
    fn terminal_word_input_round_trips_launch_values() {
        let values = vec![
            "plain".to_string(),
            "two words".to_string(),
            "say\"hi".to_string(),
            "C:\\work tree".to_string(),
            String::new(),
            "single'quote".to_string(),
        ];
        let formatted = super::format_terminal_words(&values);
        assert_eq!(
            super::parse_terminal_words("args", &formatted).unwrap(),
            values
        );
        assert_eq!(
            super::parse_terminal_words(
                "args",
                r#"--flag "two words" 'single quoted' C:\work\ path"#,
            )
            .unwrap(),
            ["--flag", "two words", "single quoted", "C:\\work path"]
        );
        assert_eq!(
            super::parse_terminal_words("args", r#""unfinished"#).unwrap_err(),
            "args has an unclosed quote"
        );
    }

    #[test]
    fn focused_text_viewport_tracks_and_blinks_cursor() {
        let mut field = super::EditText::new("0123456789abcdefghij".to_string());

        field.cursor = 0;
        assert_eq!(super::edit_text_viewport(&field, 10, true), "▏012345678");

        field.cursor = 10;
        assert_eq!(super::edit_text_viewport(&field, 10, true), "56789▏abcd");
        assert_eq!(super::edit_text_viewport(&field, 10, false), "56789 abcd");

        field.cursor = field.value.chars().count();
        assert_eq!(super::edit_text_viewport(&field, 10, true), "bcdefghij▏");

        let wide = super::EditText::new("日本語 abcdefghij".to_string());
        let visible = super::edit_text_viewport(&wide, 10, true);
        assert_eq!(unicode_width::UnicodeWidthStr::width(visible.as_str()), 10);
        assert!(visible.contains('▏'));
    }

    #[test]
    fn tab_and_ctrl_tab_navigate_clone_fields() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('d')), None);

        assert_eq!(
            app.clone_dialog.as_ref().unwrap().active_field(),
            CloneField::Command
        );
        route_key(&mut app, key(KeyCode::Tab), None);
        assert_eq!(
            app.clone_dialog.as_ref().unwrap().active_field(),
            CloneField::Args
        );
        route_key(&mut app, ctrl(KeyCode::Tab), None);
        assert_eq!(
            app.clone_dialog.as_ref().unwrap().active_field(),
            CloneField::Command
        );
        route_key(&mut app, key(KeyCode::BackTab), None);
        assert_eq!(
            app.clone_dialog.as_ref().unwrap().active_field(),
            CloneField::AttachAfterStart
        );
    }

    #[test]
    fn enter_confirms_complete_modified_launch_and_request_payload() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('d')), Some("list-node"));
        let dialog = app.clone_dialog.as_mut().unwrap();
        dialog.command = super::EditText::new("agent-cli".to_string());
        dialog.args = super::EditText::new(r#"run "two words""#.to_string());
        dialog.cwd = super::EditText::new("D:\\jobs".to_string());
        dialog.title = super::EditText::new("Cloned agent".to_string());
        dialog.tags = super::EditText::new("alpha beta".to_string());
        dialog.node = super::EditText::new("worker-b".to_string());
        dialog.rows = super::EditText::new("50".to_string());
        dialog.cols = super::EditText::new("160".to_string());
        dialog.disable_notifications = true;
        dialog.attach_after_start = true;

        let expected = CloneLaunch {
            title: Some("Cloned agent".to_string()),
            tags: vec!["alpha".to_string(), "beta".to_string()],
            command: "agent-cli".to_string(),
            args: vec!["run".to_string(), "two words".to_string()],
            cwd: Some("D:\\jobs".to_string()),
            node: Some("worker-b".to_string()),
            rows: Some(50),
            cols: Some(160),
            disable_notifications: true,
            attach_after_start: true,
        };
        assert_eq!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::Start(expected)
        );

        let AppAction::Start(launch) = route_key(&mut app, key(KeyCode::Enter), None) else {
            panic!("expected start action");
        };
        match launch.request() {
            RpcRequest::NodeProxy { node, inner } => {
                assert_eq!(node, "worker-b");
                match *inner {
                    RpcRequest::Start {
                        title,
                        tags,
                        cmd,
                        args,
                        cwd,
                        rows,
                        cols,
                        disable_notifications,
                    } => {
                        assert_eq!(title.as_deref(), Some("Cloned agent"));
                        assert_eq!(tags, ["alpha", "beta"]);
                        assert_eq!(cmd, "agent-cli");
                        assert_eq!(args, ["run", "two words"]);
                        assert_eq!(cwd.as_deref(), Some("D:\\jobs"));
                        assert_eq!(rows, Some(50));
                        assert_eq!(cols, Some(160));
                        assert!(disable_notifications);
                    }
                    other => panic!("unexpected inner request: {}", other.name()),
                }
            }
            other => panic!("unexpected request: {}", other.name()),
        }
    }

    #[test]
    fn ctrl_k_routes_stoppable_selection_and_handles_empty_or_inactive_state() {
        let mut app = App::default();
        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('k')), None),
            AppAction::None
        );
        assert_eq!(app.message.as_deref(), Some("no session in focus to stop"));

        let mut active = session("active");
        active.node = Some("worker-a".to_string());
        app.replace_sessions(vec![active]);
        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('k')), Some("list-node")),
            AppAction::Stop(super::SessionTarget {
                id: "active".to_string(),
                node: Some("worker-a".to_string()),
            })
        );

        app.sessions[0].status = "stopped".to_string();
        assert_eq!(
            route_key(&mut app, ctrl(KeyCode::Char('k')), None),
            AppAction::None
        );
        assert_eq!(
            app.message.as_deref(),
            Some("active cannot be stopped while stopped")
        );
    }

    #[test]
    fn refresh_keeps_selection_by_id() {
        let mut app = App::default();
        app.replace_sessions(vec![session("a"), session("b")]);
        app.selected = 1;
        app.replace_sessions(vec![session("b"), session("c")]);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn external_stop_refresh_updates_state_without_losing_selection() {
        let mut app = App::default();
        app.replace_sessions(vec![session("a"), session("b")]);
        app.selected = 1;

        let mut stopped = session("b");
        stopped.status = "stopped".to_string();
        app.replace_sessions(vec![session("a"), stopped]);

        assert_eq!(
            app.selected_session().map(|item| item.id.as_str()),
            Some("b")
        );
        assert_eq!(
            app.selected_session().map(|item| item.status.as_str()),
            Some("stopped")
        );

        app.replace_sessions(vec![session("a")]);
        assert_eq!(
            app.selected_session().map(|item| item.id.as_str()),
            Some("a")
        );
    }

    #[test]
    fn transient_terminal_errors_do_not_end_the_follow_loop() {
        let interrupted = super::read_terminal_event_with(
            std::time::Duration::ZERO,
            |_| Err(std::io::ErrorKind::Interrupted.into()),
            || panic!("read must not run after an interrupted poll"),
        )
        .unwrap();
        assert!(interrupted.is_none());

        let would_block = super::read_terminal_event_with(
            std::time::Duration::ZERO,
            |_| Ok(true),
            || Err(std::io::ErrorKind::WouldBlock.into()),
        )
        .unwrap();
        assert!(would_block.is_none());

        let fatal = super::read_terminal_event_with(
            std::time::Duration::ZERO,
            |_| Err(std::io::ErrorKind::BrokenPipe.into()),
            || panic!("read must not run after a fatal poll error"),
        )
        .unwrap_err();
        assert_eq!(fatal.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn drain_collects_all_queued_events_before_the_next_draw() {
        let queued = [
            crossterm::event::Event::Key(key(KeyCode::Char('a'))),
            crossterm::event::Event::Key(key(KeyCode::Char('b'))),
            crossterm::event::Event::Key(key(KeyCode::Char('c'))),
        ];
        let position = std::cell::Cell::new(0usize);
        let mut events = vec![crossterm::event::Event::Key(key(KeyCode::Char('0')))];
        super::drain_pending_events_with(
            &mut events,
            |_| Ok(true),
            || {
                let index = position.get();
                position.set(index + 1);
                queued
                    .get(index)
                    .cloned()
                    .ok_or(std::io::ErrorKind::WouldBlock.into())
            },
        )
        .unwrap();
        // The burst is handled as one batch: the initial event plus every
        // queued event, in order, with no draw in between.
        assert_eq!(events.len(), 4);
        assert!(
            matches!(events[1], crossterm::event::Event::Key(k) if k.code == KeyCode::Char('a'))
        );
        assert!(
            matches!(events[3], crossterm::event::Event::Key(k) if k.code == KeyCode::Char('c'))
        );
    }

    fn list_query(limit: usize) -> crate::protocol::ListQuery {
        crate::protocol::ListQuery {
            search: None,
            tags: Vec::new(),
            statuses: Vec::new(),
            since: None,
            until: None,
            limit,
            offset: 0,
            sort: crate::protocol::ListSortField::CreatedAt,
            order: crate::protocol::SortOrder::Desc,
        }
    }

    #[test]
    fn apply_refresh_keeps_sessions_of_failed_nodes_and_warns() {
        let mut app = App::default();
        let mut remote = session("remote");
        remote.node = Some("worker-a".to_string());
        app.replace_sessions(vec![remote]);

        let refresh = SessionRefresh {
            sessions: Vec::new(),
            failed_nodes: HashSet::from([Some("worker-a".to_string())]),
            failures: vec!["worker-a: connection refused".to_string()],
        };
        apply_refresh(&mut app, &list_query(100), Ok(refresh));

        // The unreachable node's last-known sessions stay visible...
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].id, "remote");
        // ...and the warning is shown.
        assert_eq!(
            app.message.as_deref(),
            Some("sync lost: worker-a: connection refused")
        );
    }

    #[test]
    fn apply_refresh_never_clobbers_action_feedback() {
        let mut app = App::default();
        app.set_action_message(Some("started new session abc1234".to_string()));

        let refresh = SessionRefresh {
            sessions: vec![session("a")],
            failed_nodes: HashSet::new(),
            failures: Vec::new(),
        };
        apply_refresh(&mut app, &list_query(100), Ok(refresh));
        assert_eq!(app.message.as_deref(), Some("started new session abc1234"));

        apply_refresh(
            &mut app,
            &list_query(100),
            Err(crate::error::AppError::Protocol("boom".to_string())),
        );
        assert_eq!(app.message.as_deref(), Some("started new session abc1234"));
    }

    #[test]
    fn navigation_wraps() {
        let mut app = App::default();
        app.replace_sessions(vec![session("a"), session("b")]);
        app.previous();
        assert_eq!(app.selected, 1);
        app.next();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn quick_filter_is_case_insensitive_and_limits_navigation() {
        let mut app = App::default();
        let first = session("alpha");
        let mut second = session("beta");
        second.title = Some("Worker Two".to_string());
        app.replace_sessions(vec![first, second]);

        for character in "WORKER".chars() {
            app.push_filter(character);
        }
        assert_eq!(app.visible.clone(), [1]);
        assert_eq!(app.selected, 1);
        app.next();
        assert_eq!(app.selected, 1);

        app.clear_filter();
        assert_eq!(app.visible.clone(), [0, 1]);
    }

    #[test]
    fn status_filter_cycles_all_active_inactive() {
        let mut app = App::default();
        let active = session("active");
        let mut inactive = session("inactive");
        inactive.status = "stopped".to_string();
        app.replace_sessions(vec![active, inactive]);

        assert_eq!(app.visible.clone(), [0, 1]);
        app.toggle_status_filter();
        assert_eq!(app.visible.clone(), [0]);
        app.toggle_status_filter();
        assert_eq!(app.visible.clone(), [1]);
        app.toggle_status_filter();
        assert_eq!(app.visible.clone(), [0, 1]);
    }

    fn session_ids(app: &App) -> Vec<&str> {
        app.sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect()
    }

    #[test]
    fn active_sessions_sort_before_inactive_then_newest_first_by_default() {
        let mut app = App::default();
        let mut old_running = session("old-running");
        old_running.created_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut new_running = session("new-running");
        new_running.created_at = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let mut new_stopped = session("new-stopped");
        new_stopped.status = "stopped".to_string();
        new_stopped.created_at = Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap();
        let mut old_failed = session("old-failed");
        old_failed.status = "failed".to_string();
        old_failed.created_at = Utc.with_ymd_and_hms(2025, 12, 31, 0, 0, 0).unwrap();

        app.replace_sessions(vec![old_failed, new_stopped, old_running, new_running]);

        assert_eq!(
            session_ids(&app),
            ["new-running", "old-running", "new-stopped", "old-failed"]
        );
    }

    #[test]
    fn attention_needed_session_sorts_with_the_active_group() {
        let mut app = App::default();
        let mut waiting = session("waiting");
        waiting.input_needed = true;
        waiting.created_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut stopped = session("stopped");
        stopped.status = "stopped".to_string();
        stopped.created_at = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();

        app.replace_sessions(vec![stopped, waiting]);

        assert_eq!(session_ids(&app), ["waiting", "stopped"]);
    }

    #[test]
    fn ctrl_o_cycles_sort_strategies_and_keeps_selection() {
        let mut app = App::default();
        let mut old_running = session("old-running");
        old_running.created_at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut new_stopped = session("new-stopped");
        new_stopped.status = "stopped".to_string();
        new_stopped.created_at = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();

        app.replace_sessions(vec![new_stopped, old_running]);
        // Default: active sessions first.
        assert_eq!(session_ids(&app), ["old-running", "new-stopped"]);
        app.selected = 1;

        route_key(&mut app, ctrl(KeyCode::Char('o')), None);
        assert_eq!(session_ids(&app), ["new-stopped", "old-running"]);
        assert_eq!(
            app.selected_session().map(|session| session.id.as_str()),
            Some("new-stopped")
        );
        assert!(
            app.message
                .as_deref()
                .is_some_and(|message| message.contains("newest"))
        );

        route_key(&mut app, ctrl(KeyCode::Char('o')), None);
        assert_eq!(session_ids(&app), ["old-running", "new-stopped"]);
        assert!(
            app.message
                .as_deref()
                .is_some_and(|message| message.contains("oldest"))
        );

        route_key(&mut app, ctrl(KeyCode::Char('o')), None);
        assert_eq!(session_ids(&app), ["old-running", "new-stopped"]);
        assert!(
            app.message
                .as_deref()
                .is_some_and(|message| message.contains("active first"))
        );
    }

    #[test]
    fn empty_filter_has_no_selected_session() {
        let mut app = App::default();
        app.replace_sessions(vec![session("visible")]);
        for character in "missing".chars() {
            app.push_filter(character);
        }
        assert!(app.visible.is_empty());
        assert!(app.selected_session().is_none());
    }

    #[test]
    fn stopping_session_is_attachable() {
        let mut item = session("stopping");
        item.status = "stopping".to_string();
        assert!(super::is_active_status(&item.status));
        let (_, args) =
            super::session_command(&item.id, None, super::is_active_status(&item.status)).unwrap();
        assert_eq!(args, ["attach", "stopping"]);
    }

    #[test]
    fn refresh_calculates_rate_and_keeps_history_bounded() {
        let mut app = App::default();
        let first = session("a");
        app.replace_sessions(vec![first]);
        let rate = app.rates.get_mut("a").unwrap();
        rate.sampled_at -= std::time::Duration::from_secs(1);

        let mut next = session("a");
        next.last_total_bytes = 2048;
        next.last_output_epoch = Some(Utc::now());
        app.replace_sessions(vec![next.clone()]);
        assert!((1900.0..=2100.0).contains(&app.rates["a"].rate));

        for total in 3..40 {
            app.rates.get_mut("a").unwrap().sampled_at -= std::time::Duration::from_millis(250);
            next.last_total_bytes = total * 1024;
            next.last_output_epoch = Some(Utc::now());
            app.replace_sessions(vec![next.clone()]);
        }
        assert_eq!(app.rates["a"].history.len(), super::RATE_HISTORY_LEN);
    }

    #[test]
    fn unicode_padding_has_requested_display_width() {
        let padded = super::pad_truncated("日本語 session", 8);
        assert_eq!(unicode_width::UnicodeWidthStr::width(padded.as_str()), 8);
    }

    #[test]
    fn input_required_uses_attention_status_label() {
        assert_eq!(super::status_label("running", true), "attention");
        assert_eq!(super::status_label("running", false), "running");
    }

    #[cfg(windows)]
    #[test]
    fn powershell_script_is_encoded_as_utf16le() {
        use base64::Engine as _;

        let script = "try { & 'D:\\oly.exe' 'attach' '123' } finally { cleanup }";
        let encoded = super::powershell_encoded_command(script);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let decoded = String::from_utf16(
            &bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(decoded, script);
    }

    #[test]
    fn inactive_session_uses_logs_command() {
        let (_, args) = super::session_command("abc", Some("worker"), false).unwrap();
        assert_eq!(args, ["logs", "abc", "--keep-color", "--node", "worker"]);
        let (_, args) = super::session_command("abc", None, true).unwrap();
        assert_eq!(args, ["attach", "abc"]);
    }

    #[test]
    fn aggregate_sparkline_uses_combined_history() {
        let mut app = App::default();
        app.replace_sessions(vec![session("a"), session("b")]);
        app.rates.get_mut("a").unwrap().history = [0.0, 10.0, 20.0].into();
        app.rates.get_mut("b").unwrap().history = [0.0, 20.0, 20.0].into();
        assert_eq!(super::aggregate_sparkline_data(&app.rates, 3), [0, 30, 40]);
    }

    #[test]
    fn table_render_keeps_selected_row_visible_and_shows_scrollbar() {
        let mut app = App::default();
        let sessions = (0..24)
            .map(|index| {
                let mut item = session(&format!("id-{index:02}"));
                item.title = Some(format!("Session {index:02}"));
                item.pid = Some(1000 + index);
                item
            })
            .collect();
        app.replace_sessions(sessions);
        app.selected = 18;

        let rendered = render_app(&mut app, 120, 14);

        assert!(rendered.contains("SESSION"));
        assert!(rendered.contains("COMMAND"));
        assert!(rendered.contains("OUTPUT"));
        assert!(rendered.contains("Session 18"));
        assert!(rendered.contains('┃'));
    }

    #[test]
    fn table_render_uses_responsive_headers() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        let medium = render_app(&mut app, 80, 12);
        assert!(medium.contains("SESSION"));
        assert!(medium.contains("RATE"));
        assert!(!medium.contains("COMMAND"));

        let narrow = render_app(&mut app, 50, 12);
        assert!(narrow.contains("SESSION"));
        assert!(narrow.contains("STATE"));
        assert!(!narrow.contains("RATE"));
    }

    #[test]
    fn multi_node_table_displays_node_and_keeps_duplicate_ids_distinct() {
        let mut app = App {
            show_node: true,
            ..App::default()
        };
        let mut local = session("shared");
        local.title = Some("Local session".to_string());
        let mut remote = session("shared");
        remote.title = Some("Remote session".to_string());
        remote.node = Some("worker-a".to_string());
        app.replace_sessions(vec![local, remote]);

        let rendered = render_app(&mut app, 120, 12);

        assert!(rendered.contains("NODE"));
        assert!(rendered.contains("local"));
        assert!(rendered.contains("worker-a"));
        assert_eq!(app.rates.len(), 2);
    }

    #[test]
    fn table_headers_share_each_modes_cell_alignments() {
        assert_eq!(
            super::session_table_alignments(super::LayoutMode::Narrow, false),
            vec![Alignment::Left; 6]
        );
        assert_eq!(
            super::session_table_alignments(super::LayoutMode::Medium, false),
            vec![Alignment::Left; 6]
        );
        assert_eq!(
            super::session_table_alignments(super::LayoutMode::Wide, false),
            vec![
                Alignment::Left,
                Alignment::Left,
                Alignment::Left,
                Alignment::Right,
                Alignment::Left,
                Alignment::Left,
                Alignment::Left,
                Alignment::Right,
                Alignment::Left,
            ]
        );

        let mut item = session("alignment-id");
        item.title = Some("alignment-title".to_string());
        item.command = "alignment-command".to_string();
        item.pid = Some(4242);
        item.last_total_bytes = 12_345;
        let output = super::format_bytes(item.last_total_bytes as f64);
        let mut app = App::default();
        app.replace_sessions(vec![item]);

        let rendered = render_app(&mut app, 120, 12);
        let header = rendered
            .lines()
            .find(|line| line.contains("OUTPUT"))
            .unwrap();
        let row = rendered
            .lines()
            .find(|line| line.contains("alignment-title"))
            .unwrap();
        let display_start = |line: &str, value: &str| {
            let byte_index = line.find(value).unwrap();
            unicode_width::UnicodeWidthStr::width(&line[..byte_index])
        };
        let selected_row_offset =
            display_start(row, "alignment-title") - display_start(header, "SESSION");
        assert_eq!(
            display_start(header, "PID") + 3 + selected_row_offset,
            display_start(row, "4242") + 4
        );
        assert_eq!(
            display_start(header, "OUTPUT") + "OUTPUT".len() + selected_row_offset,
            display_start(row, &output) + output.len()
        );
    }

    #[test]
    fn responsive_rows_keep_compact_and_normal_session_sparklines() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        app.rates.get_mut("source").unwrap().history = [1.0, 2.0, 4.0, 8.0, 16.0].into();

        let compact = super::sparkline(app.rates.get("source"), super::COMPACT_SPARKLINE_WIDTH);
        let narrow = render_app(&mut app, 50, 12);
        let narrow_row = narrow.lines().find(|line| line.contains("source")).unwrap();
        assert!(narrow_row.contains(&compact));
        assert_eq!(unicode_width::UnicodeWidthStr::width(compact.as_str()), 3);

        let normal = super::sparkline(app.rates.get("source"), super::SPARKLINE_WIDTH);
        let medium = render_app(&mut app, 80, 12);
        let medium_row = medium.lines().find(|line| line.contains("source")).unwrap();
        assert!(medium_row.contains(&normal));
        assert_eq!(unicode_width::UnicodeWidthStr::width(normal.as_str()), 5);
    }

    #[test]
    fn session_sparkline_pads_by_display_cells() {
        let mut app = App::default();
        app.replace_sessions(vec![session("a")]);
        let spark = super::sparkline(app.rates.get("a"), 5);
        assert_eq!(unicode_width::UnicodeWidthStr::width(spark.as_str()), 5);
    }

    #[test]
    fn opened_terminal_tracks_its_own_lifecycle_marker() {
        let mut app = App::default();
        let item = session("a");
        app.replace_sessions(vec![item.clone()]);
        let marker = std::env::temp_dir().join(format!("oly-list-test-{}", uuid::Uuid::new_v4()));
        std::fs::write(&marker, []).unwrap();
        app.opened.insert(
            item.id.clone(),
            super::OpenedTerminal {
                marker: marker.clone(),
                launched_at: std::time::Instant::now(),
            },
        );

        app.replace_sessions(vec![item.clone()]);
        assert!(app.opened.contains_key("a"));
        app.opened.get_mut("a").unwrap().launched_at -= std::time::Duration::from_secs(5);
        app.replace_sessions(vec![item]);
        assert!(!app.opened.contains_key("a"));
        assert!(!marker.exists());
    }

    #[test]
    fn arrangement_uses_distinct_cells_and_stays_on_screen() {
        let work = WindowRect {
            x: 100,
            y: 50,
            width: 1920,
            height: 1040,
        };
        let anchor = WindowRect {
            x: 400,
            y: 200,
            width: 800,
            height: 600,
        };
        let first = arrange_window(work, anchor, (80, 24), 0);
        let second = arrange_window(work, anchor, (80, 24), 1);
        assert_ne!(first, second);
        for rect in [first, second] {
            assert!(rect.x >= work.x && rect.y >= work.y);
            assert!(rect.x + rect.width as i32 <= work.x + work.width as i32);
            assert!(rect.y + rect.height as i32 <= work.y + work.height as i32);
        }
    }

    #[test]
    fn render_tolerates_stale_visible_indices() {
        let mut app = App {
            sessions: vec![session("a")],
            search_text: vec!["a".to_string()],
            visible: vec![usize::MAX],
            ..Default::default()
        };
        app.selected = usize::MAX;

        let rendered = render_app(&mut app, 120, 20);

        assert!(rendered.contains("OPEN RELAY"));
    }

    #[test]
    fn rebuild_visible_repairs_stale_search_index() {
        let mut app = App {
            sessions: vec![session("a")],
            normalized_filter: "cmd".to_string(),
            ..Default::default()
        };
        app.search_text.clear();

        app.rebuild_visible();

        assert_eq!(app.search_text, vec!["a\ncmd".to_string()]);
        assert_eq!(app.visible, vec![0]);
    }

    #[test]
    fn rate_state_tolerates_a_future_sample_instant() {
        let summary = session("a");
        let now = std::time::Instant::now();
        let mut rate = super::RateState::new(&summary, now);
        rate.sampled_at = now + std::time::Duration::from_secs(1);

        assert_eq!(rate.display_rate(now), 0.0);
        rate.sample(&summary, now);
        assert_eq!(rate.display_rate(now), 0.0);
    }

    #[test]
    fn panic_payload_message_preserves_useful_details() {
        let borrowed: Box<dyn std::any::Any + Send> = Box::new("render failed");
        let owned: Box<dyn std::any::Any + Send> = Box::new("terminal failed".to_string());
        let unknown: Box<dyn std::any::Any + Send> = Box::new(42_u32);

        assert_eq!(panic_payload_message(borrowed.as_ref()), "render failed");
        assert_eq!(panic_payload_message(owned.as_ref()), "terminal failed");
        assert_eq!(
            panic_payload_message(unknown.as_ref()),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn list_title_entry_saves_then_sets_window_title() {
        let mut output = Vec::new();

        enter_list_title(&mut output).unwrap();

        assert_eq!(
            output,
            [
                TITLE_SAVE_BYTES,
                b"\x1b]0;",
                LIST_WINDOW_TITLE.as_bytes(),
                b"\x07"
            ]
            .concat()
        );
        // The push must come first so teardown can pop back to the original.
        assert!(output.starts_with(b"\x1b[22;0t"));
        assert!(output.ends_with(b"\x07"));
    }

    #[test]
    fn title_save_and_restore_are_symmetric_title_stack_ops() {
        assert_eq!(TITLE_SAVE_BYTES, b"\x1b[22;0t");
        assert_eq!(TITLE_RESTORE_BYTES, b"\x1b[23;0t");
    }

    #[test]
    fn terminal_restore_is_complete_and_flushed() {
        let mut output = Vec::new();

        restore_tui_state(&mut output).unwrap();

        assert_eq!(output, TUI_RESTORE_BYTES);
        for sequence in [
            b"\x1b[?1049l".as_slice(),
            b"\x1b[?2026l".as_slice(),
            b"\x1b[0m".as_slice(),
            b"\x1b[?25h".as_slice(),
            b"\x1b[?2004l".as_slice(),
        ] {
            assert!(
                output
                    .windows(sequence.len())
                    .any(|window| window == sequence)
            );
        }
    }
}
