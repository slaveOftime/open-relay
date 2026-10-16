use std::time::Instant;

use super::super::list::format_timestamp_local;
use super::app::{App, RateState, is_active_status, session_key};
use super::constants::{SELECTED_ROW_BG, SPARKLINE_WIDTH};
use super::table::{
    LayoutMode, SESSION_ID_WIDTH, format_bytes, rate_color, session_command_label,
    session_program_status_span, session_status_style, session_title, session_title_spans,
    sparkline, truncate,
};
use super::tree::{TreeEntry, TreeNode, TreeView};
use crate::protocol::SessionSummary;
use chrono::{DateTime, Utc};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn render_tree(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &mut App,
    _mode: LayoutMode,
    now: Instant,
    attention_row_rects: &mut Vec<(String, Rect, bool)>,
) -> (usize, usize, u16) {
    // Keep the file-explorer presentation, but budget every row before it
    // reaches Paragraph: connector, icon/id, title, compact metrics, then command.
    let total = app.tree.visible.len();
    if total == 0 {
        frame.render_widget(
            Paragraph::new("  (no sessions)").style(Style::default().fg(Color::DarkGray)),
            area,
        );
        return (0, 0, 0);
    }

    // Centre the cursor inside the viewport so drill toggles feel snappy.
    let folder_colors = folder_status_colors(&app.tree, &app.sessions);
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
            TreeEntry::Folder { node, depth } => {
                let status_color = folder_colors[node];
                let connector = tree_connector_line(
                    &app.tree.visible,
                    absolute_index,
                    &folder_colors,
                    is_selected,
                );
                let expandable = !app.tree.nodes[node].subfolders.is_empty()
                    || !app.tree.nodes[node].direct_sessions.is_empty();
                let expanded = app.tree.is_expanded(node, depth);
                lines.push(folder_line_with_width(
                    &app.tree.nodes[node],
                    connector,
                    is_selected,
                    status_color,
                    expandable,
                    expanded,
                    area.width as usize,
                ));
            }
            TreeEntry::Session { session, .. } => {
                let Some(session_summary) = app.sessions.get(session) else {
                    lines.push(blank_line());
                    continue;
                };
                let connector = tree_connector_line(
                    &app.tree.visible,
                    absolute_index,
                    &folder_colors,
                    is_selected,
                );
                let (line, connector_width) = session_line_with_connector(
                    session_summary,
                    app.rates.get(&session_key(session_summary)),
                    connector,
                    is_selected,
                    now,
                    area.width as usize,
                );
                lines.push(line);
                if area.width > 0
                    && session_summary.input_needed
                    && session_summary.notifications_enabled
                {
                    // The connector may have hidden ancestors. Pulse the
                    // actual rendered icon, never the unbounded tree depth.
                    attention_row_rects.push((
                        session_key(session_summary),
                        Rect {
                            x: area.x + connector_width as u16,
                            y: area.y + row as u16,
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

/// Whether another visible row continues this branch at the requested depth.
fn has_next_sibling(entries: &[TreeEntry], index: usize, level: usize) -> bool {
    entries[index + 1..]
        .iter()
        .find(|entry| entry.depth() <= level)
        .is_some_and(|entry| entry.depth() == level)
}

/// Unclipped connectors use exactly two cells per depth, including the leaf
/// branch. Whole-list sibling lookup keeps rails continuous while scrolling.
#[cfg(test)]
pub fn tree_connector(entries: &[TreeEntry], index: usize) -> String {
    tree_connector_line(entries, index, &[], false)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Aggregate status across a folder and all of its descendants. Attention
/// takes precedence over running so a collapsed branch still signals the
/// most actionable session it contains.
pub(super) fn folder_status_colors(
    tree: &TreeView,
    sessions: &[SessionSummary],
) -> Vec<Option<Color>> {
    fn visit(
        node_index: usize,
        tree: &TreeView,
        sessions: &[SessionSummary],
        colors: &mut [Option<Color>],
    ) -> (bool, bool) {
        let node = &tree.nodes[node_index];
        let mut needs_attention = false;
        let mut has_active = false;
        for &session_index in &node.direct_sessions {
            if let Some(session) = sessions.get(session_index) {
                needs_attention |= session.input_needed;
                has_active |= is_active_status(&session.status);
            }
        }
        for &child_index in &node.subfolders {
            let (child_attention, child_active) = visit(child_index, tree, sessions, colors);
            needs_attention |= child_attention;
            has_active |= child_active;
        }
        colors[node_index] = if needs_attention {
            Some(Color::Yellow)
        } else if has_active {
            Some(Color::Green)
        } else {
            None
        };
        (needs_attention, has_active)
    }

    let mut colors = vec![None; tree.nodes.len()];
    if !tree.nodes.is_empty() {
        visit(tree.root, tree, sessions, &mut colors);
    }
    colors
}

/// Only the current folder's branch carries aggregate state. Ancestor rails
/// stay neutral so an active sibling does not brighten inactive sessions.
pub(super) fn tree_connector_line(
    entries: &[TreeEntry],
    index: usize,
    folder_colors: &[Option<Color>],
    selected: bool,
) -> Line<'static> {
    let entry = entries[index];
    let depth = entry.depth();
    if depth == 0 {
        return blank_line();
    }
    let (branch_color, is_folder) = match entry {
        TreeEntry::Folder { node, .. } => (folder_colors.get(node).copied().flatten(), true),
        TreeEntry::Session { .. } => (None, false),
    };
    let selected_color = if selected && is_folder {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let mut spans = Vec::with_capacity(depth);
    for level in 1..depth {
        let connector = if has_next_sibling(entries, index, level) {
            "│ "
        } else {
            "  "
        };
        spans.push(Span::styled(
            connector,
            Style::default().fg(Color::DarkGray),
        ));
    }
    let branch = if has_next_sibling(entries, index, depth) {
        "├─"
    } else {
        "└─"
    };
    spans.push(Span::styled(
        branch,
        Style::default().fg(branch_color.unwrap_or(selected_color)),
    ));
    Line::from(spans)
}

// Icon, gap, ID (at most eight cells), gap. No identifier padding.
pub const TREE_STATUS_WIDTH: usize = SESSION_ID_WIDTH;
const TREE_IDENTITY_WIDTH: usize = 1 + 1 + SESSION_ID_WIDTH + 1;
const TREE_MIN_TITLE_WIDTH: usize = 8;
const TREE_AGE_WIDTH: usize = 3;
#[cfg(test)]
const TREE_DEFAULT_WIDTH: usize = 120;

fn connector_budget(width: usize, age_width: usize) -> usize {
    // At most a quarter of the viewport belongs to ancestors. Also reserve
    // an identifying title even at the 24-cell breakpoint. A one-cell budget
    // uses just an ellipsis; otherwise clipped rails retain 2-cell segments.
    (width / 4)
        .min(width.saturating_sub(TREE_IDENTITY_WIDTH + TREE_MIN_TITLE_WIDTH + 1 + age_width))
}

fn fit_connector(connector: Line<'static>, budget: usize) -> Line<'static> {
    if connector.width() <= budget {
        return connector;
    }
    if budget == 0 {
        return blank_line();
    }
    let rail_style = Style::default().fg(Color::DarkGray);
    if budget == 1 {
        return Line::from(Span::styled("…", rail_style));
    }

    let suffix_width = (budget.saturating_sub(2) / 2) * 2;
    let mut skip = connector.width().saturating_sub(suffix_width);
    let mut spans = vec![Span::styled("… ", rail_style)];
    for span in connector.spans {
        let span_width = span.width();
        if skip >= span_width {
            skip -= span_width;
            continue;
        }
        let mut content = String::new();
        for character in span.content.chars() {
            if skip > 0 {
                skip = skip.saturating_sub(UnicodeWidthChar::width(character).unwrap_or(0));
            } else {
                content.push(character);
            }
        }
        if !content.is_empty() {
            spans.push(Span::styled(content, span.style));
        }
    }
    Line::from(spans)
}

pub fn blank_line() -> Line<'static> {
    Line::from(String::new())
}

// Preserve the existing test helper API; the actual viewport supplies its
// width to folder_line_with_width instead of relying on Paragraph clipping.
#[cfg(test)]
pub fn folder_line(
    node: &TreeNode,
    prefix: Line<'static>,
    selected: bool,
    status_color: Option<Color>,
    expandable: bool,
    expanded: bool,
) -> Line<'static> {
    folder_line_with_width(
        node,
        prefix,
        selected,
        status_color,
        expandable,
        expanded,
        TREE_DEFAULT_WIDTH,
    )
}

fn folder_line_with_width(
    node: &TreeNode,
    prefix: Line<'static>,
    selected: bool,
    status_color: Option<Color>,
    expandable: bool,
    expanded: bool,
    width: usize,
) -> Line<'static> {
    let prefix = fit_connector(prefix, connector_budget(width, TREE_AGE_WIDTH));
    let label_width = width.saturating_sub(prefix.width());
    let label = if node.is_node {
        format!("{} (node)", node.name)
    } else if node.name.is_empty() {
        // Defensive fallback: empty names must still give a breadcrumb.
        node.path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string()
    } else {
        format!("{}/", node.name)
    };
    let expander = if expandable {
        if expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };
    let label = if label_width < 2 {
        truncate(expander, label_width)
    } else {
        format!("{expander}{}", truncate(&label, label_width - 2))
    };
    let label_color = status_color.unwrap_or(if node.is_node {
        Color::Cyan
    } else if selected {
        Color::White
    } else {
        Color::Gray
    });
    let label_style = Style::default().fg(label_color).add_modifier(if selected {
        Modifier::BOLD
    } else {
        Modifier::empty()
    });
    let mut spans = prefix.spans;
    spans.push(Span::styled(label, label_style));
    selected_line(spans, selected)
}

#[cfg(test)]
pub fn session_line(
    session: &SessionSummary,
    rate: Option<&RateState>,
    prefix: &str,
    selected: bool,
    now: Instant,
) -> Line<'static> {
    session_line_with_connector(
        session,
        rate,
        Line::from(Span::styled(
            prefix.to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        selected,
        now,
        TREE_DEFAULT_WIDTH,
    )
    .0
}

/// Return the rendered connector width alongside the row so attention effects
/// use exactly the same cell position, including clipped/deep connectors.
fn session_line_with_connector(
    session: &SessionSummary,
    rate: Option<&RateState>,
    connector: Line<'static>,
    selected: bool,
    now: Instant,
    width: usize,
) -> (Line<'static>, usize) {
    let active = is_active_status(&session.status);
    let (glyph, status_style) = session_status_style(session, selected);
    let title_style = Style::default().fg(if selected { Color::White } else { Color::Gray });
    let dim = Style::default().fg(if selected {
        Color::White
    } else {
        Color::DarkGray
    });
    let age = compact_tree_start(session.started_at.unwrap_or(session.created_at), Utc::now());
    let connector = fit_connector(connector, connector_budget(width, age.len()));
    let connector_width = connector.width();
    let available = width.saturating_sub(connector_width);
    let mut spans = connector.spans;
    if available == 0 {
        return (selected_line(spans, selected), connector_width);
    }
    spans.push(Span::styled(glyph, status_style));
    if available <= 2 {
        return (selected_line(spans, selected), connector_width);
    }
    spans.push(Span::raw(" "));
    // Use the actual ID width so the title follows exactly one space later.
    let id = truncate(&session.id, TREE_STATUS_WIDTH.min(available - 2));
    let id_width = UnicodeWidthStr::width(id.as_str());
    spans.push(Span::styled(id, status_style));
    let remaining = available.saturating_sub(2 + id_width);
    if remaining < age.len() + 2 {
        return (selected_line(spans, selected), connector_width);
    }

    let current_rate = rate.map(|value| value.display_rate(now)).unwrap_or(0.0);
    let animation_age = rate
        .map(|value| now.saturating_duration_since(value.sampled_at))
        .unwrap_or_default();
    let mut metrics = vec![(age, dim)];
    let title = session_title(session);
    let badge_width = session_program_status_span(session).map_or(0, |span| span.width() + 1);
    let min_title = (UnicodeWidthStr::width(title.as_str()).min(TREE_MIN_TITLE_WIDTH)
        + badge_width)
        .min(remaining.saturating_sub(metrics[0].0.len() + 2));
    // Metrics are packed beside the title, not pinned across the terminal.
    // Preserve a recognizable title and age, then add output and rate.
    let mut metric_width = metrics[0].0.len();
    let command = session_command_label(session);
    let command_min = UnicodeWidthStr::width(command.as_str()).min(8);
    let command_reserve =
        if command_min > 0 && remaining >= 2 + min_title + metric_width + 3 + command_min {
            3 + command_min
        } else {
            0
        };
    let mut add_metric = |text: String, style: Style| {
        let cost = UnicodeWidthStr::width(text.as_str()) + 1;
        if remaining >= 2 + min_title + metric_width + cost + command_reserve {
            metric_width += cost;
            metrics.push((text, style));
        }
    };
    add_metric(format_bytes(session.last_total_bytes as f64), dim);
    if active {
        add_metric(
            format!("{}/s", format_bytes(current_rate)),
            Style::default().fg(if selected && current_rate <= 0.0 {
                Color::Gray
            } else {
                rate_color(current_rate, animation_age)
            }),
        );
    }
    if session.input_needed && session.program_status.is_none() {
        add_metric("wait".into(), status_style);
    } else if !active {
        add_metric(session.status.clone(), dim);
    }
    if !session.tags.is_empty() {
        add_metric(format!("[{}]", truncate(&session.tags.join(","), 20)), dim);
    }

    // Give long titles a fair share without reserving unused title cells.
    // Every reclaimed cell goes to command arguments rather than whitespace.
    let title_budget = remaining.saturating_sub(metric_width + 2 + command_reserve);
    let title_width = if title_budget >= min_title + 12 {
        (title_budget / 2).clamp(min_title, 40)
    } else {
        title_budget
    };
    let title = session_title_spans(session, title_width);
    // Plan the optional chart against the complete row, then place it before
    // output bytes, matching the table's age / activity / output ordering.
    let used = spans.iter().map(Span::width).sum::<usize>()
        + 1
        + title.iter().map(Span::width).sum::<usize>()
        + 1
        + metric_width;
    let has_activity =
        active && rate.is_some_and(|rate| rate.history.iter().any(|value| *value > 0.0));
    if has_activity && width.saturating_sub(used) >= SPARKLINE_WIDTH + 1 + 16 {
        metrics.insert(
            1,
            (
                sparkline(rate, SPARKLINE_WIDTH),
                Style::default().fg(rate_color(current_rate, animation_age)),
            ),
        );
    }
    spans.push(Span::raw(" "));
    spans.extend(title.into_iter().map(|mut span| {
        span.style = title_style.patch(span.style);
        span
    }));
    for (text, style) in metrics {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(text, style));
    }
    let used = spans.iter().map(Span::width).sum::<usize>();
    let command_width = width.saturating_sub(used + 3);
    if command_width >= 4 {
        spans.push(Span::styled(" │ ", dim));
        spans.push(Span::styled(
            truncate(&command, command_width),
            Style::default().fg(if selected {
                Color::White
            } else if active {
                Color::Gray
            } else {
                Color::DarkGray
            }),
        ));
    }
    (selected_line(spans, selected), connector_width)
}

fn selected_line(spans: Vec<Span<'static>>, selected: bool) -> Line<'static> {
    let line = Line::from(spans);
    if selected {
        line.style(Style::default().bg(SELECTED_ROW_BG))
    } else {
        line
    }
}

fn compact_tree_start(started: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let elapsed = now.signed_duration_since(started);
    if elapsed.num_minutes() <= 0 {
        "now".to_string()
    } else if elapsed.num_days() > 0 {
        format!("{}d", elapsed.num_days())
    } else if elapsed.num_hours() > 0 {
        format!("{}h", elapsed.num_hours())
    } else {
        format!("{}m", elapsed.num_minutes())
    }
}

// Keep the long-format API intact for callers and its existing tests.
#[allow(dead_code)] // The viewport uses compact_tree_start instead.
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

#[cfg(test)]
#[path = "tree_render_tests.rs"]
mod tests;
