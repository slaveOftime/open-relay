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
        resume_command: None,
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
fn inactive_sessions_have_no_sparkline_and_tree_rows_are_compact() {
    let mut item = session("finished");
    item.status = "stopped".to_string();
    let buffer = render_session_row(&item);
    let row = (0..buffer.area.width)
        .map(|x| buffer[(x, 0)].symbol())
        .collect::<String>();
    assert!(
        !row.chars()
            .any(|symbol| super::SPARK_BLOCKS.contains(&symbol)),
        "inactive table row should not render a sparkline: {row:?}"
    );

    let now = std::time::Instant::now();
    let inactive_line = super::session_line(&item, None, "", false, now);
    assert!(!inactive_line.spans.iter().any(|span| {
        span.content
            .chars()
            .any(|symbol| super::SPARK_BLOCKS.contains(&symbol))
    }));
    let mut active = item.clone();
    active.status = "running".to_string();
    let active_line = super::session_line(&active, None, "", false, now);
    let line_width = |line: &ratatui::text::Line<'_>| {
        line.spans
            .iter()
            .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
            .sum::<usize>()
    };
    assert_eq!(
        line_width(&active_line) - line_width(&inactive_line),
        super::SPARKLINE_WIDTH + 2,
        "inactive tree rows should reclaim the sparkline and its padding"
    );
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

fn tree_has_folder(app: &App, path: &str) -> bool {
    app.tree.visible.iter().any(|entry| match entry {
        super::TreeEntry::Folder { node, .. } => {
            app.tree.nodes[*node].path == std::path::Path::new(path)
        }
        super::TreeEntry::Session { .. } => false,
    })
}
fn expand_all_tree_folders(app: &mut App) {
    loop {
        let next = app
            .tree
            .visible
            .iter()
            .enumerate()
            .find_map(|(position, entry)| {
                let super::TreeEntry::Folder { node, depth } = *entry else {
                    return None;
                };
                ((!app.tree.nodes[node].subfolders.is_empty()
                    || !app.tree.nodes[node].direct_sessions.is_empty())
                    && !app.tree.is_expanded(node, depth))
                .then_some(position)
            });
        let Some(position) = next else {
            break;
        };
        app.tree.cursor = position;
        app.toggle_tree_drill();
    }
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
fn switching_to_tree_expands_one_level_and_focuses_the_nearest_folder() {
    let mut app = App::default();
    app.replace_sessions(vec![
        session_at("shallow", Some("/work")),
        session_at("selected-deep", Some("/work/a/b/c/d")),
    ]);
    let selected_index = app
        .sessions
        .iter()
        .position(|session| session.id == "selected-deep")
        .unwrap();
    app.selected = app
        .visible
        .iter()
        .position(|&index| index == selected_index)
        .unwrap();

    assert_eq!(
        route_key(&mut app, ctrl(KeyCode::Char('g')), None),
        AppAction::None
    );
    let ids = tree_visible_ids(&app);
    assert!(!ids.contains(&"selected-deep".to_string()), "{ids:?}");
    let focused = app.tree.visible.get(app.tree.cursor).copied();
    assert!(matches!(focused,
        Some(super::TreeEntry::Folder { node, .. })
            if app.tree.nodes[node].path == std::path::Path::new("work")));
}
#[test]
fn absolute_tree_paths_do_not_create_empty_root_folders() {
    let mut tree = super::TreeView::new();
    let root = tree.root;
    let node = None;
    let leaf = super::ensure_tree_path(
        &mut tree,
        root,
        std::path::Path::new("/Slaveoftime/open-relay"),
        "/Slaveoftime/open-relay",
        &node,
    );

    assert_eq!(tree.nodes[leaf].name, "open-relay");
    assert!(
        tree.nodes
            .iter()
            .skip(1)
            .all(|folder| !folder.name.is_empty()),
        "absolute root components must not render as duplicate (root) folders"
    );
}

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
    expand_all_tree_folders(&mut app);
    expand_all_tree_folders(&mut app);
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
    assert_eq!(super::TREE_AUTO_DEPTH, 1);
    app.replace_sessions(vec![session_at("lint", Some("/a/b/c/something"))]);
    let ids = tree_visible_ids(&app);
    let has_folder = |path: &str| {
        ids.iter().any(|id| {
            std::path::Path::new(id.strip_prefix("folder:").unwrap_or(""))
                == std::path::Path::new(path)
        })
    };
    assert!(
        has_folder("a"),
        "first folder level should be visible: {ids:?}"
    );
    assert!(
        !has_folder("a/b"),
        "second folder level should require an explicit drill: {ids:?}"
    );
    assert!(!has_folder("a/b/c"), "ids={ids:?}");
    assert!(!has_folder("a/b/c/something"), "ids={ids:?}");
    assert!(!ids.contains(&"lint".to_string()), "ids={ids:?}");
}
#[test]
fn tree_enter_on_session_emits_open_inline() {
    let mut app = App::default();
    app.replace_sessions(vec![session_at("only", Some("/p"))]);
    app.toggle_view_mode();
    expand_all_tree_folders(&mut app);
    let session_position = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "only")
        })
        .expect("leaf folder expands to its session");
    app.tree.cursor = session_position;
    let action = route_key(&mut app, key(KeyCode::Enter), None);
    assert_eq!(action, AppAction::OpenInline);
}

#[test]
fn folder_with_only_sessions_can_toggle_its_direct_session_rows() {
    let mut app = App::default();
    app.replace_sessions(vec![session_at("only", Some("/work/project"))]);
    app.toggle_view_mode();

    let work = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].path == std::path::Path::new("work"))
        })
        .expect("first cwd folder is visible");
    app.tree.cursor = work;
    route_key(&mut app, key(KeyCode::Enter), None);

    let project = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].path == std::path::Path::new("work/project"))
        })
        .expect("session-only leaf folder is visible");
    let super::TreeEntry::Folder { node, depth } = app.tree.visible[project] else {
        unreachable!()
    };
    assert!(app.tree.nodes[node].subfolders.is_empty());
    assert_eq!(app.tree.nodes[node].direct_sessions.len(), 1);
    assert!(!app.tree.is_expanded(node, depth));
    let collapsed = super::folder_line(
        &app.tree.nodes[node],
        ratatui::text::Line::default(),
        false,
        None,
        true,
        false,
    );
    assert_eq!(collapsed.spans.last().unwrap().content, "▸ project/");

    app.tree.cursor = project;
    route_key(&mut app, key(KeyCode::Enter), None);
    assert!(app.tree.is_expanded(node, depth));
    assert!(app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "only")));
    let expanded = super::folder_line(
        &app.tree.nodes[node],
        ratatui::text::Line::default(),
        false,
        None,
        true,
        true,
    );
    assert_eq!(expanded.spans.last().unwrap().content, "▾ project/");

    let project = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node: current, .. } if *current == node)
        })
        .unwrap();
    app.tree.cursor = project;
    route_key(&mut app, key(KeyCode::Enter), None);
    assert!(!app.tree.is_expanded(node, depth));
    assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "only")));
}
#[test]
fn tree_enter_expands_one_folder_at_a_time_and_can_collapse() {
    let mut app = App::default();
    app.replace_sessions(vec![session_at("deep", Some("/a/b/c/d/e"))]);
    app.toggle_view_mode();
    app.tree.drilled.clear();
    app.tree.collapsed.clear();
    app.rebuild_tree();

    assert!(tree_has_folder(&app, "a"));
    assert!(!tree_has_folder(&app, "a/b"));
    assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "deep")));

    for (step, path) in ["a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e"]
        .iter()
        .enumerate()
    {
        let position = app
            .tree
            .visible
            .iter()
            .position(|entry| {
                matches!(entry,
                super::TreeEntry::Folder { node, .. }
                    if app.tree.nodes[*node].path == std::path::Path::new(path))
            })
            .unwrap_or_else(|| panic!("folder {path:?} should appear before its drill"));
        app.tree.cursor = position;
        assert_eq!(
            route_key(&mut app, key(KeyCode::Enter), None),
            AppAction::None
        );
        assert!(tree_has_folder(&app, path));
        if step < 4 {
            assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
                super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "deep")));
        }
    }
    assert!(app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "deep")));

    let root_position = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].path == std::path::Path::new("a"))
        })
        .unwrap();
    app.tree.cursor = root_position;
    route_key(&mut app, key(KeyCode::Enter), None);
    assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "deep")));
    assert!(tree_has_folder(&app, "a"));
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
    expand_all_tree_folders(&mut app);
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
    expand_all_tree_folders(&mut app);
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
    expand_all_tree_folders(&mut app);
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
    build.args = vec!["--special-arg".into()];
    build.title = Some("Review Notes".into());
    build.tags = vec!["release-tag".into()];
    app.replace_sessions(vec![lint, build]);
    app.toggle_view_mode();
    expand_all_tree_folders(&mut app);
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

    // Every session field used by the UI can locate the matching row in
    // both views; tree filtering must also retain the path to that session.
    for filter in [
        "build",
        "build-target",
        "special-arg",
        "review notes",
        "release-tag",
    ] {
        app.normalized_filter = filter.into();
        app.rebuild_visible();
        assert!(
            app.visible
                .iter()
                .any(|&index| app.sessions[index].id == "s2"),
            "list search should match {filter:?}"
        );
        assert!(
            app.tree.visible.iter().any(|entry| matches!(entry,
                super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "s2")),
            "tree search should match {filter:?}"
        );
        if filter == "build-target" {
            assert!(tree_has_folder(&app, "proj"));
            assert!(tree_has_folder(&app, "proj/build-target"));
        }
    }
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
fn cwdless_sessions_use_their_oly_storage_directory_in_tree_and_filter_paths() {
    let storage = std::path::PathBuf::from(r"C:\oly-state\sessions");
    let expected_local_dir = storage.join("orphan-local");
    let mut local = session("orphan-local");
    local.cwd = None;
    let mut remote = session("orphan-remote");
    remote.cwd = None;
    remote.node = Some("worker-a".to_string());
    let mut app = App {
        session_storage_dir: Some(storage),
        ..Default::default()
    };
    app.replace_sessions(vec![local, remote]);

    let local_index = app
        .tree
        .nodes
        .iter()
        .find(|node| node.direct_sessions.contains(&0))
        .expect("local session directory leaf");
    assert_eq!(local_index.cwd.as_deref(), expected_local_dir.to_str());
    let remote_index = app
        .tree
        .nodes
        .iter()
        .find(|node| node.direct_sessions.contains(&1))
        .expect("remote session directory leaf");
    let expected_remote_dir = std::path::PathBuf::from("sessions").join("orphan-remote");
    assert_eq!(remote_index.cwd.as_deref(), expected_remote_dir.to_str());

    app.filter = "orphan-local".to_string();
    app.update_text_filter();
    assert!(app.tree.auto_expand_all);
    assert!(app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "orphan-local")));
    for folder in ["oly-state", "sessions", "orphan-local"] {
        assert!(app.tree.visible.iter().any(|entry| matches!(entry,
            super::TreeEntry::Folder { node, .. } if app.tree.nodes[*node].name == folder)));
    }
}
#[test]
fn tree_folders_show_attention_before_descendant_running_state() {
    let mut attention = session_at("attention", Some("/workspace/project/needs/input"));
    attention.input_needed = true;
    let active = session_at("active", Some("/workspace/running/build"));
    let mut inactive = session_at("stopped", Some("/workspace/stopped/archive"));
    inactive.status = "stopped".to_string();
    let mut app = App::default();
    app.replace_sessions(vec![attention, active, inactive]);

    let visible_ids = tree_visible_ids(&app);
    assert!(!visible_ids.contains(&"attention".to_string()));
    let colors = super::tree_render::folder_status_colors(&app.tree, &app.sessions);
    let color_for_folder = |name: &str| {
        let index = app
            .tree
            .nodes
            .iter()
            .position(|node| node.name == name)
            .expect("folder exists");
        colors[index]
    };
    assert_eq!(
        color_for_folder("workspace"),
        Some(ratatui::style::Color::Yellow)
    );
    let project_index = app
        .tree
        .nodes
        .iter()
        .position(|node| node.name == "project")
        .expect("project folder exists");
    let project_line = super::folder_line(
        &app.tree.nodes[project_index],
        ratatui::text::Line::default(),
        false,
        colors[project_index],
        true,
        false,
    );
    assert_eq!(
        project_line.spans.last().unwrap().style.fg,
        Some(ratatui::style::Color::Yellow)
    );
    assert_eq!(
        color_for_folder("running"),
        Some(ratatui::style::Color::Green)
    );
    assert_eq!(color_for_folder("stopped"), None);
}

#[test]
fn folder_branches_show_their_own_status_without_tinting_inactive_rows() {
    let entries = [
        super::TreeEntry::Folder { node: 1, depth: 1 },
        super::TreeEntry::Folder { node: 2, depth: 2 },
        super::TreeEntry::Folder { node: 3, depth: 2 },
        super::TreeEntry::Folder { node: 4, depth: 1 },
    ];
    let colors = [
        None,
        Some(ratatui::style::Color::Yellow),
        Some(ratatui::style::Color::Green),
        Some(ratatui::style::Color::Red),
        None,
    ];
    let connector = super::tree_render::tree_connector_line(&entries, 1, &colors, false);
    assert_eq!(
        connector
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>(),
        super::tree_connector(&entries, 1)
    );
    assert_eq!(
        connector.spans[0].style.fg,
        Some(ratatui::style::Color::DarkGray)
    );
    assert_eq!(
        connector.spans[1].style.fg,
        Some(ratatui::style::Color::Green)
    );

    let inactive_session_connector =
        super::tree_render::tree_connector_line(&entries, 3, &colors, false);
    assert!(
        inactive_session_connector
            .spans
            .iter()
            .all(|span| span.style.fg == Some(ratatui::style::Color::DarkGray))
    );
}
#[test]
fn tree_groups_by_node_before_cwd_and_keeps_identical_paths_separate() {
    let mut app = App::default();
    let mut remote = session_at("remote", Some("/work/app"));
    remote.node = Some("worker".into());
    let local = session_at("local", Some("/work/app"));
    app.replace_sessions(vec![remote, local]);
    assert_eq!(app.tree.nodes[app.tree.root].subfolders.len(), 2);
    let default_ids = tree_visible_ids(&app);
    assert!(!default_ids.contains(&"local".to_string()));
    assert!(!default_ids.contains(&"remote".to_string()));

    app.selected = app
        .visible
        .iter()
        .position(|&index| app.sessions[index].id == "remote")
        .expect("remote session in list");
    app.toggle_view_mode();
    let focused = app.tree.visible.get(app.tree.cursor).copied();
    assert!(matches!(focused,
        Some(super::TreeEntry::Folder { node, .. })
            if app.tree.nodes[node].node.as_deref() == Some("worker")));

    let remote_work = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].node.as_deref() == Some("worker")
                    && app.tree.nodes[*node].path == std::path::Path::new("work"))
        })
        .expect("remote work folder is initially visible");
    app.tree.cursor = remote_work;
    route_key(&mut app, key(KeyCode::Enter), None);
    let remote_app = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].node.as_deref() == Some("worker")
                    && app.tree.nodes[*node].path == std::path::Path::new("work/app"))
        })
        .expect("remote app folder appears after expanding work");
    app.tree.cursor = remote_app;
    route_key(&mut app, key(KeyCode::Enter), None);
    let drilled_ids = tree_visible_ids(&app);
    assert!(
        drilled_ids.contains(&"remote".to_string()),
        "{drilled_ids:?}"
    );
    assert!(
        !drilled_ids.contains(&"local".to_string()),
        "{drilled_ids:?}"
    );
    let rendered = render_app(&mut app, 120, 30);
    assert!(rendered.contains("local (node)"));
    assert!(rendered.contains("worker (node)"));
}
#[test]
fn auto_expanded_node_root_can_be_collapsed_and_reexpanded() {
    let mut local = session_at("local", Some("/home/binwenw/Dev/project"));
    local.command = "local".into();
    let mut remote = session_at("remote", Some("/srv/worker/job"));
    remote.node = Some("worker".into());
    let mut app = App::default();
    app.replace_sessions(vec![local, remote]);
    app.toggle_view_mode();

    let local_root = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, depth: 1 }
                if app.tree.nodes[*node].is_node && app.tree.nodes[*node].node.is_none())
        })
        .expect("local node root is visible");
    let super::TreeEntry::Folder { node, depth } = app.tree.visible[local_root] else {
        unreachable!()
    };
    assert!(app.tree.is_expanded(node, depth));
    let expanded_label = super::folder_line(
        &app.tree.nodes[node],
        ratatui::text::Line::default(),
        false,
        None,
        true,
        true,
    );
    assert_eq!(
        expanded_label.spans.last().unwrap().content,
        "▾ local (node)"
    );
    assert!(app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Folder { node, .. }
            if app.tree.nodes[*node].node.is_none() && !app.tree.nodes[*node].is_node)));

    app.tree.cursor = local_root;
    route_key(&mut app, key(KeyCode::Enter), None);
    assert!(!app.tree.is_expanded(node, depth));
    let collapsed_label = super::folder_line(
        &app.tree.nodes[node],
        ratatui::text::Line::default(),
        false,
        None,
        true,
        false,
    );
    assert_eq!(
        collapsed_label.spans.last().unwrap().content,
        "▸ local (node)"
    );
    assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Folder { node, .. }
            if app.tree.nodes[*node].node.is_none() && !app.tree.nodes[*node].is_node)));

    let local_root = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, depth: 1 }
                if app.tree.nodes[*node].is_node && app.tree.nodes[*node].node.is_none())
        })
        .unwrap();
    app.tree.cursor = local_root;
    route_key(&mut app, key(KeyCode::Enter), None);
    assert!(app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Folder { node, .. }
            if app.tree.nodes[*node].node.is_none() && !app.tree.nodes[*node].is_node)));
}
#[test]
fn tree_keeps_shared_cwd_visible_when_a_session_starts_there() {
    let mut app = App::default();
    app.replace_sessions(vec![
        session_at("parent", Some("/work/app")),
        session_at("child", Some("/work/app/sub")),
    ]);
    app.toggle_view_mode();
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
    app.tree.cursor = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. } if *node == folder)
        })
        .unwrap();
    route_key(&mut app, key(KeyCode::Enter), None);
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

    for path in ["a", "a/b", "a/b/c", "a/b/c/d", "a/b/c/d/e"] {
        let target = app
            .tree
            .visible
            .iter()
            .position(|entry| {
                matches!(entry,
                super::TreeEntry::Folder { node, .. }
                    if app.tree.nodes[*node].node.as_deref() == Some("worker")
                        && app.tree.nodes[*node].path == std::path::Path::new(path))
            })
            .unwrap_or_else(|| panic!("remote folder {path:?} is not visible yet"));
        app.tree.cursor = target;
        app.toggle_tree_drill();
        if path != "a/b/c/d/e" {
            assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
                super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "remote")));
        }
    }
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
    expand_all_tree_folders(&mut app);
    let rendered = render_app(&mut app, 120, 30);
    assert!(rendered.contains("├── ▾ config/"), "{rendered}");
    assert!(rendered.contains("│   ├──"), "{rendered}");
    assert!(rendered.contains("│   └──"), "{rendered}");
    assert!(rendered.contains("└── ▾ docs/"), "{rendered}");
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
    expand_all_tree_folders(&mut app);
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
    expand_all_tree_folders(&mut app);
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
    expand_all_tree_folders(&mut app);

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
    expand_all_tree_folders(&mut app);
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
    assert_eq!(dialog.source_node.as_deref(), Some("worker-a"));
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
    assert!(!dialog.remove_original);
    assert!(render_app(&mut app, 120, 30).contains("Duplicate source (worker-a)"));
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
    assert!(dialog.source_node.is_none());
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
    assert!(!dialog.remove_original);

    let rendered = render_app(&mut app, 120, 30);
    assert!(rendered.contains("New Session"));
    assert!(!rendered.contains("Duplicate source"));
    assert!(!rendered.contains("Remove original"));
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
    assert!(launch.remove_source.is_none());
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
    app.last_frame_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(1000));
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
    summary.created_at =
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + chrono::Duration::milliseconds(123);
    summary.started_at = Some(
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 0).unwrap() + chrono::Duration::milliseconds(456),
    );
    summary.ended_at = Some(
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 2, 0).unwrap() + chrono::Duration::milliseconds(789),
    );
    summary.last_output_epoch = Some(
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 1, 30).unwrap() + chrono::Duration::milliseconds(987),
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
    let disabled = super::update_field_line(dialog, super::UpdateField::Notifications, 80, true);
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
    assert!(last_line.contains("^Del remove"));
    assert!(!last_line.contains("^R"));
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
        CloneField::RemoveOriginal
    );
}

#[test]
fn duplicate_remove_original_is_opt_in_and_skipped_for_new_sessions() {
    let mut app = App::default();
    let mut source = session("source");
    source.node = Some("original-node".to_string());
    app.replace_sessions(vec![source]);
    route_key(&mut app, ctrl(KeyCode::Char('d')), None);
    assert!(!app.clone_dialog.as_ref().unwrap().remove_original);
    assert!(
        app.clone_dialog
            .as_ref()
            .unwrap()
            .launch()
            .unwrap()
            .remove_source
            .is_none()
    );
    assert!(render_app(&mut app, 120, 30).contains("Remove original"));

    // This checkbox is reachable by keyboard, and its source node is not
    // affected by editing the target node for the new session.
    app.clone_dialog.as_mut().unwrap().active = super::CLONE_FIELDS.len() - 1;
    route_key(&mut app, key(KeyCode::Char(' ')), None);
    assert!(app.clone_dialog.as_ref().unwrap().remove_original);
    app.clone_dialog.as_mut().unwrap().node = super::EditText::new("new-node".to_string());
    let AppAction::Start(launch) = route_key(&mut app, key(KeyCode::Enter), None) else {
        panic!("duplicate must produce a start action");
    };
    assert_eq!(launch.node.as_deref(), Some("new-node"));
    let source = launch.remove_source.expect("remove-original option");
    assert_eq!(source.id, "source");
    assert_eq!(source.node.as_deref(), Some("original-node"));
    let RpcRequest::NodeProxy { node, inner } = super::remove_request(&source) else {
        panic!("original removal must route to original node");
    };
    assert_eq!(node, "original-node");
    assert!(matches!(*inner, RpcRequest::Remove { id, force: true } if id == "source"));

    let mut blank = super::CloneDialog::blank(Some("new-node"));
    blank.command = super::EditText::new("echo".to_string());
    blank.previous();
    assert_eq!(blank.active_field(), CloneField::AttachAfterStart);
    blank.remove_original = true;
    assert!(blank.launch().unwrap().remove_source.is_none());
}
#[test]
fn duplicate_remove_reports_failures_and_cleans_the_original_node_only_on_success() {
    let mut app = App::default();
    let local = session("same-id");
    let mut remote = session("same-id");
    remote.node = Some("worker".to_string());
    app.replace_sessions(vec![local, remote]);
    let target = super::SessionTarget {
        id: "same-id".to_string(),
        node: Some("worker".to_string()),
    };
    assert!(app.rates.contains_key("same-id"));
    assert!(app.rates.contains_key("worker\0same-id"));

    let (message, failed) = super::refresh::apply_clone_remove_response(
        &mut app,
        "new-session",
        &target,
        Err(AppError::NodeNotConnected("worker".to_string())),
    );
    assert!(failed);
    assert!(message.contains("original same-id was not removed"));
    assert_eq!(app.sessions.len(), 2);

    let (message, failed) = super::refresh::apply_clone_remove_response(
        &mut app,
        "new-session",
        &target,
        Ok(RpcResponse::Remove { removed: false }),
    );
    assert!(failed);
    assert!(message.contains("not found"));
    assert_eq!(app.sessions.len(), 2);

    let (message, failed) = super::refresh::apply_clone_remove_response(
        &mut app,
        "new-session",
        &target,
        Ok(RpcResponse::Remove { removed: true }),
    );
    assert!(!failed);
    assert!(message.contains("removed original same-id"));
    assert_eq!(app.sessions.len(), 1);
    assert!(app.sessions[0].node.is_none());
    assert!(app.rates.contains_key("same-id"));
    assert!(!app.rates.contains_key("worker\0same-id"));
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
    dialog.remove_original = true;

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
        remove_source: Some(super::SessionTarget {
            id: "source".to_string(),
            node: None,
        }),
    };
    assert_eq!(
        route_key(&mut app, key(KeyCode::Enter), None),
        AppAction::Start(expected)
    );

    let AppAction::Start(launch) = route_key(&mut app, key(KeyCode::Enter), None) else {
        panic!("expected start action");
    };
    assert!(matches!(
        super::remove_request(launch.remove_source.as_ref().unwrap()),
        RpcRequest::Remove { id, force: true } if id == "source"
    ));
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
fn ctrl_delete_requires_confirmation_before_force_removal() {
    let mut app = App::default();
    assert_eq!(
        route_key(&mut app, ctrl(KeyCode::Delete), None),
        AppAction::None
    );
    assert_eq!(
        app.message.as_deref(),
        Some("no session or folder in focus to remove")
    );

    assert_eq!(
        route_key(&mut app, ctrl(KeyCode::Char('r')), None),
        AppAction::None
    );
    assert!(
        app.remove_dialog.is_none(),
        "Ctrl+R is reserved and must not delete a session"
    );

    let mut selected = session("remove-me");
    selected.node = Some("worker-a".to_string());
    app.replace_sessions(vec![selected]);

    assert_eq!(
        route_key(&mut app, ctrl(KeyCode::Delete), None),
        AppAction::None
    );
    assert_eq!(
        app.sessions.len(),
        1,
        "opening confirmation must not remove the row"
    );
    assert_eq!(
        app.remove_dialog
            .as_ref()
            .map(|dialog| dialog.targets.as_slice()),
        Some(
            &[super::SessionTarget {
                id: "remove-me".to_string(),
                node: Some("worker-a".to_string()),
            }][..]
        )
    );
    let rendered = render_app(&mut app, 100, 18);
    assert!(rendered.contains("Force-remove this session?"));
    assert!(rendered.contains("remove-me on worker-a"));
    let message_line = rendered
        .lines()
        .find(|line| line.contains("Force-remove this session?"))
        .unwrap();
    let target_line = rendered
        .lines()
        .find(|line| line.contains("remove-me on worker-a"))
        .unwrap();
    let actions_line = rendered
        .lines()
        .find(|line| line.contains("Enter/Y remove"))
        .unwrap();
    assert_eq!(
        message_line.find("Force-remove"),
        actions_line.find("Enter/Y")
    );
    assert_eq!(target_line.find("remove-me"), actions_line.find("Enter/Y"));

    assert_eq!(
        route_key(&mut app, key(KeyCode::Esc), None),
        AppAction::None
    );
    assert!(app.remove_dialog.is_none());
    assert_eq!(
        app.sessions.len(),
        1,
        "cancelling must preserve the session"
    );

    route_key(&mut app, ctrl(KeyCode::Delete), None);
    assert_eq!(
        route_key(&mut app, key(KeyCode::Enter), None),
        AppAction::Remove(vec![super::SessionTarget {
            id: "remove-me".to_string(),
            node: Some("worker-a".to_string()),
        }])
    );
    assert!(app.remove_dialog.is_none());
    assert_eq!(
        app.sessions.len(),
        1,
        "RPC dispatch owns the optimistic removal"
    );

    let request = super::remove_request(&super::SessionTarget {
        id: "remove-me".to_string(),
        node: Some("worker-a".to_string()),
    });
    let RpcRequest::NodeProxy { node, inner } = request else {
        panic!("remote remove must be routed through NodeProxy");
    };
    assert_eq!(node, "worker-a");
    assert!(matches!(*inner, RpcRequest::Remove { id, force: true } if id == "remove-me"));
}
#[test]
fn removing_a_nonselected_session_preserves_the_selected_session() {
    let mut app = App::default();
    app.replace_sessions(vec![session("first"), session("selected")]);
    app.selected = app
        .sessions
        .iter()
        .position(|session| session.id == "selected")
        .unwrap();
    app.remove_session_payload("first", None);
    assert_eq!(
        app.selected_session().map(|session| session.id.as_str()),
        Some("selected")
    );
}
#[test]
fn ctrl_delete_on_folder_removes_all_descendant_sessions_even_when_filtered() {
    let mut direct = session_at("direct", Some("/work/project"));
    direct.command = "matches-filter".into();
    let mut hidden = session_at("nested", Some("/work/project/sub/deep"));
    hidden.command = "does-not-match".into();
    let mut outside = session_at("outside", Some("/work/other"));
    outside.command = "matches-filter".into();
    let mut app = App::default();
    app.replace_sessions(vec![direct, hidden, outside]);
    app.filter = "matches-filter".into();
    app.update_text_filter();
    app.toggle_view_mode();

    assert!(!app.tree.visible.iter().any(|entry| matches!(entry,
        super::TreeEntry::Session { session, .. } if app.sessions[*session].id == "nested")));
    let folder = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
            super::TreeEntry::Folder { node, .. }
                if app.tree.nodes[*node].cwd.as_deref() == Some("/work/project"))
        })
        .expect("project folder in the filtered tree");
    app.tree.cursor = folder;
    assert_eq!(
        route_key(&mut app, ctrl(KeyCode::Delete), None),
        AppAction::None
    );
    let dialog = app
        .remove_dialog
        .as_ref()
        .expect("folder removal confirmation");
    let ids = dialog
        .targets
        .iter()
        .map(|target| target.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["direct", "nested"]);
    assert!(dialog.prompt.contains("2 sessions"));
    assert!(dialog.detail.contains("/work/project"));
    assert_eq!(
        app.sessions.len(),
        3,
        "confirmation is not optimistic removal"
    );

    route_key(&mut app, key(KeyCode::Esc), None);
    assert!(app.remove_dialog.is_none());
    assert_eq!(app.sessions.len(), 3);

    app.tree.cursor = app
        .tree
        .visible
        .iter()
        .position(|entry| {
            matches!(entry,
        super::TreeEntry::Folder { node, .. }
            if app.tree.nodes[*node].cwd.as_deref() == Some("/work/project"))
        })
        .unwrap();
    route_key(&mut app, ctrl(KeyCode::Delete), None);
    let AppAction::Remove(targets) = route_key(&mut app, key(KeyCode::Enter), None) else {
        panic!("confirm should dispatch the folder session batch");
    };
    assert_eq!(
        targets
            .iter()
            .map(|target| target.id.as_str())
            .collect::<Vec<_>>(),
        ["direct", "nested"]
    );
    assert!(targets.iter().all(|target| {
        matches!(
            super::remove_request(target),
            RpcRequest::Remove { force: true, .. }
        )
    }));
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
    assert!(matches!(events[1], crossterm::event::Event::Key(k) if k.code == KeyCode::Char('a')));
    assert!(matches!(events[3], crossterm::event::Event::Key(k) if k.code == KeyCode::Char('c')));
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
    assert_eq!(
        args,
        [
            "logs",
            "abc",
            "--keep-color",
            "--tail",
            "1000",
            "--node",
            "worker"
        ]
    );
    let (_, local_args) = super::session_command("abc", None, false).unwrap();
    assert_eq!(
        local_args,
        ["logs", "abc", "--keep-color", "--tail", "1000"]
    );
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

    assert_eq!(app.search_text, vec!["a\ncmd\nrunning".to_string()]);
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
