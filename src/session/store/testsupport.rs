//! Shared fixtures for the `store` submodule tests.
//!
//! Each concern-specific test module needs the same handful of runtime and
//! store builders, so they live here instead of being duplicated four times.

#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use parking_lot::RwLock;

use crate::config::AppConfig;
use crate::db::Database;
use crate::session::{SessionMeta, SessionStatus};

use super::{SessionHandle, SessionStore};

pub(super) fn make_runtime(
    id: &str,
    status: SessionStatus,
    excerpt: &str,
    last_output_ago: Option<Duration>,
) -> Arc<RwLock<super::super::runtime::SessionRuntime>> {
    use tokio::sync::{broadcast, mpsc};

    let dir = std::env::temp_dir().join(format!("oly_store_test_{id}_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create runtime test dir");
    if !excerpt.is_empty() {
        crate::session::persist::append_output_raw(&dir, excerpt.as_bytes())
            .expect("persist runtime excerpt");
    }

    let meta = SessionMeta {
        id: id.to_string(),
        title: None,
        tags: vec![],
        command: "sh".to_string(),
        args: vec![],
        cwd: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        ended_at: None,
        status,
        pid: None,
        exit_code: None,
    };

    let last_output_at = last_output_ago.map(|ago| Instant::now() - ago);
    let mut screen_parser =
        vt100::Parser::new(24, 80, crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS);
    if !excerpt.is_empty() {
        screen_parser.process(excerpt.as_bytes());
    }

    let (broadcast_tx, _rx) = broadcast::channel(4);
    let (resize_tx, _resize_rx) = broadcast::channel(4);
    let (writer_tx, _writer_rx) = mpsc::channel(8);
    let (child, pty_master) = make_dummy_child();
    Arc::new(RwLock::new(super::super::runtime::SessionRuntime {
        meta,
        dir,
        last_total_bytes: excerpt.as_bytes().len() as u64,
        raw_total_bytes: excerpt.as_bytes().len() as u64,
        broadcast_tx,
        resize_tx,
        pty: super::super::pty::PtyHandle {
            child,
            writer_tx,
            pty_master: parking_lot::Mutex::new(Some(pty_master)),
        },
        pty_size: None,
        resize_history: Vec::new(),
        completed_at: None,
        persisted: false,
        requested_final_status: None,
        spawned_at: Instant::now(),
        last_output_epoch: last_output_at,
        last_input_at: None,
        last_attach_activity_at: None,
        attach_count: 0,
        last_notified_at: None,
        notified_output_epoch: None,
        screen_parser,
        screen_scrollback_rows: crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS,
        terminal_signals: Default::default(),
        shared_modes: Default::default(),
        output_closed: false,
        notifications_enabled: true,
    }))
}

pub(super) fn make_dummy_child() -> (
    super::super::pty::RuntimeChild,
    Box<dyn portable_pty::MasterPty + Send>,
) {
    // Spawn a long-running process so refresh_status() sees it still alive.
    // We must also return the PTY master to keep the child alive — dropping
    // the master sends EOF/HUP to the child, which would cause it to exit.
    #[cfg(target_os = "windows")]
    let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
    #[cfg(target_os = "windows")]
    cmd.args(["/c", "ping", "127.0.0.1", "-n", "120"]);
    #[cfg(not(target_os = "windows"))]
    let mut cmd = portable_pty::CommandBuilder::new("sleep");
    #[cfg(not(target_os = "windows"))]
    cmd.args(["60"]);

    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty in test");
    let child = pty.slave.spawn_command(cmd).expect("spawn in test");
    (super::super::pty::RuntimeChild::Pty(child), pty.master)
}

pub(super) async fn make_test_db() -> Arc<Database> {
    // Use a unique per-test file-based DB in the temp directory so
    // concurrent tests don't interfere with each other.
    let path = std::env::temp_dir().join(format!("oly_test_{}.db", uuid::Uuid::new_v4()));
    Arc::new(
        Database::open(&path, std::env::temp_dir())
            .await
            .expect("open test DB"),
    )
}

pub(super) fn store_with(
    runtimes: Vec<Arc<RwLock<super::super::runtime::SessionRuntime>>>,
    db: Arc<Database>,
) -> SessionStore {
    let store = SessionStore::new(900, db);
    for rt in runtimes {
        let id = rt.read().meta.id.clone();
        let handle = Arc::new(SessionHandle::new(rt));
        store.sessions.rcu(|current| {
            let mut next = (**current).clone();
            next.insert(id.clone(), handle.clone());
            next
        });
    }
    store
}

// -----------------------------------------------------------------------
// silent_candidates
// -----------------------------------------------------------------------

pub(super) fn make_runtime_writable(
    id: &str,
    status: SessionStatus,
) -> (
    Arc<RwLock<super::super::runtime::SessionRuntime>>,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    make_runtime_writable_with_capacity(id, status, 8)
}

pub(super) fn make_runtime_writable_with_capacity(
    id: &str,
    status: SessionStatus,
    capacity: usize,
) -> (
    Arc<RwLock<super::super::runtime::SessionRuntime>>,
    tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    use tokio::sync::{broadcast, mpsc};

    let dir =
        std::env::temp_dir().join(format!("oly_store_writable_{id}_{}", uuid::Uuid::new_v4()));
    let meta = SessionMeta {
        id: id.to_string(),
        title: None,
        tags: vec![],
        command: "sh".to_string(),
        args: vec![],
        cwd: None,
        created_at: Utc::now(),
        started_at: Some(Utc::now()),
        ended_at: None,
        status,
        pid: None,
        exit_code: None,
    };
    let (broadcast_tx, _rx) = broadcast::channel(4);
    let (resize_tx, _resize_rx) = broadcast::channel(4);
    let (writer_tx, writer_rx) = mpsc::channel(capacity.max(1));
    let (child, pty_master) = make_dummy_child();
    let rt = Arc::new(RwLock::new(super::super::runtime::SessionRuntime {
        meta,
        dir,
        last_total_bytes: 0,
        raw_total_bytes: 0,
        broadcast_tx,
        resize_tx,
        pty: super::super::pty::PtyHandle {
            child,
            writer_tx,
            pty_master: parking_lot::Mutex::new(Some(pty_master)),
        },
        pty_size: None,
        resize_history: Vec::new(),
        completed_at: None,
        persisted: false,
        requested_final_status: None,
        spawned_at: Instant::now(),
        last_output_epoch: None,
        last_input_at: None,
        last_attach_activity_at: None,
        attach_count: 0,
        last_notified_at: None,
        notified_output_epoch: None,
        screen_parser: vt100::Parser::new(24, 80, crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS),
        screen_scrollback_rows: crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS,
        terminal_signals: Default::default(),
        shared_modes: Default::default(),
        output_closed: false,
        notifications_enabled: true,
    }));
    (rt, writer_rx)
}
pub(super) fn make_test_config(max_running_sessions: usize) -> AppConfig {
    use std::path::PathBuf;
    AppConfig {
        http_bind: "127.0.0.1".to_string(),
        silence_seconds: 10,
        stop_grace_seconds: 5,
        session_eviction_seconds: 15,
        http_port: 0,
        log_level: "info".into(),
        prompt_patterns: vec![],
        web_push_vapid_public_key: None,
        web_push_vapid_private_key: None,
        web_push_subject: None,
        web_push_proxy: None,
        state_dir: PathBuf::from("."),
        sessions_dir: PathBuf::from("."),
        db_file: PathBuf::from("."),
        socket_name: "test.sock".into(),
        socket_file: PathBuf::from("."),
        info_file: PathBuf::from("."),
        lock_file: PathBuf::from("."),
        max_running_sessions,
        screen_scrollback_rows: crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS,
        notification_hook: None,
        runtime_overrides: Default::default(),
    }
}

#[cfg(target_os = "windows")]
pub(super) fn expected_soft_stop_inputs() -> Vec<Vec<u8>> {
    vec![vec![0x03], vec![0x03], vec![0x1a, b'\r']]
}

#[cfg(not(target_os = "windows"))]
pub(super) fn expected_soft_stop_inputs() -> Vec<Vec<u8>> {
    vec![vec![0x03], vec![0x03], vec![0x04]]
}
