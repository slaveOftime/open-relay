use std::time::Instant;

use super::super::list::format_timestamp_local;
use super::app::{App, RateState, is_active_status, session_key};
use super::constants::SELECTED_ROW_BG;
use super::constants::SPARKLINE_WIDTH;
use super::table::{
    LayoutMode, pad_truncated, rate_color, session_status_style, sparkline, status_label,
};
use super::tree::{TreeEntry, TreeNode};
use crate::protocol::SessionSummary;
use chrono::{DateTime, Utc};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn render_tree(
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

/// Draw a real tree edge for each visible ancestor. Looking at the whole
/// visible list (not just the viewport) keeps vertical lines continuous when
/// the user scrolls past a sibling.
pub fn tree_connector(entries: &[TreeEntry], index: usize) -> String {
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

pub fn blank_line() -> Line<'static> {
    Line::from(String::new())
}

pub fn folder_line(node: &TreeNode, prefix: &str, selected: bool) -> Line<'static> {
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

pub const TREE_STATUS_WIDTH: usize = 9;

pub fn session_line(
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

pub fn format_tree_start(started: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(started);
    if elapsed.num_seconds() <= 0 {
        "just now".to_string()
    } else if elapsed.num_hours() >= 24 {
        format_timestamp_local(started)
    } else if elapsed.num_hours() > 0 {
        format!("{}h ago", elapsed.num_hours())
    } else if elapsed.num_minutes() > 0 {
        format!("{}m ago", elapsed.num_minutes())
    } else {
        format!("{}s ago", elapsed.num_seconds())
    }
}
