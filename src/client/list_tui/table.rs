use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::list::format_age;
use super::app::{RateState, is_active_status};
use super::constants::SELECTED_ROW_BG;
use super::constants::{COMPACT_SPARKLINE_WIDTH, SPARK_BLOCKS, SPARKLINE_WIDTH};
use crate::protocol::SessionSummary;
use crate::session::program_status::ProgramState;
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Row},
};

pub const SESSION_ID_WIDTH: usize = 8;
const RATE_WIDTH: u16 = (SPARKLINE_WIDTH + 7) as u16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LayoutMode {
    /// Identity and age only on very small terminals.
    Minimal,
    /// Add a compact activity chart.
    Narrow,
    /// Add a normal activity chart and numeric rate.
    Medium,
    /// Add PID, total output, and command arguments.
    Wide,
}

impl LayoutMode {
    pub fn for_width(width: u16) -> Self {
        match width {
            100.. => Self::Wide,
            62.. => Self::Medium,
            36.. => Self::Narrow,
            _ => Self::Minimal,
        }
    }
}

pub fn session_table_widths(mode: LayoutMode, show_node: bool) -> Vec<Constraint> {
    let mut widths = match mode {
        LayoutMode::Minimal => vec![
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(SESSION_ID_WIDTH as u16),
            Constraint::Length(4),
        ],
        LayoutMode::Narrow => vec![
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(SESSION_ID_WIDTH as u16),
            Constraint::Length(5),
            Constraint::Length(COMPACT_SPARKLINE_WIDTH as u16),
        ],
        LayoutMode::Medium => vec![
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(SESSION_ID_WIDTH as u16),
            Constraint::Length(5),
            Constraint::Length(RATE_WIDTH),
        ],
        LayoutMode::Wide => vec![
            Constraint::Length(1),
            Constraint::Length(22),
            Constraint::Length(6),
            Constraint::Length(SESSION_ID_WIDTH as u16),
            Constraint::Length(5),
            Constraint::Length(RATE_WIDTH),
            Constraint::Length(6),
            Constraint::Fill(1),
        ],
    };
    if show_node {
        widths.insert(1, Constraint::Length(8));
    }
    widths
}

/// Resolve once for both ellipsis truncation and table rendering. A single
/// selection-marker cell keeps header and row origins identical.
pub fn session_table_columns(
    mode: LayoutMode,
    show_node: bool,
    width: u16,
    sessions: &[SessionSummary],
) -> Vec<u16> {
    let mut constraints = session_table_widths(mode, show_node);
    if show_node {
        let node_width = sessions
            .iter()
            .map(|session| UnicodeWidthStr::width(session.node.as_deref().unwrap_or("local")))
            .max()
            .unwrap_or(5)
            .clamp(5, 12);
        // At small widths the title wins over a long machine name.
        let node_limit = match mode {
            LayoutMode::Minimal => 3,
            LayoutMode::Narrow => 5,
            _ => 12,
        };
        constraints[1] = Constraint::Length(node_width.min(node_limit) as u16);
    }
    if mode == LayoutMode::Wide {
        let title_width = sessions
            .iter()
            .map(|session| {
                UnicodeWidthStr::width(session_title(session).as_str())
                    + session_program_status_span(session).map_or(0, |span| span.width() + 1)
            })
            .max()
            .unwrap_or(18)
            .clamp(18, 28);
        constraints[1 + usize::from(show_node)] = Constraint::Length(title_width as u16);
    }
    Layout::horizontal(constraints)
        .spacing(1)
        .split(Rect::new(0, 0, width.saturating_sub(1), 1))
        .iter()
        .map(|column| column.width)
        .collect()
}

pub fn session_table_alignments(mode: LayoutMode, show_node: bool) -> Vec<Alignment> {
    let (left, right) = (Alignment::Left, Alignment::Right);
    let mut alignments = match mode {
        LayoutMode::Minimal => vec![left, left, left, right],
        LayoutMode::Narrow => vec![left, left, left, right, left],
        LayoutMode::Medium => vec![left, left, left, right, right],
        LayoutMode::Wide => vec![left, left, right, left, right, right, right, left],
    };
    if show_node {
        alignments.insert(1, left);
    }
    alignments
}

pub fn session_table_header(mode: LayoutMode, show_node: bool) -> Row<'static> {
    let mut labels = match mode {
        LayoutMode::Minimal => vec!["", "SESSION", "ID", "AGE"],
        LayoutMode::Narrow => vec!["", "SESSION", "ID", "AGE", "I/O"],
        LayoutMode::Medium => vec!["", "SESSION", "ID", "AGE", "RATE"],
        LayoutMode::Wide => vec![
            "", "SESSION", "PID", "ID", "AGE", "RATE", "OUTPUT", "COMMAND",
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

/// Show the executable rather than its install directory so arguments remain
/// visible. Remote nodes may use either separator; stored commands are unchanged.
pub fn session_command_label(session: &SessionSummary) -> String {
    let command = session
        .command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&session.command);
    if session.args.is_empty() {
        command.to_string()
    } else {
        format!("{} {}", command, session.args.join(" "))
    }
}

pub fn session_title(session: &SessionSummary) -> String {
    session
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            session
                .command
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&session.command)
                .to_string()
        })
}

/// Program state is independent of process lifecycle and input heuristics.
/// Render only the typed state, never arbitrary report text or guessed labels.
pub fn session_program_status_span(session: &SessionSummary) -> Option<Span<'static>> {
    let report = session.program_status.as_ref()?;
    // Use distinct, bright theme-aware ANSI colors so reports do not blend
    // into title/metadata gray or inherit a session's own terminal foreground.
    let (label, color) = match report.state {
        ProgramState::Idle => ("idle", Color::LightBlue),
        ProgramState::Working => ("working", Color::LightCyan),
        ProgramState::Done => ("done", Color::LightGreen),
        ProgramState::Blocked => ("blocked", Color::LightYellow),
        ProgramState::Error => ("error", Color::LightRed),
    };
    let mut style = Style::default().fg(color);
    if matches!(report.state, ProgramState::Blocked | ProgramState::Error) {
        style = style.add_modifier(Modifier::BOLD);
    }
    Some(Span::styled(format!("[{label}]"), style))
}

/// Keep an entire state badge beside the title when both can fit. Extremely
/// narrow cells retain identity rather than showing a misleading partial state.
pub fn session_title_spans(session: &SessionSummary, width: usize) -> Vec<Span<'static>> {
    let title = session_title(session);
    if let Some(badge) = session_program_status_span(session)
        && width >= badge.width() + 2
    {
        vec![
            Span::raw(truncate(&title, width - badge.width() - 1)),
            Span::raw(" "),
            badge,
        ]
    } else {
        vec![Span::raw(truncate(&title, width))]
    }
}

pub fn aligned_cell(content: impl Into<Line<'static>>, alignment: Alignment) -> Cell<'static> {
    Cell::from(content.into().alignment(alignment))
}

/// Shared semantic styling for the table and the tree. The attention glyph
/// and label stay bold/yellow on a selected or pulsing row.
pub fn session_status_style(session: &SessionSummary, selected: bool) -> (&'static str, Style) {
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
pub fn session_row(
    session: &SessionSummary,
    rate: Option<&RateState>,
    mode: LayoutMode,
    now: Instant,
    show_node: bool,
    selected: bool,
    column_widths: &[u16],
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
    let name = session_title(session);
    let age = format_age(session.created_at, session.started_at, session.ended_at);
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
    let text_cell = |text: &str, index: usize| {
        aligned_cell(
            truncate(
                text,
                column_widths.get(index).copied().unwrap_or(0) as usize,
            ),
            alignments[index],
        )
    };
    let rate_text = if active {
        format!(
            "{} {:>4}/s",
            sparkline(rate, SPARKLINE_WIDTH),
            format_bytes(current_rate)
        )
    } else {
        String::new()
    };
    let name_index = 1 + node_offset;
    let name_width = column_widths.get(name_index).copied().unwrap_or(0) as usize;
    let command = session_command_label(session);
    // Medium rows have a generous SESSION column but no COMMAND column.
    // Use otherwise empty space for a subdued preview, without sacrificing
    // a useful title or repeating the executable when it is also the title.
    let name_cell = if mode == LayoutMode::Medium && name_width >= 28 && command != name {
        let title_width = (name_width * 3 / 5).min(26);
        let mut title = session_title_spans(session, title_width);
        let command_width =
            name_width.saturating_sub(title.iter().map(Span::width).sum::<usize>() + 3);
        title.extend([
            Span::styled(" · ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                truncate(&command, command_width),
                Style::default().fg(if selected {
                    Color::Gray
                } else {
                    Color::DarkGray
                }),
            ),
        ]);
        aligned_cell(Line::from(title), alignments[name_index])
    } else {
        aligned_cell(
            Line::from(session_title_spans(session, name_width)),
            alignments[name_index],
        )
    };
    // Finished sessions remain easy to identify without brightening their
    // command preview or metadata. Selected titles inherit the white row style.
    let name_cell = if !active && !selected {
        name_cell.style(Style::default().fg(Color::Gray))
    } else {
        name_cell
    };
    let mut cells = vec![
        aligned_cell(Span::styled(status_glyph, status_style), alignments[0]),
        name_cell,
    ];
    if mode == LayoutMode::Wide {
        cells.push(
            text_cell(
                &session.pid.map_or("-".into(), |pid| pid.to_string()),
                2 + node_offset,
            )
            .style(Style::default().fg(muted)),
        );
    }
    let id_index = cells.len() + node_offset;
    cells.push(text_cell(&session.id, id_index).style(status_style));
    cells.push(text_cell(&age, id_index + 1).style(Style::default().fg(muted)));
    if mode != LayoutMode::Minimal {
        let chart = if mode == LayoutMode::Narrow {
            if active {
                sparkline(rate, COMPACT_SPARKLINE_WIDTH)
            } else {
                String::new()
            }
        } else {
            rate_text
        };
        cells.push(text_cell(&chart, id_index + 2).style(Style::default().fg(rate_color)));
    }
    if mode == LayoutMode::Wide {
        cells.push(
            text_cell(&format_bytes(session.last_total_bytes as f64), id_index + 3)
                .style(Style::default().fg(muted)),
        );
        cells.push(text_cell(&session_command_label(session), id_index + 4));
    }
    if show_node {
        cells.insert(
            1,
            text_cell(session.node.as_deref().unwrap_or("local"), 1)
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

pub fn aggregate_sparkline_data(rates: &HashMap<String, RateState>, width: usize) -> Vec<u64> {
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

pub fn sparkline(rate: Option<&RateState>, width: usize) -> String {
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

pub fn rate_color(rate: f64, animation_age: Duration) -> Color {
    if rate <= 0.0 {
        Color::DarkGray
    } else {
        let pulse = ((animation_age.as_millis() / 40) % 5) as u8;
        Color::Rgb(45 + pulse * 8, 190 + pulse * 8, 180 + pulse * 10)
    }
}

pub fn format_bytes(bytes: f64) -> String {
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

pub fn pad_truncated(value: &str, width: usize) -> String {
    let value = truncate(value, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(value.as_str()));
    format!("{value}{}", " ".repeat(padding))
}

/// Short attention-vs-status label for list rows. The tree renderer uses
/// `session_status_style` directly; this helper stays for the table
/// renderers' tests and the `list_tui` re-export surface.
#[allow(dead_code)]
pub fn status_label(status: &str, input_needed: bool) -> &str {
    if input_needed { "attention" } else { status }
}

/// Parse a terminal colour spec (as reported by a session's `OSC 10`/`OSC 11`
/// replies, e.g. `#rrggbb`, X11 `rgb:r/g/b` / `rgbi:r/g/b`, or a colour name)
/// into a ratatui colour. Unrecognised specs fall back to the default style.
pub fn parse_terminal_color(spec: &str) -> Option<Color> {
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
pub fn parse_hex_color(hex: &str) -> Option<Color> {
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
pub fn scale_hex_component(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some(((value * 0xffff / max) >> 8) as u8)
}

/// A practical subset of the X11 colour names (per `rgb.txt`) covering the
/// names terminal colour schemes typically report.
pub fn named_color(name: &str) -> Option<Color> {
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

pub fn status_glyph(status: &str, input_needed: bool) -> (&'static str, Color) {
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

pub fn truncate(value: &str, width: usize) -> String {
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
