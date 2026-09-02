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

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
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

const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(16);
const REDRAW_INTERVAL: Duration = Duration::from_millis(250);
const RATE_HISTORY_LEN: usize = 30;
const COMPACT_SPARKLINE_WIDTH: usize = 3;
const SPARKLINE_WIDTH: usize = 5;
const STOP_GRACE_SECONDS: u64 = 15;
const SPARK_BLOCKS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const TUI_RESTORE_BYTES: &[u8] = b"\x1b[?1049l\x1b[?2026l\x1b[0m\x1b[?25h\x1b[0 q\
    \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l";
const CLONE_DIALOG_HELP: &str =
    " Quotes keep spaces · ←/→ cursor · Tab/Shift+Tab · Space toggle · Enter create · Esc cancel";
const UPDATE_DIALOG_HELP: &str =
    " Quote multi-word tags · Tab/Shift+Tab · Space toggle · Enter save · Esc cancel";

use super::list::ListTarget;

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
    native_crash::install();

    let query = super::list::build_list_query(args)?;
    let mut app = App::default();
    app.show_node = targets.len() > 1;
    let refresh = fetch_sessions(config, query.clone(), &targets).await?;
    app.message = refresh.warning();
    app.replace_sessions(refresh.sessions);
    let mut terminal = TuiTerminal::new()?;
    let mut last_refresh = Instant::now();
    let mut last_draw = Instant::now();
    let mut redraw = true;

    loop {
        if redraw || last_draw.elapsed() >= REDRAW_INTERVAL {
            terminal.draw(|frame| render(frame, &mut app))?;
            last_draw = Instant::now();
            redraw = false;
        }

        match read_terminal_event(INPUT_POLL_INTERVAL)? {
            Some(Event::Key(key)) if key.kind != KeyEventKind::Release => {
                match route_key(&mut app, key, None) {
                    AppAction::None => {}
                    AppAction::Quit => break,
                    AppAction::OpenInline => open_selected_inline(&mut terminal, &mut app, None)?,
                    AppAction::Start(launch) => {
                        start_clone(config, &mut terminal, &mut app, launch).await?
                    }
                    AppAction::Update(update) => update_session(config, &mut app, update).await,
                    AppAction::Stop(target) => stop_session(config, &mut app, target),
                }
                redraw = true;
            }
            Some(Event::Resize(_, _)) => redraw = true,
            _ => {}
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match fetch_sessions(config, query.clone(), &targets).await {
                Ok(mut refresh) => {
                    refresh.sessions.extend(
                        app.sessions
                            .iter()
                            .filter(|session| refresh.failed_nodes.contains(&session.node))
                            .cloned(),
                    );
                    refresh
                        .sessions
                        .sort_by(|a, b| b.created_at.cmp(&a.created_at));
                    refresh.sessions.truncate(query.limit);
                    let warning = refresh.warning();
                    app.replace_sessions(refresh.sessions);
                    app.message = warning;
                }
                Err(error) => app.message = Some(format!("sync lost: {error}")),
            }
            last_refresh = Instant::now();
            redraw = true;
        }
    }

    terminal.teardown()?;
    Ok(())
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
    sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    sessions.truncate(query.limit);
    Ok(SessionRefresh {
        sessions,
        failed_nodes,
        failures,
    })
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
            app.message = Some(format!("started new session {session_id}"));
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
            app.message = Some(format!("updated session {target_id}"));
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
    app.message = Some(format!("stop signal sent to {}", target.id));
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
        app.message = Some(error);
    }
}

fn set_update_error(app: &mut App, error: String) {
    if let Some(dialog) = app.update_dialog.as_mut() {
        dialog.error = Some(error);
    } else {
        app.message = Some(error);
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
    filter: String,
    normalized_filter: String,
    search_text: Vec<String>,
    visible: Vec<usize>,
    status_filter: StatusFilter,
    clone_dialog: Option<CloneDialog>,
    update_dialog: Option<UpdateDialog>,
    show_node: bool,
}

#[derive(Debug, Eq, PartialEq)]
enum AppAction {
    None,
    Quit,
    OpenInline,
    Start(CloneLaunch),
    Update(SessionUpdate),
    Stop(SessionTarget),
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

    fn blank() -> Self {
        Self {
            source_id: None,
            active: 0,
            command: EditText::new(String::new()),
            args: EditText::new(String::new()),
            cwd: EditText::new(String::new()),
            title: EditText::new(String::new()),
            tags: EditText::new(String::new()),
            node: EditText::new(String::new()),
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
            app.clone_dialog = Some(CloneDialog::blank());
            AppAction::None
        }
        _ if is_clone_dialog_key(key) => {
            let Some(session) = app.selected_session() else {
                app.message = Some("no session selected to clone".to_string());
                return AppAction::None;
            };
            app.clone_dialog = Some(CloneDialog::from_session(session, list_node));
            AppAction::None
        }
        _ if is_update_dialog_key(key) => {
            let Some(session) = app.selected_session() else {
                app.message = Some("no session selected to update".to_string());
                return AppAction::None;
            };
            app.update_dialog = Some(UpdateDialog::from_session(session, list_node));
            AppAction::None
        }
        KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let Some(session) = app.selected_session() else {
                app.message = Some("no session selected to stop".to_string());
                return AppAction::None;
            };
            if !matches!(session.status.as_str(), "created" | "running") {
                app.message = Some(format!(
                    "{} cannot be stopped while {}",
                    session.id, session.status
                ));
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
        KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.toggle_status_filter();
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
            app.previous();
            AppAction::None
        }
        KeyCode::Down => {
            app.next();
            AppAction::None
        }
        KeyCode::Home => {
            app.first();
            AppAction::None
        }
        KeyCode::End => {
            app.last();
            AppAction::None
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_selected_terminal(list_node);
            AppAction::None
        }
        KeyCode::Enter => AppAction::OpenInline,
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
        app.message = Some(message.to_string());
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
        app.message = Some("update cancelled".to_string());
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

impl App {
    fn replace_sessions(&mut self, sessions: Vec<SessionSummary>) {
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
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        self.visible
            .contains(&self.selected)
            .then(|| self.sessions.get(self.selected))
            .flatten()
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
        self.first();
        self.message = None;
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

    fn toggle_status_filter(&mut self) {
        self.status_filter = match self.status_filter {
            StatusFilter::All => StatusFilter::Active,
            StatusFilter::Active => StatusFilter::Inactive,
            StatusFilter::Inactive => StatusFilter::All,
        };
        self.rebuild_visible();
        self.first();
        self.message = Some(format!(
            "showing {} sessions · Ctrl+S toggle",
            self.status_filter.label()
        ));
    }

    fn open_selected_terminal(&mut self, node: Option<&str>) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let attach = is_active_status(&session.status);
        let id = session.id.clone();
        let target_node = session.node.as_deref().or(node).map(str::to_string);
        let key = session_key(session);
        let size = (session.cols.unwrap_or(80), session.rows.unwrap_or(24));
        if attach && self.opened.contains_key(&key) {
            self.message = Some(format!("{id} is already jacked in"));
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
                self.message = Some(if attach {
                    format!("opened {id} · link established")
                } else {
                    format!("opened {id} · log tail")
                });
            }
            Err(error) => self.message = Some(format!("launch failed: {error}")),
        }
    }
}

fn open_selected_inline(
    terminal: &mut TuiTerminal,
    app: &mut App,
    node: Option<&str>,
) -> Result<()> {
    let Some(session) = app.selected_session() else {
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

    app.message = Some(match result {
        Ok(status) if status.success() => format!("returned from {id}"),
        Ok(status) => format!("session {id} exited with {status}"),
        Err(error) => format!("open failed: {error}"),
    });
    Ok(())
}

fn wait_for_ctrl_d() -> Result<()> {
    println!("\nPress Ctrl+D to return to the session list");
    io::stdout().flush()?;
    enable_raw_mode()?;
    let result = loop {
        match event::read() {
            Ok(Event::Key(key))
                if key.kind != KeyEventKind::Release
                    && key.code == KeyCode::Char('d')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break Ok(());
            }
            Ok(_) => {}
            Err(error) => break Err(error.into()),
        }
    };
    let _ = disable_raw_mode();
    result
}

struct TuiTerminal {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    cleaned_up: bool,
}

impl TuiTerminal {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        #[cfg(windows)]
        native_crash::set_tui_active(true);
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            let _ = restore_tui_state(&mut stdout);
            #[cfg(windows)]
            native_crash::set_tui_active(false);
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                cleaned_up: false,
            }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = disable_raw_mode();
                let _ = restore_tui_state(&mut stdout);
                #[cfg(windows)]
                native_crash::set_tui_active(false);
                Err(error.into())
            }
        }
    }

    fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }

    /// Hand the terminal to a child process (`oly attach` / `oly logs`) on the
    /// main screen.  Leaving the alternate buffer lets the child render like a
    /// natively running CLI: its output flows into the terminal's scrollback,
    /// so history and the scrollbar keep working during and after the inline
    /// view.  Only raw mode is released, because the child installs its own.
    fn suspend(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show)?;
        Ok(())
    }

    /// Take the terminal back after the child exits.  `EnterAlternateScreen` is
    /// re-issued unconditionally: an attached child's teardown emits
    /// `\x1b[?1049l`, which drops the terminal back to the main buffer even
    /// though we never left it ourselves.
    fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen, Hide)?;
        self.terminal.clear()?;
        Ok(())
    }

    fn teardown(&mut self) -> Result<()> {
        if self.cleaned_up {
            return Ok(());
        }

        let mut first_error = disable_raw_mode().err();
        if let Err(error) = restore_tui_state(self.terminal.backend_mut())
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        #[cfg(windows)]
        native_crash::set_tui_active(false);
        self.cleaned_up = true;

        if let Some(error) = first_error {
            Err(error.into())
        } else {
            Ok(())
        }
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

fn restore_tui_state(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(TUI_RESTORE_BYTES)?;
    writer.flush()
}

#[cfg(windows)]
mod native_crash {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::{
        Storage::FileSystem::WriteFile,
        System::{
            Console::{GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE},
            Diagnostics::Debug::{
                EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
            },
        },
    };

    use super::TUI_RESTORE_BYTES;

    static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
    static CRASH_REPORTED: AtomicBool = AtomicBool::new(false);

    /// Restore the terminal just before the process dies from a fatal exception.
    ///
    /// Only the top-level unhandled-exception filter is used: unlike a vectored
    /// handler it runs solely for genuinely unhandled exceptions, so handled
    /// first-chance exceptions raised by normal Windows code cannot tear down
    /// the live TUI by mistake.
    pub(super) fn install() {
        unsafe {
            SetUnhandledExceptionFilter(Some(handle_unhandled_exception));
        }
    }

    pub(super) fn set_tui_active(active: bool) {
        if active {
            CRASH_REPORTED.store(false, Ordering::SeqCst);
        }
        TUI_ACTIVE.store(active, Ordering::SeqCst);
    }

    unsafe extern "system" fn handle_unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
        unsafe { report_crash(info) };
        EXCEPTION_CONTINUE_SEARCH
    }

    unsafe fn report_crash(info: *const EXCEPTION_POINTERS) {
        if TUI_ACTIVE.load(Ordering::SeqCst) && !CRASH_REPORTED.swap(true, Ordering::SeqCst) {
            unsafe {
                write_handle(STD_OUTPUT_HANDLE, TUI_RESTORE_BYTES);
                write_handle(STD_ERROR_HANDLE, native_crash_message(info));
            }
        }
    }

    unsafe fn native_crash_message(info: *const EXCEPTION_POINTERS) -> &'static [u8] {
        let code = unsafe {
            if info.is_null() || (*info).ExceptionRecord.is_null() {
                0
            } else {
                (*(*info).ExceptionRecord).ExceptionCode as u32
            }
        };
        match code {
            0xC0000005 => {
                b"\r\nerror: interactive session list crashed: STATUS_ACCESS_VIOLATION\r\n"
            }
            0xC00000FD => b"\r\nerror: interactive session list crashed: STATUS_STACK_OVERFLOW\r\n",
            0xC0000374 => {
                b"\r\nerror: interactive session list crashed: STATUS_HEAP_CORRUPTION\r\n"
            }
            _ => b"\r\nerror: interactive session list crashed: native exception\r\n",
        }
    }

    unsafe fn write_handle(handle_kind: STD_HANDLE, bytes: &[u8]) {
        let handle = unsafe { GetStdHandle(handle_kind) };
        let mut written = 0;
        let _ = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
    }
}

#[derive(Clone, Copy)]
enum LayoutMode {
    Wide,
    Medium,
    Narrow,
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
    let chunks = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
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
        Span::raw(format!(
            " {} sessions · {} live · {}",
            app.sessions.len(),
            running,
            app.status_filter.label()
        )),
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
    if app.sessions.is_empty() || visible.is_empty() {
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
        let rows = visible.iter().filter_map(|index| {
            app.sessions.get(*index).map(|session| {
                session_row(
                    session,
                    app.rates.get(&session_key(session)),
                    mode,
                    now,
                    app.show_node,
                )
            })
        });
        let table = Table::new(rows, session_table_widths(mode, app.show_node))
            .header(session_table_header(mode, app.show_node))
            .column_spacing(1)
            .highlight_symbol("▸ ")
            .row_highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(25, 55, 72))
                    .add_modifier(Modifier::BOLD),
            );
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

    let default_help = if app.filter.is_empty() {
        match mode {
            LayoutMode::Narrow => {
                " type filter  ^N new  ^D clone  ^U edit  ^K stop  ↵/^↵ open ^C exit".to_string()
            }
            _ => " type to filter    Ctrl+N new    Ctrl+D duplicate    Ctrl+U update    Ctrl+K stop    Enter open    Ctrl+Enter window    Ctrl+S status    Ctrl+C exit".to_string(),
        }
    } else {
        format!(
            " filter: {}_    status: {} (Ctrl+S)    Ctrl+N new    Ctrl+D duplicate    Ctrl+U update    Ctrl+K stop    Backspace edit    Esc clear",
            app.filter,
            app.status_filter.label()
        )
    };
    let help = app.message.as_deref().unwrap_or(&default_help);
    frame.render_widget(
        Paragraph::new(help)
            .style(Style::default().fg(if app.message.is_some() {
                Color::Yellow
            } else {
                Color::DarkGray
            }))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
        chunks[2],
    );

    if let Some(dialog) = app.clone_dialog.as_ref() {
        render_clone_dialog(frame, dialog);
    } else if let Some(dialog) = app.update_dialog.as_ref() {
        render_update_dialog(frame, dialog);
    }
}

fn render_clone_dialog(frame: &mut Frame<'_>, dialog: &CloneDialog) {
    let area = centered_rect(frame.area(), 96, 14);
    let cursor_visible = clone_cursor_visible();
    let fields =
        CLONE_FIELDS.map(|field| clone_field_line(dialog, field, area.width, cursor_visible));
    let mut lines = fields.to_vec();
    lines.push(tip_separator(area.width));
    lines.push(dialog_footer(dialog.error.as_deref(), CLONE_DIALOG_HELP));
    render_dialog(
        frame,
        area,
        dialog.source_id.as_ref().map_or_else(
            || " New Session ".to_string(),
            |source_id| format!(" Duplicate {source_id} "),
        ),
        Color::Cyan,
        lines,
    );
}

fn render_update_dialog(frame: &mut Frame<'_>, dialog: &UpdateDialog) {
    let area = centered_rect(frame.area(), 110, 19);
    let cursor_visible = clone_cursor_visible();
    let mut lines = UPDATE_FIELDS
        .map(|field| update_field_line(dialog, field, area.width, cursor_visible))
        .to_vec();
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
        format!(" Update {} ", dialog.target_id),
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
    Line::from(Span::styled(
        error.unwrap_or(help),
        Style::default().fg(if error.is_some() {
            Color::Red
        } else {
            Color::DarkGray
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
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .shadow(Shadow::dark_shade().style(Style::default().fg(Color::DarkGray)));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn update_field_line(
    dialog: &UpdateDialog,
    field: UpdateField,
    width: u16,
    cursor_visible: bool,
) -> Line<'static> {
    let active = dialog.active_field() == field;
    let (label, value) = match field {
        UpdateField::Title => (
            "Title",
            edit_text_display(
                &dialog.title,
                active,
                width.saturating_sub(24) as usize,
                cursor_visible,
            ),
        ),
        UpdateField::Tags => (
            "Tags",
            edit_text_display(
                &dialog.tags,
                active,
                width.saturating_sub(24) as usize,
                cursor_visible,
            ),
        ),
        UpdateField::Notifications => (
            "Notifications",
            pad_truncated(
                &checkbox(dialog.notifications_enabled),
                width.saturating_sub(24) as usize,
            ),
        ),
    };
    Line::from(vec![
        Span::styled(
            format!(" {label:<22}"),
            Style::default()
                .fg(if active { Color::Yellow } else { Color::Cyan })
                .add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(
            value,
            Style::default()
                .fg(if active { Color::White } else { Color::Gray })
                .add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ])
}

fn update_read_only_line(label: &str, value: &str, width: u16) -> Line<'static> {
    let value_width = width.saturating_sub(24) as usize;
    Line::from(vec![
        Span::styled(
            format!(" {label:<22}"),
            Style::default().fg(Color::DarkGray),
        ),
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
    let label = match field {
        CloneField::Command => "Cmd",
        CloneField::Args => "Args",
        CloneField::Cwd => "Cwd",
        CloneField::Title => "Title",
        CloneField::Tags => "Tags",
        CloneField::Node => "Node",
        CloneField::Rows => "Rows",
        CloneField::Cols => "Cols",
        CloneField::DisableNotifications => "Notifications",
        CloneField::AttachAfterStart => "Attach after start",
    };
    let value_width = width.saturating_sub(22) as usize;
    let value = match field {
        CloneField::Command => {
            edit_text_display(&dialog.command, active, value_width, cursor_visible)
        }
        CloneField::Args => edit_text_display(&dialog.args, active, value_width, cursor_visible),
        CloneField::Cwd => edit_text_display(&dialog.cwd, active, value_width, cursor_visible),
        CloneField::Title => edit_text_display(&dialog.title, active, value_width, cursor_visible),
        CloneField::Tags => edit_text_display(&dialog.tags, active, value_width, cursor_visible),
        CloneField::Node => edit_text_display(&dialog.node, active, value_width, cursor_visible),
        CloneField::Rows => edit_text_display(&dialog.rows, active, value_width, cursor_visible),
        CloneField::Cols => edit_text_display(&dialog.cols, active, value_width, cursor_visible),
        CloneField::DisableNotifications => {
            pad_truncated(&checkbox(!dialog.disable_notifications), value_width)
        }
        CloneField::AttachAfterStart => {
            pad_truncated(&checkbox(dialog.attach_after_start), value_width)
        }
    };
    Line::from(vec![
        Span::styled(
            format!(" {:<21}", label),
            Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }),
        ),
        Span::styled(
            value,
            Style::default()
                .fg(if active { Color::White } else { Color::Gray })
                .add_modifier(if active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
    ])
}

fn edit_text_display(field: &EditText, active: bool, width: usize, cursor_visible: bool) -> String {
    if !active {
        return pad_truncated(&field.value, width);
    }
    edit_text_viewport(field, width, cursor_visible)
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
        "✅".to_string()
    } else {
        "❌".to_string()
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
        LayoutMode::Wide => vec![
            Constraint::Length(1),
            Constraint::Length(22),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Fill(1),
            Constraint::Length(12),
            Constraint::Length(8),
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
            Alignment::Right,
            Alignment::Left,
            Alignment::Left,
            Alignment::Left,
            Alignment::Left,
            Alignment::Left,
            Alignment::Right,
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
            "", "SESSION", "PID", "STATE", "AGE", "ID", "COMMAND", "RATE", "OUTPUT",
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
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

fn aligned_cell(content: impl Into<Line<'static>>, alignment: Alignment) -> Cell<'static> {
    Cell::from(content.into().alignment(alignment))
}

fn session_row(
    session: &SessionSummary,
    rate: Option<&RateState>,
    mode: LayoutMode,
    now: Instant,
    show_node: bool,
) -> Row<'static> {
    let active = is_active_status(&session.status);
    let mut status = status_glyph(&session.status, session.input_needed);
    if !active {
        status.1 = Color::DarkGray;
    }
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
            aligned_cell(
                Span::styled(status.0, Style::default().fg(status.1)),
                alignments[0],
            ),
            aligned_cell(session.id.clone(), alignments[1 + node_offset])
                .style(Style::default().fg(Color::DarkGray)),
            aligned_cell(name, alignments[2 + node_offset]),
            aligned_cell(status_text.to_string(), alignments[3 + node_offset])
                .style(Style::default().fg(status.1)),
            aligned_cell(age, alignments[4 + node_offset])
                .style(Style::default().fg(Color::DarkGray)),
            aligned_cell(
                sparkline(rate, COMPACT_SPARKLINE_WIDTH),
                alignments[5 + node_offset],
            )
            .style(Style::default().fg(rate_color)),
        ],
        LayoutMode::Medium => vec![
            aligned_cell(
                Span::styled(status.0, Style::default().fg(status.1)),
                alignments[0],
            ),
            aligned_cell(session.id.clone(), alignments[1 + node_offset])
                .style(Style::default().fg(Color::DarkGray)),
            aligned_cell(name, alignments[2 + node_offset]),
            aligned_cell(status_text.to_string(), alignments[3 + node_offset])
                .style(Style::default().fg(status.1)),
            aligned_cell(age, alignments[4 + node_offset])
                .style(Style::default().fg(Color::DarkGray)),
            aligned_cell(
                format!(
                    "{} {:>6}/s",
                    sparkline(rate, SPARKLINE_WIDTH),
                    format_bytes(current_rate)
                ),
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
                aligned_cell(
                    Span::styled(status.0, Style::default().fg(status.1)),
                    alignments[0],
                ),
                aligned_cell(name, alignments[1 + node_offset]),
                aligned_cell(
                    session.pid.map_or("-".into(), |pid| pid.to_string()),
                    alignments[2 + node_offset],
                )
                .style(Style::default().fg(Color::DarkGray)),
                aligned_cell(status_text.to_string(), alignments[3 + node_offset])
                    .style(Style::default().fg(status.1)),
                aligned_cell(age, alignments[4 + node_offset])
                    .style(Style::default().fg(Color::DarkGray)),
                aligned_cell(session.id.clone(), alignments[5 + node_offset])
                    .style(Style::default().fg(Color::DarkGray)),
                aligned_cell(command, alignments[6 + node_offset]),
                aligned_cell(
                    format!(
                        "{} {:>6}/s",
                        sparkline(rate, SPARKLINE_WIDTH),
                        format_bytes(current_rate)
                    ),
                    alignments[7 + node_offset],
                )
                .style(Style::default().fg(rate_color)),
                aligned_cell(
                    format_bytes(session.last_total_bytes as f64),
                    alignments[8 + node_offset],
                )
                .style(Style::default().fg(Color::DarkGray)),
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
    if active {
        row
    } else {
        row.style(Style::default().fg(Color::DarkGray))
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

fn status_label<'a>(status: &'a str, input_needed: bool) -> &'a str {
    if input_needed { "attention" } else { status }
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
        App, AppAction, CloneField, CloneLaunch, TUI_RESTORE_BYTES, WindowRect, arrange_window,
        panic_payload_message, restore_tui_state, route_key,
    };
    use crate::{
        error::AppError,
        protocol::{RpcRequest, RpcResponse, SessionSummary},
    };
    use chrono::{Local, TimeZone, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, layout::Alignment};

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
            notifications_enabled: false,
            node: None,
            last_total_bytes: 0,
            last_output_epoch: None,
            rows: Some(24),
            cols: Some(80),
            attach_count: 0,
        }
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
    fn ctrl_n_opens_completely_blank_new_session_dialog() {
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
        assert!(dialog.node.value.is_empty());
        assert!(dialog.rows.value.is_empty());
        assert!(dialog.cols.value.is_empty());
        assert!(!dialog.disable_notifications);
        assert!(!dialog.attach_after_start);

        let rendered = render_app(&mut app, 120, 30);
        assert!(rendered.contains("New Session"));
        assert!(!rendered.contains("Duplicate source"));
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
            editable_line.spans[0].style.fg,
            Some(ratatui::style::Color::Yellow)
        );
        let read_only_line = super::update_read_only_line("ID", "source", 80);
        assert!(read_only_line.to_string().contains("ID"));
        assert!(!read_only_line.to_string().contains("read-only"));
        assert!(read_only_line.to_string().contains("source"));
        assert_eq!(
            read_only_line.spans[0].style.fg,
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
            Some("no session selected to update")
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
        assert!(!app.update_dialog.as_ref().unwrap().notifications_enabled);
        assert_eq!(
            route_key(&mut app, key(KeyCode::Char(' ')), None),
            AppAction::None
        );
        assert!(app.update_dialog.as_ref().unwrap().notifications_enabled);
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
    fn dialog_boolean_fields_use_emoji_indicators() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);
        route_key(&mut app, ctrl(KeyCode::Char('u')), None);
        let dialog = app.update_dialog.as_mut().unwrap();

        let disabled =
            super::update_field_line(dialog, super::UpdateField::Notifications, 80, true);
        assert!(disabled.to_string().contains("❌"));
        assert!(!disabled.to_string().contains("disabled"));

        dialog.notifications_enabled = true;
        let enabled = super::update_field_line(dialog, super::UpdateField::Notifications, 80, true);
        assert!(enabled.to_string().contains("✅"));
        assert!(!enabled.to_string().contains("enabled"));

        assert_eq!(super::checkbox(true), "✅");
        assert_eq!(super::checkbox(false), "❌");
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
    fn list_and_dialog_tips_render_with_top_dividers() {
        let mut app = App::default();
        app.replace_sessions(vec![session("source")]);

        let list = render_app(&mut app, 120, 30);
        let list_lines = list.lines().collect::<Vec<_>>();
        let help_index = list_lines
            .iter()
            .position(|line| line.contains("Ctrl+D duplicate"))
            .unwrap();
        assert!(list_lines[help_index - 1].contains('\u{2500}'));

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
        dialog.notifications_enabled = true;
        let AppAction::Update(update) = route_key(&mut app, key(KeyCode::Enter), None) else {
            panic!("expected update action");
        };
        assert_eq!(update.title.as_deref(), Some(""));
        assert_eq!(
            update.tags,
            Some(vec!["beta".to_string(), "two words".to_string()])
        );
        assert_eq!(update.notifications_enabled, Some(true));
        match update.request() {
            RpcRequest::NodeProxy { node, inner } => {
                assert_eq!(node, "worker-a");
                assert!(matches!(
                    *inner,
                    RpcRequest::SessionMetadataSet {
                        ref id,
                        title: Some(ref title),
                        tags: Some(ref tags),
                        notifications_enabled: Some(true),
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
        assert_eq!(app.message.as_deref(), Some("no session selected to stop"));

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
                Alignment::Right,
                Alignment::Left,
                Alignment::Left,
                Alignment::Left,
                Alignment::Left,
                Alignment::Left,
                Alignment::Right,
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
        let mut app = App::default();
        app.sessions = vec![session("a")];
        app.search_text = vec!["a".to_string()];
        app.visible = vec![usize::MAX];
        app.selected = usize::MAX;

        let rendered = render_app(&mut app, 120, 20);

        assert!(rendered.contains("OPEN RELAY"));
    }

    #[test]
    fn rebuild_visible_repairs_stale_search_index() {
        let mut app = App::default();
        app.sessions = vec![session("a")];
        app.search_text.clear();
        app.normalized_filter = "cmd".to_string();

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
