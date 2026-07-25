use std::{
    collections::{HashMap, VecDeque},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::{
    cli::ListArgs,
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse, SessionSummary},
};

const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const RATE_HISTORY_LEN: usize = 30;
const SPARK_BLOCKS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub(super) async fn run(config: &AppConfig, args: &ListArgs, node: Option<String>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(AppError::Protocol(
            "--follow requires an interactive terminal".to_string(),
        ));
    }

    let query = super::list::build_list_query(args)?;
    let mut app = App::default();
    app.replace_sessions(fetch_sessions(config, query.clone(), node.as_deref()).await?);
    let mut terminal = TuiTerminal::new()?;
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;
        if event::poll(FRAME_INTERVAL)? {
            if let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
            {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Home => app.first(),
                    KeyCode::End => app.last(),
                    KeyCode::Enter => app.open_selected(node.as_deref()),
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH_INTERVAL {
            match fetch_sessions(config, query.clone(), node.as_deref()).await {
                Ok(sessions) => {
                    app.replace_sessions(sessions);
                    app.message = None;
                }
                Err(error) => app.message = Some(format!("sync lost: {error}")),
            }
            last_refresh = Instant::now();
        }
    }

    Ok(())
}

async fn fetch_sessions(
    config: &AppConfig,
    query: crate::protocol::ListQuery,
    node: Option<&str>,
) -> Result<Vec<SessionSummary>> {
    let inner = RpcRequest::List { query };
    let request = match node {
        Some(node) => RpcRequest::NodeProxy {
            node: node.to_string(),
            inner: Box::new(inner),
        },
        None => inner,
    };
    match ipc::send_request_checked(config, request).await? {
        RpcResponse::List { mut sessions, .. } => {
            sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            Ok(sessions)
        }
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
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
        let elapsed = now.duration_since(self.sampled_at).as_secs_f64();
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
        let progress =
            now.duration_since(self.sampled_at).as_secs_f64() / REFRESH_INTERVAL.as_secs_f64();
        let eased = progress.clamp(0.0, 1.0);
        self.previous_rate + (self.rate - self.previous_rate) * eased
    }
}

impl App {
    fn replace_sessions(&mut self, sessions: Vec<SessionSummary>) {
        let selected_id = self.sessions.get(self.selected).map(|item| item.id.clone());
        let now = Instant::now();
        for session in &sessions {
            match self.rates.get_mut(&session.id) {
                Some(rate) => rate.sample(session, now),
                None => {
                    self.rates
                        .insert(session.id.clone(), RateState::new(session, now));
                }
            }
        }
        self.rates
            .retain(|id, _| sessions.iter().any(|session| session.id == *id));
        self.sessions = sessions;
        self.opened.retain(|id, opened| {
            let attach_count = self
                .sessions
                .iter()
                .find(|session| session.id == *id)
                .map(|session| session.attach_count);
            let launch_pending = opened.launched_at.elapsed() < Duration::from_secs(5);
            let active = attach_count.is_some_and(|count| count > 0);
            if opened.marker.exists() && !launch_pending && !active {
                let _ = std::fs::remove_file(&opened.marker);
            }
            attach_count.is_some() && (launch_pending || opened.marker.exists())
        });
        self.selected = selected_id
            .and_then(|id| self.sessions.iter().position(|item| item.id == id))
            .unwrap_or_else(|| self.selected.min(self.sessions.len().saturating_sub(1)));
    }

    fn previous(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or(self.sessions.len() - 1);
        }
    }

    fn next(&mut self) {
        if !self.sessions.is_empty() {
            self.selected = (self.selected + 1) % self.sessions.len();
        }
    }

    fn first(&mut self) {
        self.selected = 0;
    }

    fn last(&mut self) {
        self.selected = self.sessions.len().saturating_sub(1);
    }

    fn open_selected(&mut self, node: Option<&str>) {
        let Some(session) = self.sessions.get(self.selected) else {
            return;
        };
        let attach = matches!(session.status.as_str(), "created" | "running");
        if attach && self.opened.contains_key(&session.id) {
            self.message = Some(format!("{} is already jacked in", session.id));
            return;
        }

        let size = (session.cols.unwrap_or(80), session.rows.unwrap_or(24));
        let marker = attach.then(|| terminal_marker(&session.id));
        match spawn_session_terminal(
            &session.id,
            node,
            size,
            self.next_slot,
            attach,
            marker.as_deref(),
        ) {
            Ok(()) => {
                if let Some(marker) = marker {
                    self.opened.insert(
                        session.id.clone(),
                        OpenedTerminal {
                            marker,
                            launched_at: Instant::now(),
                        },
                    );
                }
                self.next_slot += 1;
                self.message = Some(if attach {
                    format!("opened {} · link established", session.id)
                } else {
                    format!("opened {} · log tail", session.id)
                });
            }
            Err(error) => self.message = Some(format!("launch failed: {error}")),
        }
    }
}

struct TuiTerminal {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TuiTerminal {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }

    fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
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
        .filter_map(|session| app.rates.get(&session.id))
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
            " {} sessions · {} live",
            app.sessions.len(),
            running
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
        .map(|rate| now.duration_since(rate.sampled_at))
        .min()
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                aggregate_sparkline(&app.rates, 6),
                Style::default().fg(rate_color(throughput, animation_age)),
            ),
            Span::styled(
                format!(" {:>7}/s", format_bytes(throughput)),
                Style::default().fg(if throughput > 0.0 {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            ),
        ]))
        .alignment(Alignment::Right)
        .block(header_block()),
        header[1],
    );

    if app.sessions.is_empty() {
        frame.render_widget(
            Paragraph::new("\n  no signals detected\n  start one: oly start -d <cmd>")
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );
    } else {
        let items = app.sessions.iter().map(|session| {
            session_item(
                session,
                app.rates.get(&session.id),
                mode,
                chunks[1].width.saturating_sub(2),
                now,
            )
        });
        let list = List::new(items).highlight_symbol("▸ ").highlight_style(
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(25, 55, 72))
                .add_modifier(Modifier::BOLD),
        );
        let mut state = ListState::default().with_selected(Some(app.selected));
        frame.render_stateful_widget(list, chunks[1], &mut state);
    }

    let help = app.message.as_deref().unwrap_or(match mode {
        LayoutMode::Narrow => " ↑↓/jk move  ↵ open  q quit",
        _ => " ↑↓ / j k  navigate    Enter  open terminal    Home/End  warp    q  disconnect",
    });
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(if app.message.is_some() {
            Color::Yellow
        } else {
            Color::DarkGray
        })),
        chunks[2],
    );
}

fn session_item(
    session: &SessionSummary,
    rate: Option<&RateState>,
    mode: LayoutMode,
    available_width: u16,
    now: Instant,
) -> ListItem<'static> {
    let status = status_glyph(&session.status, session.input_needed);
    let status_text = status_label(&session.status, session.input_needed);
    let name = session
        .title
        .as_deref()
        .filter(|title| !title.is_empty())
        .unwrap_or(&session.command);
    let age = super::list::format_age(session.created_at, session.started_at, session.ended_at);
    let current_rate = rate.map(|value| value.display_rate(now)).unwrap_or(0.0);
    let animation_age = rate
        .map(|value| now.duration_since(value.sampled_at))
        .unwrap_or_default();
    let rate_color = rate_color(current_rate, animation_age);
    let item = match mode {
        LayoutMode::Narrow => {
            let indicator =
                tiny_indicator(current_rate, session.status == "running", animation_age);
            let status_width = UnicodeWidthStr::width(status_text).clamp(5, 8);
            let fixed_width = 5
                + status_width
                + UnicodeWidthStr::width(age.as_str()).max(5)
                + UnicodeWidthStr::width(indicator.as_str());
            let name_width = (available_width as usize)
                .saturating_sub(fixed_width)
                .max(1);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", status.0), Style::default().fg(status.1)),
                Span::raw(format!("{} ", truncate(name, name_width))),
                Span::styled(
                    format!("{status_text:<status_width$} "),
                    Style::default().fg(status.1),
                ),
                Span::styled(format!("{:>5} ", age), Style::default().fg(Color::DarkGray)),
                Span::styled(indicator, Style::default().fg(rate_color)),
            ]))
        }
        LayoutMode::Medium => {
            let fixed_width = 2 + 9 + 7 + 13;
            let name_width = (available_width as usize)
                .saturating_sub(fixed_width)
                .clamp(8, 30);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", status.0), Style::default().fg(status.1)),
                Span::raw(pad_truncated(name, name_width)),
                Span::styled(
                    format!(" {:<8}", status_text),
                    Style::default().fg(status.1),
                ),
                Span::styled(
                    format!(" {:>5} ", age),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!(
                        "{} {:>6}/s",
                        rate_bar(current_rate, 4),
                        format_bytes(current_rate)
                    ),
                    Style::default().fg(rate_color),
                ),
            ]))
        }
        LayoutMode::Wide => {
            let command = if session.args.is_empty() {
                session.command.clone()
            } else {
                format!("{} {}", session.command, session.args.join(" "))
            };
            let fixed_width = 2 + 23 + 8 + 9 + 7 + 13 + 10;
            let command_width = (available_width as usize)
                .saturating_sub(fixed_width)
                .max(8);
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", status.0), Style::default().fg(status.1)),
                Span::raw(pad_truncated(name, 22)),
                Span::styled(
                    format!(
                        " {:>6} ",
                        session.pid.map_or("-".into(), |pid| pid.to_string())
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<8} ", status_text),
                    Style::default().fg(status.1),
                ),
                Span::styled(format!("{:>5} ", age), Style::default().fg(Color::DarkGray)),
                Span::raw(pad_truncated(&command, command_width)),
                Span::styled(
                    format!(
                        " {} {:>6}/s",
                        sparkline(rate, 5),
                        format_bytes(current_rate)
                    ),
                    Style::default().fg(rate_color),
                ),
                Span::styled(
                    format!(" {:>8}", format_bytes(session.last_total_bytes as f64)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        }
    };
    if session.status == "stopped" {
        item.style(
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )
    } else {
        item
    }
}

fn aggregate_sparkline(rates: &HashMap<String, RateState>, width: usize) -> String {
    let values = (0..width)
        .map(|index| {
            rates
                .values()
                .filter_map(|rate| rate.history.iter().rev().nth(width - index - 1))
                .sum::<f64>()
        })
        .collect::<Vec<_>>();
    let max = values.iter().copied().fold(1.0_f64, f64::max);
    values
        .into_iter()
        .map(|value| SPARK_BLOCKS[((value / max) * 7.0).round() as usize])
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
        .map(|value| SPARK_BLOCKS[((value / max) * 7.0).round() as usize])
        .collect::<String>();
    format!("{}{}", "▁".repeat(padding), spark)
}

fn rate_bar(rate: f64, width: usize) -> String {
    let level = if rate <= 0.0 {
        0
    } else {
        ((rate.log2() + 1.0) / 3.0).ceil().clamp(1.0, width as f64) as usize
    };
    format!("{}{}", "▰".repeat(level), "▱".repeat(width - level))
}

fn tiny_indicator(rate: f64, running: bool, animation_age: Duration) -> String {
    if rate > 0.0 {
        let phase = (animation_age.as_millis() / 40) as usize;
        SPARK_BLOCKS[(phase + ((rate.log2().max(0.0) as usize) % 5) + 3) % SPARK_BLOCKS.len()]
            .to_string()
    } else if running {
        "·".to_string()
    } else {
        "○".to_string()
    }
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
    if input_needed { "input" } else { status }
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

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
    command.args(["powershell.exe", "-NoProfile"]);
    if !attach {
        command.arg("-NoExit");
    }
    command.args(["-Command", &script]);
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
    use super::{App, WindowRect, arrange_window};
    use crate::protocol::SessionSummary;
    use chrono::{TimeZone, Utc};

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

    #[test]
    fn refresh_keeps_selection_by_id() {
        let mut app = App::default();
        app.replace_sessions(vec![session("a"), session("b")]);
        app.selected = 1;
        app.replace_sessions(vec![session("b"), session("c")]);
        assert_eq!(app.selected, 0);
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
    fn input_required_replaces_running_status_label() {
        assert_eq!(super::status_label("running", true), "input");
        assert_eq!(super::status_label("running", false), "running");
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
        assert_eq!(super::aggregate_sparkline(&app.rates, 3), "▁▆█");
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
}
