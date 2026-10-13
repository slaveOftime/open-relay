use chrono::{DateTime, Utc};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Shadow},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::constants::{
    CLONE_DIALOG_HELP, DIALOG_FIELD_BG, DIALOG_LABEL_WIDTH, REMOVE_DIALOG_HELP, RESUME_DIALOG_HELP,
    UPDATE_DIALOG_HELP,
};
use super::dialog::format_terminal_words;
use super::dialog::{
    CloneDialog, CloneField, EditText, RemoveDialog, ResumeDialog, ResumeField, UPDATE_FIELDS,
    UpdateDialog, UpdateField,
};
use super::table::{format_bytes, pad_truncated};
use crate::protocol::SessionSummary;

pub fn render_clone_dialog(frame: &mut Frame<'_>, dialog: &CloneDialog) {
    let area = centered_rect(
        frame.area(),
        96,
        if dialog.source_id.is_some() { 20 } else { 19 },
    );
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
        .chain(
            dialog
                .source_id
                .is_some()
                .then_some(CloneField::RemoveOriginal),
        )
        .map(field_line),
    );
    lines.push(tip_separator(area.width));
    lines.push(dialog_footer(dialog.error.as_deref(), CLONE_DIALOG_HELP));
    render_dialog(
        frame,
        area,
        dialog.source_id.as_ref().map_or_else(
            || " ✚ New Session ".to_string(),
            |source_id| {
                format!(
                    " ⧉ Duplicate {source_id} ({}) ",
                    dialog.source_node.as_deref().unwrap_or("local")
                )
            },
        ),
        Color::Cyan,
        lines,
    );
}

fn resume_field_line(
    dialog: &ResumeDialog,
    field: ResumeField,
    width: u16,
    cursor_visible: bool,
) -> Line<'static> {
    let active = dialog.active_field() == field;
    let value_width = dialog_value_width(width);
    let (label, value_spans) = match field {
        ResumeField::Command => (
            "Command",
            text_value_spans(
                &dialog.command,
                active,
                value_width,
                cursor_visible,
                "‹required›",
            ),
        ),
        ResumeField::Args => (
            "Arguments",
            text_value_spans(&dialog.args, active, value_width, cursor_visible, "‹none›"),
        ),
        ResumeField::Cwd => (
            "Directory",
            text_value_spans(
                &dialog.cwd,
                active,
                value_width,
                cursor_visible,
                "‹default›",
            ),
        ),
        ResumeField::Title => (
            "Title",
            text_value_spans(&dialog.title, active, value_width, cursor_visible, "‹auto›"),
        ),
        ResumeField::Tags => (
            "Tags",
            text_value_spans(&dialog.tags, active, value_width, cursor_visible, "‹none›"),
        ),
        ResumeField::Node => (
            "Node",
            text_value_spans(&dialog.node, active, value_width, cursor_visible, "‹local›"),
        ),
        ResumeField::Rows => (
            "Rows",
            text_value_spans(&dialog.rows, active, value_width, cursor_visible, "‹auto›"),
        ),
        ResumeField::Cols => (
            "Columns",
            text_value_spans(&dialog.cols, active, value_width, cursor_visible, "‹auto›"),
        ),
        ResumeField::DisableNotifications => (
            "Notifications",
            checkbox_spans(!dialog.disable_notifications, active, None),
        ),
        ResumeField::AttachAfterStart => (
            "Attach",
            checkbox_spans(dialog.attach_after_start, active, Some("after start")),
        ),
        ResumeField::RemoveOriginal => (
            "Remove original",
            checkbox_spans(
                dialog.remove_original,
                active,
                Some("force after successful start"),
            ),
        ),
    };
    dialog_field_line(active, label, value_spans)
}

pub fn render_resume_dialog(frame: &mut Frame<'_>, dialog: &ResumeDialog) {
    let area = centered_rect(frame.area(), 96, 20);
    let cursor_visible = clone_cursor_visible();
    let field_line = |field| resume_field_line(dialog, field, area.width, cursor_visible);
    let mut lines = vec![section_header("PROCESS")];
    lines.extend(
        [ResumeField::Command, ResumeField::Args, ResumeField::Cwd]
            .into_iter()
            .map(field_line),
    );
    lines.push(Line::default());
    lines.push(section_header("METADATA"));
    lines.extend(
        [ResumeField::Title, ResumeField::Tags, ResumeField::Node]
            .into_iter()
            .map(field_line),
    );
    lines.push(Line::default());
    lines.push(section_header("OPTIONS"));
    lines.extend(
        [
            ResumeField::Rows,
            ResumeField::Cols,
            ResumeField::DisableNotifications,
            ResumeField::AttachAfterStart,
            ResumeField::RemoveOriginal,
        ]
        .into_iter()
        .map(field_line),
    );
    lines.push(tip_separator(area.width));
    lines.push(dialog_footer(dialog.error.as_deref(), RESUME_DIALOG_HELP));
    render_dialog(
        frame,
        area,
        format!(
            " ↻ Resume {} ({}) ",
            dialog.source.id,
            dialog.source_node.as_deref().unwrap_or("local")
        ),
        Color::Cyan,
        lines,
    );
}
pub fn render_update_dialog(frame: &mut Frame<'_>, dialog: &UpdateDialog) {
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

pub fn render_remove_dialog(frame: &mut Frame<'_>, dialog: &RemoveDialog) {
    let area = centered_rect(frame.area(), 100, 8);

    let lines = vec![
        Line::from(format!(" {}", dialog.prompt)),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                dialog.detail.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::default(),
        dialog_footer(None, REMOVE_DIALOG_HELP),
    ];
    render_dialog(
        frame,
        area,
        " ⚠ Remove session? ".to_string(),
        Color::Red,
        lines,
    );
}

pub fn tip_separator(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "\u{2500}".repeat(width.saturating_sub(2) as usize),
        Style::default().fg(Color::DarkGray),
    ))
}

pub fn dialog_footer<'a>(error: Option<&'a str>, help: &'static str) -> Line<'a> {
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

pub fn render_dialog<'a>(
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
pub fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    ))
}

/// The width available to a field value inside a dialog of `width` cells.
pub fn dialog_value_width(width: u16) -> usize {
    (width as usize).saturating_sub(2 + DIALOG_LABEL_WIDTH + 2 + 2)
}

/// Gutter marker + label + gap shared by every dialog field row. The active
/// field is marked with `▸` and a bright label; its value sits on a subtle
/// background so it reads as a focused input box.
pub fn dialog_field_line(
    active: bool,
    label: &str,
    value_spans: Vec<Span<'static>>,
) -> Line<'static> {
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
pub fn text_value_spans(
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
pub fn checkbox_spans(
    checked: bool,
    active: bool,
    suffix: Option<&'static str>,
) -> Vec<Span<'static>> {
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

pub fn update_field_line(
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

pub fn update_read_only_line(label: &str, value: &str, width: u16) -> Line<'static> {
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

pub fn update_read_only_values(summary: &SessionSummary) -> Vec<(&'static str, String)> {
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
            super::super::list::format_timestamp_local(summary.created_at),
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

pub fn display_words(words: &[String]) -> String {
    if words.is_empty() {
        "—".to_string()
    } else {
        format_terminal_words(words)
    }
}

pub fn format_dialog_timestamp(timestamp: Option<&DateTime<Utc>>) -> String {
    timestamp.map_or_else(
        || "—".to_string(),
        |timestamp| super::super::list::format_timestamp_local(*timestamp),
    )
}

pub fn centered_rect(area: Rect, max_width: u16, max_height: u16) -> Rect {
    let width = area.width.saturating_sub(2).min(max_width).max(1);
    let height = area.height.saturating_sub(2).min(max_height).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn clone_field_line(
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
        CloneField::RemoveOriginal => (
            "Remove original",
            checkbox_spans(
                dialog.remove_original,
                active,
                Some("force after successful start"),
            ),
        ),
    };
    dialog_field_line(active, label, value_spans)
}

pub fn edit_text_viewport(field: &EditText, width: usize, cursor_visible: bool) -> String {
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

pub fn clone_cursor_visible() -> bool {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(true, |elapsed| (elapsed.as_millis() / 500) % 2 == 0)
}

pub fn checkbox(checked: bool) -> String {
    if checked {
        "[x]".to_string()
    } else {
        "[ ]".to_string()
    }
}
