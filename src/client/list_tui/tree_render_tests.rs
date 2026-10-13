use super::*;
use chrono::{Duration, TimeZone};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, widgets::Widget};
use std::path::PathBuf;

const WIDTHS: [usize; 5] = [24, 40, 60, 80, 120];

fn session() -> SessionSummary {
    serde_json::from_value(serde_json::json!({
        "id": "ident42",
        "title": "Release deployment checklist and verification",
        "command": "/usr/local/bin/runner",
        "args": ["--verbose", "--configuration", "a-very-long-configuration-name"],
        "status": "running",
        "created_at": "2026-01-01T00:00:00Z",
        "started_at": Utc::now() - Duration::days(3) - Duration::seconds(30),
        "last_total_bytes": 0,
        "input_needed": false,
        "notifications_enabled": true
    }))
    .expect("valid isolated session summary")
}

fn connector(depth: usize) -> Line<'static> {
    tree_connector_line(&[TreeEntry::Session { session: 0, depth }], 0, &[], false)
}

fn text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn render_line(line: Line<'static>, width: usize) -> Buffer {
    let area = Rect::new(0, 0, width as u16, 1);
    let mut buffer = Buffer::empty(area);
    Paragraph::new(line).render(area, &mut buffer);
    buffer
}

fn folder(name: &str) -> TreeNode {
    TreeNode {
        path: PathBuf::from(name),
        cwd: None,
        node: None,
        is_node: false,
        name: name.to_string(),
        direct_sessions: vec![],
        subfolders: vec![],
    }
}

#[test]
fn connectors_are_two_cells_per_depth_and_keep_sibling_rails() {
    let entries = [
        TreeEntry::Folder { node: 0, depth: 1 },
        TreeEntry::Folder { node: 1, depth: 2 },
        TreeEntry::Session {
            session: 0,
            depth: 3,
        },
        TreeEntry::Folder { node: 2, depth: 2 },
        TreeEntry::Folder { node: 3, depth: 1 },
    ];
    let expected = ["├─", "│ ├─", "│ │ └─", "│ └─", "└─"];
    for (index, entry) in entries.iter().enumerate() {
        let line = tree_connector_line(&entries, index, &[], false);
        assert_eq!(text(&line), expected[index]);
        assert_eq!(line.width(), entry.depth() * 2);
        assert_eq!(tree_connector(&entries, index), expected[index]);
    }
    assert_eq!(connector(0).width(), 0);
}

#[test]
fn folders_reserve_expander_slot_and_preserve_status_selection() {
    let node = folder("project");
    let collapsed = folder_line_with_width(
        &node,
        connector(1),
        true,
        Some(Color::Yellow),
        true,
        false,
        40,
    );
    let expanded = folder_line_with_width(&node, connector(1), false, None, true, true, 40);
    let leaf = folder_line_with_width(&node, connector(1), false, None, false, false, 40);
    assert_eq!(text(&collapsed), "└─▸ project/");
    assert_eq!(text(&expanded), "└─▾ project/");
    assert_eq!(text(&leaf), "└─  project/");
    assert_eq!(collapsed.width(), leaf.width());
    assert_eq!(collapsed.style.bg, Some(SELECTED_ROW_BG));
    let label = collapsed.spans.last().unwrap();
    assert_eq!(label.style.fg, Some(Color::Yellow));
    assert!(label.style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn clipped_connectors_keep_hidden_ancestor_marker_and_branch_style() {
    let entries = [TreeEntry::Folder { node: 0, depth: 40 }];
    let colors = [Some(Color::Yellow)];
    for budget in 0..=30 {
        let fitted = fit_connector(tree_connector_line(&entries, 0, &colors, true), budget);
        assert!(fitted.width() <= budget);
        if budget > 0 {
            assert!(text(&fitted).starts_with('…'));
        }
        if budget >= 4 {
            assert!(text(&fitted).ends_with("└─"));
            assert_eq!(fitted.spans.last().unwrap().style.fg, Some(Color::Yellow));
        }
    }
}

#[test]
fn pulse_rect_matches_actual_clipped_connector_and_rendered_icon() {
    for width in WIDTHS {
        for depth in [1, 8, 64] {
            let mut item = session();
            item.input_needed = true;
            let (line, connector_width) = session_line_with_connector(
                &item,
                None,
                connector(depth),
                true,
                Instant::now(),
                width,
            );
            assert_eq!(line.style.bg, Some(SELECTED_ROW_BG));
            let mut app = App::default();
            app.sessions.push(item);
            app.tree
                .visible
                .push(TreeEntry::Session { session: 0, depth });
            let area = Rect::new(3, 1, width as u16, 1);
            let mut terminal = Terminal::new(TestBackend::new(width as u16 + 6, 3)).unwrap();
            let mut pulse_rects = Vec::new();
            terminal
                .draw(|frame| {
                    render_tree(
                        frame,
                        area,
                        &mut app,
                        LayoutMode::Narrow,
                        Instant::now(),
                        &mut pulse_rects,
                    );
                })
                .unwrap();
            assert_eq!(pulse_rects.len(), 1);
            let (key, rect, selected) = &pulse_rects[0];
            assert_eq!(key, &session_key(&app.sessions[0]));
            assert!(*selected);
            assert_eq!(
                *rect,
                Rect::new(area.x + connector_width as u16, area.y, 1, 1)
            );
            let cell = &terminal.backend().buffer()[(rect.x, rect.y)];
            assert_eq!(cell.symbol(), "◆", "width={width}, depth={depth}");
            assert_eq!(cell.fg, Color::Yellow);
            assert_eq!(cell.bg, SELECTED_ROW_BG);
            assert!(cell.modifier.contains(Modifier::BOLD));
            let id_cell = &terminal.backend().buffer()[(rect.x + 2, rect.y)];
            assert_eq!(id_cell.symbol(), "i");
            assert_eq!(id_cell.fg, Color::Yellow);
        }
    }
}

#[test]
fn compact_age_keeps_minutes_hours_and_old_sessions_small() {
    let now = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
    for (started, expected) in [
        (now + Duration::hours(1), "now"),
        (now, "now"),
        (now - Duration::seconds(59), "now"),
        (now - Duration::minutes(1), "1m"),
        (now - Duration::minutes(59), "59m"),
        (now - Duration::hours(2), "2h"),
        (now - Duration::hours(23), "23h"),
        (now - Duration::days(3), "3d"),
        (now - Duration::days(365), "365d"),
    ] {
        assert_eq!(compact_tree_start(started, now), expected);
    }
}

#[test]
fn ids_are_unpadded_and_long_ids_use_shared_ellipsis_truncation() {
    let mut item = session();
    for (id, expected) in [
        ("ident42", "ident42"),
        ("123456789abcdef", "1234567…"),
        ("会话标识符很长", "会话标…"),
    ] {
        item.id = id.to_string();
        let (line, icon_x) =
            session_line_with_connector(&item, None, connector(1), false, Instant::now(), 80);
        assert_eq!(truncate(id, SESSION_ID_WIDTH), expected);
        let id_span = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == expected)
            .expect("fixed short id span");
        assert!(id_span.width() <= SESSION_ID_WIDTH);
        let buffer = render_line(line, 80);
        let first = expected.chars().next().unwrap().to_string();
        assert_eq!(buffer[(icon_x as u16 + 2, 0)].symbol(), first);
    }
}

#[test]
fn dense_rows_keep_identity_title_and_age_without_filling_to_the_right_edge() {
    let mut item = session();
    item.title = Some("Release".into());
    item.command = "runner".into();
    item.args.clear();
    item.pid = Some(42856);
    item.last_total_bytes = 18432;
    for status in ["running", "stopped"] {
        item.status = status.into();
        for width in WIDTHS {
            for depth in [0, 1, 2, 8, 40, 128] {
                let (line, icon_x) = session_line_with_connector(
                    &item,
                    None,
                    connector(depth),
                    false,
                    Instant::now(),
                    width,
                );
                assert!(line.width() <= width, "{width}/{depth}: {:?}", text(&line));
                let row = text(&line);
                assert!(row.contains("ident42 "), "{row}");
                assert!(
                    row.contains("Release 3d"),
                    "title and age must be adjacent: {row}"
                );
                assert!(
                    !row.chars().skip(icon_x).collect::<String>().contains("   "),
                    "no layout padding: {row}"
                );
                if width >= 80 {
                    assert!(row.contains("out:18K"), "{row}");
                    assert!(!row.contains("pid:"), "{row}");
                    assert!(row.contains("│ runner"), "{row}");
                    assert!(
                        line.width() < width,
                        "short commands must not be padded to fill the row: {row}"
                    );
                }
                let buffer = render_line(line, width);
                assert_eq!(
                    buffer[(icon_x as u16, 0)].symbol(),
                    if status == "running" { "●" } else { "○" }
                );
            }
        }
    }
}

#[test]
fn sparse_screenshot_regression_packs_metrics_next_to_notes_and_codex() {
    let mut item = session();
    item.id = "86acb7b".into();
    item.title = Some("Notes".into());
    item.command = "codex".into();
    item.args.clear();
    item.started_at = Some(Utc::now() - Duration::minutes(52) - Duration::seconds(5));
    item.pid = Some(41740);
    item.last_total_bytes = 587 * 1024;
    item.input_needed = true;
    let (line, icon_x) =
        session_line_with_connector(&item, None, connector(6), true, Instant::now(), 160);
    let row = text(&line);
    assert_eq!(
        &row.chars().skip(icon_x).collect::<String>(),
        "◆ 86acb7b Notes 52m out:587K 0B/s wait │ codex"
    );
    assert!(
        !row.contains('▁'),
        "flat idle charts have no information value"
    );
    assert!(
        line.width() < 80,
        "wide terminal must not introduce arbitrary gaps: {row}"
    );
    let buffer = render_line(line, 160);
    let age_byte = row.find("52m").unwrap();
    let age_x = UnicodeWidthStr::width(&row[..age_byte]);
    assert_eq!(buffer[(age_x as u16, 0)].symbol(), "5");
    assert_eq!(buffer[(159, 0)].symbol(), " ");
}

#[test]
fn density_keeps_output_and_numeric_rate_even_for_long_commands() {
    let mut item = session();
    item.title = Some("Deploy".into());
    item.pid = Some(42856);
    item.last_total_bytes = 123 * 1024;
    item.args = vec!["--configuration".into(), "long-config".repeat(40)];
    let now = Instant::now();
    let mut rate = RateState::new(&item, now);
    rate.rate = 2048.0;
    rate.previous_rate = 2048.0;
    rate.history = [1024.0, 2048.0, 1024.0, 2048.0, 1024.0].into();
    for width in [60, 80, 120, 160] {
        let (line, _) =
            session_line_with_connector(&item, Some(&rate), connector(1), false, now, width);
        let row = text(&line);
        assert!(line.width() <= width, "{row}");
        assert!(row.contains("Deploy 3d out:123K 2.0K/s"), "{width}: {row}");
        if width >= 80 {
            assert!(row.contains("│ runner"), "{row}");
            assert!(
                row.ends_with('…'),
                "long command is explicitly truncated: {row}"
            );
            assert!(
                row.contains(&sparkline(Some(&rate), SPARKLINE_WIDTH)),
                "{row}"
            );
        }
    }
}

#[test]
fn titles_and_metrics_precede_commands_and_long_titles_get_explicit_ellipsis() {
    let mut item = session();
    item.pid = Some(41740);
    for width in [40, 60, 80, 120] {
        let (line, _) =
            session_line_with_connector(&item, None, connector(1), false, Instant::now(), width);
        let row = text(&line);
        assert!(line.width() <= width);
        assert!(row.contains("Release"), "{row}");
        assert!(
            row.contains("… 3d"),
            "title must truncate before swallowing age: {row}"
        );
        if let Some(command) = row.find("runner") {
            assert!(row.find("Release").unwrap() < row.find("3d").unwrap());
            assert!(row.find("3d").unwrap() < command);
        }
    }
}

#[test]
fn empty_titles_use_basename_and_commands_keep_arguments() {
    let mut item = session();
    for command in ["/usr/local/bin/runner", r"C:\Tools\runner"] {
        item.command = command.into();
        for title in [None, Some(String::new()), Some("   ".into())] {
            item.title = title;
            let (line, _) =
                session_line_with_connector(&item, None, connector(1), false, Instant::now(), 80);
            let row = text(&line);
            assert!(row.contains("runner 3d"), "{row}");
            assert!(row.contains("│ runner --verbose"), "{row}");
            assert!(!row.contains("/usr/local/bin"));
            assert!(!row.contains(r"C:\Tools"));
        }
    }
}

#[test]
fn dense_unicode_rows_stay_bounded_and_metadata_is_not_overwritten() {
    let mut item = session();
    item.title = Some("部署检查清单 e\u{301} 🦀 测试验证结果".repeat(6));
    item.command = "/工具/执行器".into();
    item.args = vec!["--标签=生产环境".repeat(20)];
    let node = folder(&"很长的目录名称 🦀 e\u{301}".repeat(12));
    for width in WIDTHS {
        for depth in [1, 40] {
            let (line, icon_x) = session_line_with_connector(
                &item,
                None,
                connector(depth),
                false,
                Instant::now(),
                width,
            );
            let row = text(&line);
            assert!(line.width() <= width, "{row}");
            assert!(row.contains('…'), "{row}");
            assert!(row.contains(" 3d"), "{row}");
            let age_byte = row.find("3d").unwrap();
            let age_x = UnicodeWidthStr::width(&row[..age_byte]);
            let buffer = render_line(line, width);
            assert_eq!(buffer[(icon_x as u16, 0)].symbol(), "●");
            assert_eq!(buffer[(age_x as u16, 0)].symbol(), "3");
            assert_eq!(buffer[(age_x as u16 + 1, 0)].symbol(), "d");
            let line =
                folder_line_with_width(&node, connector(depth), false, None, true, false, width);
            assert!(line.width() <= width);
            assert!(text(&line).ends_with('…'));
        }
    }
}

#[test]
fn dense_tiny_viewports_never_overflow_and_keep_icon_visible() {
    let mut item = session();
    for id in ["x", "ident42", "123456789abcdef", "会话标识符很长"] {
        item.id = id.into();
        for width in 0..=24 {
            for depth in [0, 1, 64] {
                let (line, icon_x) = session_line_with_connector(
                    &item,
                    None,
                    connector(depth),
                    false,
                    Instant::now(),
                    width,
                );
                assert!(line.width() <= width, "{width}: {:?}", text(&line));
                if width > 0 {
                    let buffer = render_line(line, width);
                    assert_eq!(buffer[(icon_x as u16, 0)].symbol(), "●");
                }
            }
        }
    }
}

#[test]
fn metrics_include_tags_but_not_pid_or_view_counts() {
    let mut item = session();
    item.title = Some("Deploy".into());
    item.args.clear();
    item.attach_count = 2;
    item.tags = vec!["prod".into(), "api".into()];
    let (line, _) =
        session_line_with_connector(&item, None, connector(1), false, Instant::now(), 120);
    let row = text(&line);
    assert!(row.contains("[prod,api] │ runner"), "{row}");
    assert!(!row.contains("view:"), "{row}");
    assert!(!row.contains("pid:"), "{row}");
}

#[test]
fn dense_selection_and_inactive_styles_preserve_status_and_metrics() {
    let mut item = session();
    item.title = Some("Release".into());
    for (status, attention, selected) in [
        ("running", false, true),
        ("stopped", false, true),
        ("running", true, true),
        ("stopped", false, false),
    ] {
        item.status = status.into();
        item.input_needed = attention;
        let (glyph, style) = session_status_style(&item, selected);
        let (line, icon_x) =
            session_line_with_connector(&item, None, connector(1), selected, Instant::now(), 80);
        let row = text(&line);
        let title_byte = row.find("Release").unwrap();
        let title_x = UnicodeWidthStr::width(&row[..title_byte]);
        let age_byte = row.find("3d").unwrap();
        let age_x = UnicodeWidthStr::width(&row[..age_byte]);
        let buffer = render_line(line, 80);
        assert_eq!(buffer[(icon_x as u16, 0)].symbol(), glyph);
        assert_eq!(buffer[(icon_x as u16, 0)].fg, style.fg.unwrap());
        assert_eq!(buffer[(icon_x as u16 + 2, 0)].fg, style.fg.unwrap());
        assert_eq!(
            buffer[(title_x as u16, 0)].fg,
            if selected {
                Color::White
            } else {
                Color::DarkGray
            }
        );
        assert_eq!(
            buffer[(age_x as u16, 0)].fg,
            if selected {
                Color::White
            } else {
                Color::DarkGray
            }
        );
    }
}

#[test]
fn narrow_tree_reserves_executable_before_secondary_metrics() {
    let mut item = session();
    item.title = Some("Notes".into());
    item.command = "codex".into();
    item.args.clear();
    item.pid = Some(41740);
    item.last_total_bytes = 587 * 1024;
    for width in [40, 50, 60] {
        let (line, _) =
            session_line_with_connector(&item, None, connector(4), false, Instant::now(), width);
        let row = text(&line);
        assert!(row.contains("Notes 3d"), "{row}");
        assert!(
            row.contains("│ codex"),
            "command executable must survive: {row}"
        );
        assert!(line.width() <= width);
    }
}

#[test]
fn exactly_one_space_separates_session_id_and_title() {
    let mut item = session();
    item.title = Some("Notes".into());
    item.command = "codex".into();
    item.args.clear();
    item.pid = Some(41740);
    item.attach_count = 3;
    for id in [
        "a",
        "86acb7b",
        "12345678",
        "123456789abcdef",
        "会话标识符很长",
    ] {
        item.id = id.into();
        for width in [24, 40, 60, 80, 120] {
            let (line, _) = session_line_with_connector(
                &item,
                None,
                connector(1),
                false,
                Instant::now(),
                width,
            );
            let row = text(&line);
            let expected_id = truncate(id, SESSION_ID_WIDTH);
            assert!(
                row.contains(&format!("{expected_id} Notes")),
                "{width}: {row}"
            );
            assert!(!row.contains("pid:"), "{row}");
            assert!(!row.contains("view:"), "{row}");
            assert!(line.width() <= width);
        }
    }
}
