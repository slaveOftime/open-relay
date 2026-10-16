//! Manual performance probes for the session TUI with many sessions.
//!
//! These tests are `#[ignore]`d because they are measurement tools, not
//! assertions; run them explicitly against a release build:
//!
//! ```text
//! cargo test --release --bin oly perf_tests -- --ignored --nocapture
//! ```
//!
//! They answer the question behind `oly ls -f --node-all --limit 5000`
//! feeling slow: how much wall time do a refresh tick and one frame cost
//! when the list holds thousands of sessions? Numbers feed PERFORMANCE.md.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use ratatui::{Terminal, backend::TestBackend};

use super::app::App;
use super::refresh::{SessionRefresh, apply_refresh};
use super::view::render;
use crate::protocol::SessionSummary;

const SESSION_COUNT: usize = 5000;
const FRAME_WIDTH: u16 = 140;
const FRAME_HEIGHT: u16 = 40;
const FRAMES: usize = 10;
const REFRESHES: usize = 10;

const COMMANDS: [(&str, &str, &str); 8] = [
    ("claude", "--continue", "Claude Code session"),
    ("codex", "--model gpt-5", "Codex run"),
    ("copilot", "suggest", "Copilot chat"),
    ("cargo", "test --locked", "cargo test"),
    ("npm", "run build", "web build"),
    ("python", "-m pytest -q", "pytest"),
    ("node", "scripts/server.js", "server"),
    ("git", "rebase -i origin/main", "rebase"),
];
const CWDS: [&str; 8] = [
    "C:/Users/dev/Code/open-relay/src",
    "C:/Users/dev/Code/open-relay/web/src",
    "C:/Users/dev/Code/open-relay/docs",
    "C:/Users/dev/Code/web-app/src",
    "C:/Users/dev/Code/web-app/tests",
    "C:/Users/dev/Code/tools",
    "C:/Users/dev/scratch",
    "C:/Users/dev/Code/archive",
];

fn fake_sessions(count: usize) -> Vec<SessionSummary> {
    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    (0..count)
        .map(|index| {
            let (command, args, title) = COMMANDS[index % COMMANDS.len()];
            let active = index % 20 == 0;
            let failed = !active && index % 37 == 0;
            SessionSummary {
                id: format!("be{index:06}"),
                title: Some(format!("{title} #{index}")),
                tags: if index % 5 == 0 {
                    vec!["prod".to_string()]
                } else {
                    Vec::new()
                },
                command: command.to_string(),
                args: args.split_whitespace().map(str::to_string).collect(),
                pid: active.then_some(1000 + index as u32),
                status: if active {
                    "running".to_string()
                } else if failed {
                    "failed".to_string()
                } else {
                    "stopped".to_string()
                },
                created_at: base + chrono::TimeDelta::minutes(index as i64 * 7),
                started_at: Some(base + chrono::TimeDelta::minutes(index as i64 * 7)),
                ended_at: (!active)
                    .then(|| base + chrono::TimeDelta::minutes(index as i64 * 7 + 30)),
                resume_command: None,
                cwd: Some(CWDS[index % CWDS.len()].to_string()),
                input_needed: active && index % 100 == 0,
                program_status: None,
                notifications_enabled: true,
                node: None,
                last_total_bytes: (index as u64) * 4096,
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
        })
        .collect()
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

fn report(label: &str, total: Duration, iterations: usize) {
    println!(
        "{label}: total {:.1?} · {:.0} µs/op over {iterations} runs",
        total,
        micros(total) / iterations as f64
    );
}

#[test]
#[ignore = "manual perf probe; run with --release --ignored --nocapture"]
fn perf_many_sessions_refresh_and_render_cost() {
    let sessions = fake_sessions(SESSION_COUNT);
    let query = crate::protocol::ListQuery {
        search: None,
        tags: Vec::new(),
        statuses: Vec::new(),
        since: None,
        until: None,
        limit: SESSION_COUNT,
        offset: 0,
        sort: crate::protocol::ListSortField::CreatedAt,
        order: crate::protocol::SortOrder::Desc,
    };

    let mut app = App::default();
    let started = Instant::now();
    app.replace_sessions(sessions.clone());
    report("replace_sessions (cold)", started.elapsed(), 1);

    let mut total = Duration::ZERO;
    for _ in 0..REFRESHES {
        let input = sessions.clone();
        let started = Instant::now();
        app.replace_sessions(input);
        total += started.elapsed();
    }
    report("replace_sessions (warm, unchanged data)", total, REFRESHES);

    let mut total = Duration::ZERO;
    for _ in 0..REFRESHES {
        let refresh = SessionRefresh {
            sessions: sessions.clone(),
            failed_nodes: HashSet::new(),
            failures: Vec::new(),
        };
        let started = Instant::now();
        apply_refresh(&mut app, &query, Ok(refresh));
        total += started.elapsed();
    }
    report("apply_refresh (warm, unchanged data)", total, REFRESHES);

    let backend = TestBackend::new(FRAME_WIDTH, FRAME_HEIGHT);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("warmup draw");
    let mut total = Duration::ZERO;
    for _ in 0..FRAMES {
        let started = Instant::now();
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw");
        total += started.elapsed();
    }
    report("render list mode (140x40)", total, FRAMES);

    // Selection in the middle of the list: same viewport work expected.
    app.selected = SESSION_COUNT / 2;
    let mut total = Duration::ZERO;
    for _ in 0..FRAMES {
        let started = Instant::now();
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw");
        total += started.elapsed();
    }
    report("render middle selection", total, FRAMES);

    // Filter typing: one keystroke re-filters and re-renders the tree.
    let mut total = Duration::ZERO;
    for character in "web-app".chars() {
        let started = Instant::now();
        app.push_filter(character);
        total += started.elapsed();
    }
    report("filter keystrokes after base filter", total, 7);
    let started = Instant::now();
    app.clear_filter();
    report("clear filter", started.elapsed(), 1);
    let started = Instant::now();
    app.push_filter('w');
    report("first filter keystroke", started.elapsed(), 1);
    let started = Instant::now();
    app.clear_filter();
    report("clear filter again", started.elapsed(), 1);

    // Tree mode draws only the viewport of rows.
    app.toggle_view_mode();
    let mut total = Duration::ZERO;
    for _ in 0..FRAMES {
        let started = Instant::now();
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw");
        total += started.elapsed();
    }
    report("render tree mode (140x40)", total, FRAMES);
    let started = Instant::now();
    app.toggle_view_mode();
    report("toggle back to list", started.elapsed(), 1);

    println!(
        "sessions={SESSION_COUNT} visible={} adapters={}",
        app.visible.len(),
        app.tree.nodes.len()
    );
}

#[test]
#[ignore = "manual perf probe; run with --release --ignored --nocapture"]
fn perf_many_sessions_replace_phases() {
    // Break `replace_sessions` down by phase so the next optimization targets
    // whatever actually dominates after the key/search/row caches.
    let sessions = fake_sessions(SESSION_COUNT);
    let mut app = App::default();
    app.replace_sessions(sessions.clone());

    let rounds = 5;
    let mut sort_total = Duration::ZERO;
    let mut keys_total = Duration::ZERO;
    let mut visible_total = Duration::ZERO;
    let mut tree_total = Duration::ZERO;
    let mut nodes_total = Duration::ZERO;
    for _ in 0..rounds {
        let mut input = sessions.clone();
        let started = Instant::now();
        super::app::sort_sessions(&mut input, app.sort_strategy);
        sort_total += started.elapsed();

        let started = Instant::now();
        let keys: Vec<String> = input.iter().map(super::app::session_key).collect();
        keys_total += started.elapsed();
        std::hint::black_box(&keys);

        app.rebuild_visible();
        let started = Instant::now();
        app.rebuild_tree();
        tree_total += started.elapsed();
        let started = Instant::now();
        app.build_tree_nodes();
        nodes_total += started.elapsed();
        let started = Instant::now();
        app.rebuild_visible();
        visible_total += started.elapsed();

        app.replace_sessions(input);
    }
    report("sort_sessions", sort_total, rounds);
    report("parallel keys build", keys_total, rounds);
    report("rebuild_visible (incl. tree)", visible_total, rounds);
    report("rebuild_tree", tree_total, rounds);
    report("build_tree_nodes", nodes_total, rounds);

    // Filter changes rebuild the tree with an active filter; kept here so the
    // cost of the lazy search index is visible.
    let mut total = Duration::ZERO;
    for _ in 0..rounds {
        let started = Instant::now();
        app.push_filter('w');
        total += started.elapsed();
        app.clear_filter();
    }
    report("push_filter warm", total, rounds);
}

#[test]
#[ignore = "probe: time DB phases against a real bench DB (OLY_BENCH_DB)"]
fn perf_probe_db_phases() {
    let db_path = std::env::var("OLY_BENCH_DB").expect("set OLY_BENCH_DB to oly.db");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let db = crate::db::Database::open(std::path::Path::new(&db_path), "sessions".into())
            .await
            .expect("open db");
        let query = crate::protocol::ListQuery {
            search: None,
            tags: Vec::new(),
            statuses: Vec::new(),
            since: None,
            until: None,
            limit: SESSION_COUNT,
            offset: 0,
            sort: crate::protocol::ListSortField::CreatedAt,
            order: crate::protocol::SortOrder::Desc,
        };
        let started = Instant::now();
        let sessions = db
            .list_summaries_without_offsets(&query)
            .await
            .expect("list");
        report("db.list_summaries_without_offsets", started.elapsed(), 1);

        let started = Instant::now();
        for session in &sessions {
            let exists = db.session_exists(&session.id).await;
            assert!(exists);
        }
        report("db.session_exists x n", started.elapsed(), sessions.len());

        let started = Instant::now();
        for session in &sessions {
            let dir = db.session_dir_by_id(&session.id);
            std::hint::black_box(dir.is_dir());
        }
        report("session dir is_dir x n", started.elapsed(), sessions.len());
    });
}
