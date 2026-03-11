#![allow(dead_code)]
//! Shared e2e test infrastructure for oly daemon tests.
//!
//! This module provides:
//! - DaemonGuard: RAII guard for daemon lifecycle
//! - Helper functions for starting sessions, sending input, polling logs
//! - Test environment setup (tmp dirs, socket names, state isolation)
//!
//! ## Named-pipe isolation on Windows
//!
//! The oly IPC socket on Windows is a named pipe with a fixed name
//! (`open-relay.oly.sock`), which is a global OS resource.  All tests in e2e
//! binaries are serialised via `E2E_LOCK` so that no two daemons are started
//! concurrently.  The `cli_errors` test binary never starts a daemon, so there
//! is no cross-binary conflict.

use std::{
    env, fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
    thread::sleep,
    time::{Duration, Instant},
};

// Global e2e serialiser
pub static E2E_LOCK: Mutex<()> = Mutex::new(());

pub fn oly_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oly"))
}

pub fn oly_cmd(tmp_dir: &PathBuf) -> Command {
    let mut cmd = Command::new(oly_bin());
    apply_state_env(&mut cmd, tmp_dir);
    cmd
}

fn apply_state_env(cmd: &mut Command, tmp_dir: &PathBuf) {
    cmd.env("OLY_STATE_DIR", tmp_dir.join("oly"));
    cmd.env("OLY_SOCKET_NAME", socket_name_for_tmp(tmp_dir));
}

fn socket_name_for_tmp(tmp_dir: &PathBuf) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tmp_dir.to_string_lossy().hash(&mut hasher);
    format!("open-relay.oly.{}.sock", hasher.finish())
}

pub fn make_tmp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("oly_e2e_{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

pub fn program_exists(program: &str) -> bool {
    #[cfg(target_os = "windows")]
    let finder = ("where", program);
    #[cfg(not(target_os = "windows"))]
    let finder = ("which", program);

    Command::new(finder.0)
        .arg(finder.1)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct DaemonGuard {
    child: std::process::Child,
    tmp_dir: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = oly_cmd(&self.tmp_dir)
            .args(["daemon", "stop"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        sleep(Duration::from_millis(400));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn start_daemon(tmp: &PathBuf) -> DaemonGuard {
    let log_path = tmp.join("daemon-stderr.log");
    let log_file = fs::File::create(&log_path).expect("create daemon log file");

    let child = oly_cmd(tmp)
        .args([
            "daemon",
            "start",
            "--foreground-internal",
            "--no-auth",
            "--no-http",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
        .expect("failed to spawn daemon process");

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        sleep(Duration::from_millis(250));
        let probe = oly_cmd(tmp)
            .args(["stop", "zzz9999"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("readiness probe (`oly stop`) failed to execute");
        let probe_stderr = String::from_utf8_lossy(&probe.stderr).to_string();
        if probe_stderr.contains("zzz9999") {
            return DaemonGuard {
                child,
                tmp_dir: tmp.clone(),
            };
        }
        if Instant::now() >= deadline {
            let daemon_log = fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "daemon did not become ready within 3 s\n--- daemon stderr ({}) ---\n{}\n--- last probe stderr ---\n{}",
                log_path.display(),
                daemon_log,
                probe_stderr,
            );
        }
    }
}

pub fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("read local addr").port()
}

pub fn start_daemon_http(tmp: &PathBuf, port: u16) -> DaemonGuard {
    let log_path = tmp.join("daemon-stderr.log");
    let log_file = fs::File::create(&log_path).expect("create daemon log file");

    let child = oly_cmd(tmp)
        .args([
            "daemon",
            "start",
            "--foreground-internal",
            "--no-auth",
            "--port",
            &port.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log_file)
        .spawn()
        .expect("failed to spawn daemon process");

    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        sleep(Duration::from_millis(250));
        let probe = oly_cmd(tmp)
            .args(["stop", "zzz9999"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("readiness probe (`oly stop`) failed to execute");
        let probe_stderr = String::from_utf8_lossy(&probe.stderr).to_string();
        if probe_stderr.contains("zzz9999") {
            return DaemonGuard {
                child,
                tmp_dir: tmp.clone(),
            };
        }
        if Instant::now() >= deadline {
            let daemon_log = fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "daemon did not become ready within 4 s\n--- daemon stderr ({}) ---\n{}\n--- last probe stderr ---\n{}",
                log_path.display(),
                daemon_log,
                probe_stderr,
            );
        }
    }
}

pub fn start_session(tmp: &PathBuf, cmd_and_args: &[&str]) -> String {
    let mut args = vec!["start", "--detach"];
    args.extend_from_slice(cmd_and_args);
    let output = oly_cmd(tmp)
        .args(&args)
        .output()
        .expect("`oly start` failed to execute");
    assert!(
        output.status.success(),
        "`oly start` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(id.len(), 7, "expected 7-char session ID, got: {id:?}");
    id
}

pub fn send_line(tmp: &PathBuf, id: &str, text: &str) {
    let output = oly_cmd(tmp)
        .args(["input", id, "--text", text, "--key", "enter"])
        .output()
        .expect("`oly input` failed to execute");
    assert!(
        output.status.success(),
        "`oly input --text …` exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

pub fn send_text_only(tmp: &PathBuf, id: &str, text: &str) {
    let output = oly_cmd(tmp)
        .args(["input", id, "--text", text])
        .output()
        .expect("`oly input --text` failed to execute");
    assert!(
        output.status.success(),
        "`oly input --text` (no enter) exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

pub fn send_key(tmp: &PathBuf, id: &str, key: &str) {
    let output = oly_cmd(tmp)
        .args(["input", id, "--key", key])
        .output()
        .expect("`oly input --key` failed to execute");
    assert!(
        output.status.success(),
        "`oly input --key {key}` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

pub fn fetch_logs(tmp: &PathBuf, id: &str) -> String {
    let output = oly_cmd(tmp)
        .args(["logs", id, "--tail", "200", "--no-truncate"])
        .output()
        .expect("`oly logs` failed to execute");
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn wait_for_log(
    tmp: &PathBuf,
    id: &str,
    predicate: impl Fn(&str) -> bool,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let log = fetch_logs(tmp, id);
        if predicate(&log) {
            return Some(log);
        }
        sleep(Duration::from_millis(250));
    }
    None
}

pub fn fetch_logs_node(tmp: &PathBuf, node: &str, id: &str) -> String {
    let output = oly_cmd(tmp)
        .args(["logs", id, "--node", node, "--tail", "200", "--no-truncate"])
        .output()
        .expect("`oly logs --node` failed to execute");
    String::from_utf8_lossy(&output.stdout).to_string()
}

pub fn wait_for_log_node(
    tmp: &PathBuf,
    node: &str,
    id: &str,
    predicate: impl Fn(&str) -> bool,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let log = fetch_logs_node(tmp, node, id);
        if predicate(&log) {
            return Some(log);
        }
        sleep(Duration::from_millis(250));
    }
    None
}

pub async fn wait_for_node_connected(port: u16, node: &str, timeout_secs: u64) -> bool {
    use std::time::Duration;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{port}/api/nodes")).await
            && resp.status().is_success()
            && let Ok(body) = resp.text().await
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
            && value.as_array().is_some_and(|arr| {
                arr.iter().any(|entry| {
                    entry.get("name").and_then(|v| v.as_str()) == Some(node)
                        && entry.get("connected").and_then(|v| v.as_bool()) == Some(true)
                })
            })
        {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn wait_for_no_nodes(port: u16, timeout_secs: u64) -> bool {
    use std::time::Duration;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }

        if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{port}/api/nodes")).await
            && resp.status().is_success()
            && let Ok(body) = resp.text().await
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&body)
            && value.as_array().is_some_and(|arr| arr.is_empty())
        {
            return true;
        }

        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
