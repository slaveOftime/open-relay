use std::time::Instant;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline,
        Table, TableState,
    },
};

use super::app::App;
use super::app::session_key;
use super::dialog_render::{
    render_clone_dialog, render_remove_dialog, render_resume_dialog, render_update_dialog,
};
use super::effects::render_effects;
use super::table::{
    LayoutMode, aggregate_sparkline_data, column_maxima, format_bytes, rate_color, session_row,
    session_table_columns_with_maxima, session_table_header, truncate,
};
use super::tree::ViewMode;
use super::tree_render::render_tree;
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let mode = LayoutMode::for_width(area.width);
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
    // Rates are keyed by session and pruned with the session list in
    // `replace_sessions`, so the aggregate can iterate values directly
    // instead of deriving one key string per session per frame.
    let throughput = app
        .rates
        .values()
        .map(|rate| rate.display_rate(now))
        .sum::<f64>();
    let throughput_width = if area.width >= 80 { 14 } else { 7 };
    let header = Layout::horizontal([Constraint::Min(0), Constraint::Length(throughput_width)])
        .split(chunks[0]);
    let badge = if area.width >= 62 {
        " ◉ OPEN RELAY "
    } else {
        " oly "
    };
    let view_label = match app.view_mode {
        ViewMode::List => "list",
        ViewMode::Tree => "tree",
    };
    let summary = if area.width >= 100 {
        format!(
            " {} sessions · {} live · {} · {} · sort:{}",
            app.sessions.len(),
            running,
            app.status_filter.label(),
            view_label,
            app.sort_strategy.label()
        )
    } else if area.width >= 62 {
        format!(
            " {} / {} live · {} · {} · {}",
            app.sessions.len(),
            running,
            app.status_filter.label(),
            view_label,
            app.sort_strategy.label()
        )
    } else {
        format!(
            " {} / {} live · {} · {}",
            app.sessions.len(),
            running,
            app.status_filter.label(),
            view_label
        )
    };
    let summary_width =
        (header[0].width as usize).saturating_sub(unicode_width::UnicodeWidthStr::width(badge));
    let title = Line::from(vec![
        Span::styled(
            badge,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate(&summary, summary_width),
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
    let throughput_header = Layout::horizontal([
        Constraint::Length(if area.width >= 80 { 6 } else { 0 }),
        Constraint::Min(0),
    ])
    .split(header[1]);
    frame.render_widget(
        Sparkline::default()
            .data(aggregate_sparkline_data(&app.rates, 5))
            .style(Style::default().fg(rate_color(throughput, animation_age)))
            .block(header_block()),
        throughput_header[0],
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            format!("{}/s", format_bytes(throughput)),
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
        let viewport_end = (viewport_start + viewport_len).min(visible.len());
        let show_scrollbar = visible.len() > viewport_len;
        let table_area = if show_scrollbar {
            Rect {
                width: chunks[1].width.saturating_sub(1),
                ..chunks[1]
            }
        } else {
            chunks[1]
        };
        if !app.column_maxima_valid {
            let (node_max, title_max) = column_maxima(&app.sessions);
            app.node_column_max = node_max;
            app.title_column_max = title_max;
            app.column_maxima_valid = true;
        }
        let columns = session_table_columns_with_maxima(
            mode,
            app.show_node,
            table_area.width,
            app.node_column_max,
            app.title_column_max,
        );
        // Rows are built only for the viewport slice; the table then renders
        // the slice's own window and the selection is expressed relative to
        // it. Building a `Row` for every one of thousands of sessions was the
        // dominant cost of a list-mode frame.
        let rows = visible[viewport_start..viewport_end]
            .iter()
            .enumerate()
            .filter_map(|(offset, index)| {
                let session = app.sessions.get(*index)?;
                Some(session_row(
                    session,
                    app.rate_for(*index, session),
                    mode,
                    now,
                    app.show_node,
                    viewport_start + offset == selected_position,
                    &columns,
                ))
            });
        // Cell styling preserves semantic status colours on the selection band.
        // One marker cell also makes headers line up exactly with their rows.
        let table = Table::new(rows, columns.iter().copied().map(Constraint::Length))
            .header(session_table_header(mode, app.show_node))
            .column_spacing(1)
            .highlight_symbol("▸");
        let mut state = TableState::new()
            .with_offset(0)
            .with_selected(Some(selected_position.saturating_sub(viewport_start)));
        frame.render_stateful_widget(table, table_area, &mut state);

        // Record where each waiting session's row actually landed (one line
        // below the header, offset by the scroll position) so the attention
        // pulse can be scoped to exactly those rows. A session with
        // notifications disabled never pulses, even while it waits for
        // input: the animation is an attention signal, and the user opted
        // out of attention signals for that session. Only viewport rows can
        // land on screen, so only they need rects.
        for offset in 0..viewport_end - viewport_start {
            let Some(session) = app.sessions.get(visible[viewport_start + offset]) else {
                continue;
            };
            if !session.input_needed || !session.notifications_enabled {
                continue;
            }
            attention_row_rects.push((
                session_key(session),
                Rect {
                    x: table_area.x,
                    y: table_area.y + 1 + offset as u16,
                    width: 3,
                    height: 1,
                },
                viewport_start + offset == selected_position,
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

    // Prioritised hints fit the actual remaining width, not a coarse layout
    // breakpoint. A long message cannot consume all navigation help.
    let message_width = app
        .message
        .as_deref()
        .map(|message| {
            unicode_width::UnicodeWidthStr::width(message).min(area.width as usize * 2 / 5) as u16
        })
        .unwrap_or(0);
    let footer = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(if message_width > 0 {
            message_width + 1
        } else {
            0
        }),
    ])
    .split(chunks[2]);
    let help = footer_help(app, footer[0].width as usize);
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        footer[0],
    );
    if let Some(message) = app.message.as_deref() {
        frame.render_widget(
            Paragraph::new(truncate(message, message_width as usize))
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

    if let Some(dialog) = app.remove_dialog.as_ref() {
        render_remove_dialog(frame, dialog);
    } else if let Some(dialog) = app.resume_dialog.as_ref() {
        render_resume_dialog(frame, dialog);
    } else if let Some(dialog) = app.clone_dialog.as_ref() {
        render_clone_dialog(frame, dialog);
    } else if let Some(dialog) = app.update_dialog.as_ref() {
        render_update_dialog(frame, dialog);
    }

    render_effects(frame, app, message_area, attention_row_rects);
}

fn footer_help(app: &App, width: usize) -> String {
    if !app.filter.is_empty() {
        return truncate(
            &format!(
                " filter: {}_ · ⌫ edit · esc clear · ^S {}",
                app.filter,
                app.status_filter.label()
            ),
            width,
        );
    }
    let toggle = match app.view_mode {
        ViewMode::List => "^G tree",
        ViewMode::Tree => "^G list",
    };
    let open = if app.view_mode == ViewMode::Tree && app.focused_session().is_none() {
        "⏎ expand"
    } else {
        "⏎ open"
    };
    let hints = [
        "^N new",
        open,
        toggle,
        "^C quit",
        "type filter",
        "^K stop",
        "^S status",
        "^O sort",
        "^D dup",
        "^U update",
        "^Del rm",
        "^⏎ window",
    ];
    let mut help = String::from(" ");
    for hint in hints {
        let separator = if help.len() > 1 { " · " } else { "" };
        if unicode_width::UnicodeWidthStr::width(help.as_str())
            + unicode_width::UnicodeWidthStr::width(separator)
            + unicode_width::UnicodeWidthStr::width(hint)
            > width
        {
            continue;
        }
        help.push_str(separator);
        help.push_str(hint);
    }
    truncate(&help, width)
}
