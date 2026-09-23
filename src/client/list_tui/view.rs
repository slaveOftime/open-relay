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
use super::dialog_render::{render_clone_dialog, render_update_dialog};
use super::effects::render_effects;
use super::table::{
    LayoutMode, aggregate_sparkline_data, format_bytes, rate_color, session_row,
    session_table_header, session_table_widths,
};
use super::tree::ViewMode;
use super::tree_render::render_tree;
pub fn render(frame: &mut Frame<'_>, app: &mut App) {
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
