//! Tests for the log index and renderer.
//!
//! These live in one module rather than beside each half because the golden
//! transcript fixtures exercise the whole path: index a real `output.log`, then
//! replay it and compare the rendered rows byte for byte.
#![cfg(test)]

use super::index::LOG_RECORD_FALLBACK_BYTES;
use super::index::{
    parse_resize_event, read_persisted_log_page, read_relevant_resize_events, read_resize_events,
    split_persisted_log_records, split_rendered_log_output, sync_persisted_log_index,
};
use super::render::{parser_cols, parser_rows, render_log_bytes, render_log_file, render_screen};
use super::{ViewportReplayPlan, ViewportSize};
use crate::protocol::LogResize;
use crate::session::persist::append_output_raw;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn expected_fixture(name: &str) -> Vec<u8> {
    let name = fixture_name(name);
    let bytes = fs::read(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(name),
    )
    .expect("read expected fixture");

    normalize_fixture_line_endings(&bytes)
}

fn assert_fixture_or_update(name: &str, output: &[u8]) {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(fixture_name(name));
    if std::env::var_os("OLY_UPDATE_LOG_FIXTURES").is_some() {
        fs::write(&fixture_path, output).expect("write updated log fixture");
    }

    assert_eq!(output, expected_fixture(name));
}

fn fixture_name(name: &str) -> &str {
    #[cfg(windows)]
    if name == "output-copilot.expected" {
        return "output-copilot.expected.windows";
    }

    name
}

fn normalize_fixture_line_endings(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
            continue;
        }

        normalized.push(bytes[index]);
        index += 1;
    }

    normalized
}

fn empty_plan() -> ViewportReplayPlan {
    ViewportReplayPlan::default()
}

fn temp_session_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

#[test]
fn renders_copilot_transcript_exactly() {
    let log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("output-copilot.log");

    let output = render_log_file(
        &log_path,
        40,
        true,
        100,
        Some(ViewportSize {
            rows: 37,
            cols: 105,
        }),
    )
    .expect("render copilot output log with color");

    assert_fixture_or_update("output-copilot.expected", &output);
}

#[test]
fn renders_opencode_transcript_exactly() {
    let log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("output-opencode.log");

    let output = render_log_file(
        &log_path,
        40,
        true,
        100,
        Some(ViewportSize {
            rows: 37,
            cols: 105,
        }),
    )
    .expect("render opencode output log with color");

    assert_fixture_or_update("output-opencode.expected", &output);
}

#[test]
fn keeps_visible_rows_below_cursor() {
    let bytes = b"\x1b[2J\x1b[1;1HTitle\x1b[5;1HOption 1\x1b[6;1HOption 2\x1b[2;1HSearch";

    let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
    let rendered = String::from_utf8_lossy(&output);

    assert!(rendered.contains("Title"));
    assert!(rendered.contains("Search"));
    assert!(rendered.contains("Option 1"));
    assert!(rendered.contains("Option 2"));
}

#[test]
fn render_screen_respects_tail_limit() {
    let mut parser = vt100::Parser::new(4, 80, 0);
    parser.process(b"\x1b[1;1Hone\x1b[2;1Htwo\x1b[3;1Hthree\x1b[4;1Hfour");

    let output = render_screen(&parser, 2, false, 80);

    assert_eq!(String::from_utf8_lossy(&output), "three\nfour\n");
}

#[test]
fn drops_stale_alt_screen_content_before_latest_redraw() {
    let bytes = concat!(
        "\x1b[?1049h",
        "\x1b[20;1Hstale",
        "\x1b[2J\x1b[HTitle",
        "\x1b[5;1HOption 1",
        "\x1b[6;1HOption 2",
        "\x1b[2;1HSearch"
    )
    .as_bytes();

    let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
    let rendered = String::from_utf8_lossy(&output);

    assert!(!rendered.contains("stale"));
    assert!(rendered.contains("Title"));
    assert!(rendered.contains("Option 1"));
    assert!(rendered.contains("Option 2"));
}

#[test]
fn falls_back_to_previous_non_empty_alt_screen_frame() {
    let bytes = concat!(
        "\x1b[?1049h",
        "\x1b[2J\x1b[HTitle",
        "\x1b[2;1HSearch",
        "\x1b[H\x1b[2J"
    )
    .as_bytes();

    let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
    let rendered = String::from_utf8_lossy(&output);

    assert!(rendered.contains("Title"));
    assert!(rendered.contains("Search"));
}

#[test]
fn falls_back_when_alt_screen_teardown_clears_final_output() {
    let bytes = concat!("\x1b[?1049h", "\x1b[2J\x1b[HMenu", "\x1b[?1049l").as_bytes();

    let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
    let rendered = String::from_utf8_lossy(&output);

    assert!(rendered.contains("Menu"));
}

#[test]
fn parses_resize_events() {
    let parsed = parse_resize_event("resize offset=42 rows=37 cols=105");

    assert_eq!(
        parsed,
        Some(LogResize {
            offset: 42,
            rows: 37,
            cols: 105,
        })
    );
}

#[test]
fn reads_all_resize_events_from_events_log() {
    let temp_dir = std::env::temp_dir().join(format!(
        "oly-log-render-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));

    fs::create_dir_all(&temp_dir).expect("create temp dir");

    let log_path = temp_dir.join("output.log");
    let events_path = temp_dir.join("events.log");

    fs::write(&log_path, b"placeholder").expect("write output log");
    fs::write(
        &events_path,
        b"resize offset=0 rows=24 cols=80\nresize offset=10 rows=30 cols=90\nresize offset=20 rows=37 cols=105\n",
    )
    .expect("write events log");

    let resizes = read_resize_events(&temp_dir).expect("read resizes");

    assert_eq!(
        resizes,
        vec![
            LogResize {
                offset: 0,
                rows: 24,
                cols: 80,
            },
            LogResize {
                offset: 10,
                rows: 30,
                cols: 90,
            },
            LogResize {
                offset: 20,
                rows: 37,
                cols: 105,
            },
        ]
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn keeps_last_resize_before_tail_and_future_resizes() {
    let temp_dir = std::env::temp_dir().join(format!(
        "oly-log-render-plan-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));

    fs::create_dir_all(&temp_dir).expect("create temp dir");
    let log_path = temp_dir.join("output.log");
    let events_path = temp_dir.join("events.log");
    fs::write(&log_path, b"placeholder").expect("write output log");
    fs::write(
        &events_path,
        b"resize offset=0 rows=24 cols=80\nresize offset=100 rows=30 cols=90\nresize offset=140 rows=37 cols=105\nresize offset=220 rows=50 cols=140\n",
    )
    .expect("write events log");

    let plan = read_relevant_resize_events(&log_path, 120, 200).expect("read relevant resizes");

    assert_eq!(
        plan,
        ViewportReplayPlan {
            initial: Some(LogResize {
                offset: 100,
                rows: 30,
                cols: 90,
            }),
            resizes: vec![LogResize {
                offset: 20,
                rows: 37,
                cols: 105,
            }],
        }
    );

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn applies_resize_history_during_alt_screen_replay() {
    let bytes = b"\x1b[?1049h\x1b[2J\x1b[H12345";
    let output = render_log_bytes(
        bytes,
        4,
        false,
        10,
        None,
        &ViewportReplayPlan {
            initial: Some(LogResize {
                offset: 0,
                rows: 2,
                cols: 4,
            }),
            resizes: vec![],
        },
    );
    let rendered = String::from_utf8_lossy(&output);

    assert!(rendered.contains("1234"));
    assert!(rendered.contains("5"));
}

#[test]
fn persisted_viewport_overrides_alt_screen_fallback_dimensions() {
    let bytes = b"\x1b[?1049h\x1b[20;1Hstale\x1b[HSelect Model";
    let viewport = Some(ViewportSize { rows: 6, cols: 100 });

    assert_eq!(parser_rows(bytes, true, 10, viewport, &empty_plan()), 6);
    assert_eq!(parser_cols(true, 80, viewport, &empty_plan()), 100);
    assert_eq!(parser_cols(true, 80, None, &empty_plan()), 80);
}

#[test]
fn splits_persisted_logs_on_terminal_boundaries_without_newlines() {
    let records = split_persisted_log_records(
        b"\x1b[2J\x1b[HTitle\x1b[5;1HOption 1\x1b[6;1HOption 2\x1b[2;1HSearch",
    );

    assert_eq!(
        records,
        vec![
            "\x1b[2J".to_string(),
            "\x1b[HTitle".to_string(),
            "\x1b[5;1HOption 1".to_string(),
            "\x1b[6;1HOption 2".to_string(),
            "\x1b[2;1HSearch".to_string(),
        ]
    );
}

#[test]
fn splits_persisted_logs_by_fallback_size_when_no_boundaries_exist() {
    let bytes = vec![b'a'; LOG_RECORD_FALLBACK_BYTES + 17];
    let records = split_persisted_log_records(&bytes);

    assert_eq!(records.len(), 2);
    assert_eq!(records[0].len(), LOG_RECORD_FALLBACK_BYTES);
    assert_eq!(records[1].len(), 17);
}

#[test]
fn persisted_log_index_extends_across_append_boundaries() {
    let temp_dir = temp_session_dir("oly-log-index");
    fs::create_dir_all(&temp_dir).expect("create temp dir");

    let log_path = temp_dir.join("output.log");
    fs::write(&log_path, b"alpha").expect("write initial output log");
    sync_persisted_log_index(&log_path).expect("index initial output log");

    let (lines, total) = read_persisted_log_page(&temp_dir, 0, 10).expect("read initial page");
    assert_eq!(lines, vec!["alpha".to_string()]);
    assert_eq!(total, 1);

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .expect("open output log for append");
    file.write_all(b" beta\ngamma")
        .expect("append output log continuation");
    file.flush().expect("flush appended output log");

    sync_persisted_log_index(&log_path).expect("extend persisted log index");

    let (lines, total) = read_persisted_log_page(&temp_dir, 0, 10).expect("read extended page");
    assert_eq!(lines, vec!["alpha beta\n".to_string(), "gamma".to_string()]);
    assert_eq!(total, 2);

    let (tail_lines, tail_total) =
        read_persisted_log_page(&temp_dir, 1, 10).expect("read trailing page");
    assert_eq!(tail_lines, vec!["gamma".to_string()]);
    assert_eq!(tail_total, 2);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn persisted_log_page_rebuilds_index_after_append_output_raw() {
    let temp_dir = temp_session_dir("oly-log-index-lazy");
    fs::create_dir_all(&temp_dir).expect("create temp dir");

    append_output_raw(&temp_dir, b"alpha").expect("write initial raw output");
    let (lines, total) = read_persisted_log_page(&temp_dir, 0, 10).expect("read initial raw page");
    assert_eq!(lines, vec!["alpha".to_string()]);
    assert_eq!(total, 1);

    append_output_raw(&temp_dir, b" beta\ngamma").expect("append raw output continuation");
    let (lines, total) = read_persisted_log_page(&temp_dir, 0, 10).expect("read extended raw page");
    assert_eq!(lines, vec!["alpha beta\n".to_string(), "gamma".to_string()]);
    assert_eq!(total, 2);

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn split_rendered_log_output_drops_final_reset_suffix() {
    let output = b"alpha\x1b[0m\nbeta\x1b[0m\n\x1b[0m\x1b[39m\x1b[49m\x1b[?25h";

    assert_eq!(
        split_rendered_log_output(output),
        vec!["alpha\x1b[0m\n".to_string(), "beta\x1b[0m\n".to_string()]
    );
}

#[test]
fn read_persisted_log_total_counts_indexed_records() {
    let temp_dir = temp_session_dir("oly-log-total");
    fs::create_dir_all(&temp_dir).expect("create temp dir");

    let log_path = temp_dir.join("output.log");
    fs::write(&log_path, b"one\ntwo\nthree\nfour\n").expect("write output log");
    sync_persisted_log_index(&log_path).expect("index output log");
    assert_eq!(
        read_persisted_log_page(&temp_dir, 0, 1)
            .expect("read total")
            .1,
        4
    );

    let _ = fs::remove_dir_all(&temp_dir);
}
