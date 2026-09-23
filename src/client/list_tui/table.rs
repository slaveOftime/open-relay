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
use ratatui::{
    layout::{Alignment, Constraint},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Row},
};

#[derive(Clone, Copy)]
pub enum LayoutMode {
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

pub fn session_table_widths(mode: LayoutMode, show_node: bool) -> Vec<Constraint> {
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

pub fn session_table_alignments(mode: LayoutMode, show_node: bool) -> Vec<Alignment> {
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

pub fn session_table_header(mode: LayoutMode, show_node: bool) -> Row<'static> {
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
                    String::new()
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
                    String::new()
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
                        String::new()
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
