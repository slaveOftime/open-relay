//! End-to-end integration tests for the `oly attach` and `oly input` features.
//!
//! Test sequence for each scenario:
//!   1. Start the oly daemon  (`oly daemon start --foreground-internal --no-auth --no-http`).
//!   2. Start an interactive shell session (`oly start --detach <shell>`).
//!   3. Wait for the shell's initial output to appear in `oly logs`.
//!   4. Send commands with `oly input --text … --key enter`.
//!   5. Poll `oly logs` until the expected output appears (or timeout).
//!
//! ## Named-pipe isolation on Windows
//!
//! The oly IPC socket on Windows is a named pipe with a fixed name
//! (`open-relay.oly.sock`), which is a global OS resource.  All tests in this
//! binary are serialised via `E2E_LOCK` so that no two daemons are started
//! concurrently.  The `cli_errors` test binary never starts a daemon, so there
//! is no cross-binary conflict.
//!
//! ## Running
//!
//! ```text
//! cargo test --test e2e_daemon
//! ```

use std::{
    env, fs,
    net::TcpListener,
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
    thread::sleep,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

// ---------------------------------------------------------------------------
// Global e2e serialiser
// ---------------------------------------------------------------------------

/// Ensures no two tests in this binary race over the global named-pipe name
/// (Windows) or over shared PTY resources.
static E2E_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Helpers – binary / environment
// ---------------------------------------------------------------------------

fn oly_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oly"))
}

fn oly_cmd(tmp_dir: &PathBuf) -> Command {
    let mut cmd = Command::new(oly_bin());
    apply_state_env(&mut cmd, tmp_dir);
    cmd
}

fn apply_state_env(cmd: &mut Command, tmp_dir: &PathBuf) {
    #[cfg(target_os = "windows")]
    cmd.env("LOCALAPPDATA", tmp_dir);
    #[cfg(target_os = "linux")]
    cmd.env("XDG_STATE_HOME", tmp_dir);
    #[cfg(target_os = "macos")]
    cmd.env("HOME", tmp_dir);
    cmd.env("OLY_SOCKET_NAME", socket_name_for_tmp(tmp_dir));
}

fn socket_name_for_tmp(tmp_dir: &PathBuf) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tmp_dir.to_string_lossy().hash(&mut hasher);
    format!("open-relay.oly.{}.sock", hasher.finish())
}

fn make_tmp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("oly_e2e_{name}"));
    // Start with a clean slate so leftover lock/socket files from a previous
    // aborted run don't prevent the daemon from starting.
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Returns `true` if `program` can be found on `PATH`.
fn program_exists(program: &str) -> bool {
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

// ---------------------------------------------------------------------------
// Helpers – daemon lifecycle
// ---------------------------------------------------------------------------

/// RAII guard – holds the daemon process and stops it when the test finishes
/// (pass or fail).
///
/// We keep the `Child` so we can force-kill the daemon if the graceful stop
/// request doesn't complete in time.
struct DaemonGuard {
    child: std::process::Child,
    tmp_dir: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        // Ask the daemon to stop gracefully.
        let _ = oly_cmd(&self.tmp_dir)
            .args(["daemon", "stop"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        // Brief pause so the daemon can finish its cleanup before force-kill.
        sleep(Duration::from_millis(400));
        // Force-kill if still alive, then reap to avoid zombie processes.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start the daemon and block until it is accepting IPC connections.
///
/// We bypass `--detach` entirely and instead spawn the daemon with
/// `--foreground-internal --no-auth --no-http`.  This avoids the interactive
/// `--no-auth` confirmation prompt and sidesteps a Windows handle-inheritance
/// bug: with `--detach`, the daemon grandchild process inherits the `stdout`
/// pipe that the test has open, so `wait_with_output()` blocks forever.
///
/// With `--foreground-internal` we keep the `Child` handle ourselves and
/// detect readiness by polling `oly stop <fake-id>`:
///   • while starting  → stderr contains "unavailable"
///   • once ready      → stderr contains "not found"
fn start_daemon(tmp: &PathBuf) -> DaemonGuard {
    // Redirect the daemon's stderr to a log file so we can read it on failure.
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

    // Poll until the daemon responds to IPC requests.
    // The `oly` client wraps ALL RpcResponse::Error payloads into
    // AppError::DaemonUnavailable, so we cannot use `!contains("unavailable")`
    // as the readiness signal — the daemon's "session not found" reply is also
    // formatted as "daemon is unavailable: session not found or failed to stop:
    // zzz9999".  Instead, check whether our probe id appears in the output:
    // a connection-level failure never mentions the fake session id.
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
            // Daemon processed the request — it's up and accepting connections.
            return DaemonGuard {
                child,
                tmp_dir: tmp.clone(),
            };
        }
        if Instant::now() >= deadline {
            let daemon_log = fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "daemon did not become ready within 3 s\n\
                 --- daemon stderr ({}) ---\n{}\n\
                 --- last probe stderr ---\n{}",
                log_path.display(),
                daemon_log,
                probe_stderr,
            );
        }
    }
}

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("read local addr").port()
}

fn start_daemon_http(tmp: &PathBuf, port: u16) -> DaemonGuard {
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
                "daemon did not become ready within 4 s\n\
                 --- daemon stderr ({}) ---\n{}\n\
                 --- last probe stderr ---\n{}",
                log_path.display(),
                daemon_log,
                probe_stderr,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers – session management
// ---------------------------------------------------------------------------

/// `oly start --detach <cmd_and_args>` → returns the 7-char session ID.
fn start_session(tmp: &PathBuf, cmd_and_args: &[&str]) -> String {
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

// ---------------------------------------------------------------------------
// Helpers – input
// ---------------------------------------------------------------------------

/// `oly input <id> --text <text> --key enter` — sends a line of text followed
/// by a carriage return (Enter / newline-to-the-PTY).
fn send_line(tmp: &PathBuf, id: &str, text: &str) {
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

/// `oly input <id> --text <text>` — sends text WITHOUT a trailing newline.
fn send_text_only(tmp: &PathBuf, id: &str, text: &str) {
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

/// `oly input <id> --key <key>` — sends a single named key sequence.
fn send_key(tmp: &PathBuf, id: &str, key: &str) {
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

// ---------------------------------------------------------------------------
// Helpers – log polling
// ---------------------------------------------------------------------------

/// Return the rendered output of `oly logs <id> --tail 200 --no-truncate`.
fn fetch_logs(tmp: &PathBuf, id: &str) -> String {
    let output = oly_cmd(tmp)
        .args(["logs", id, "--tail", "200", "--no-truncate"])
        .output()
        .expect("`oly logs` failed to execute");
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Poll `oly logs` until `predicate(&log_text)` is `true` or `timeout` elapses.
/// Returns the log text that satisfied the predicate, or `None` on timeout.
fn wait_for_log(
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

fn fetch_logs_node(tmp: &PathBuf, node: &str, id: &str) -> String {
    let output = oly_cmd(tmp)
        .args(["logs", id, "--node", node, "--tail", "200", "--no-truncate"])
        .output()
        .expect("`oly logs --node` failed to execute");
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn wait_for_log_node(
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

async fn wait_for_node_connected(port: u16, node: &str, timeout_secs: u64) -> bool {
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

async fn wait_for_no_nodes(port: u16, timeout_secs: u64) -> bool {
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

// ============================================================================
// Test 1: echo via the native shell (cmd.exe on Windows, sh elsewhere)
//
// Verifies: daemon start → session start → oly input (text+enter) → oly logs.
// ============================================================================

#[test]
fn e2e_native_shell_echo_marker_appears_in_logs() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_native_echo");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let shell: &[&str] = &["cmd.exe"];
    #[cfg(not(target_os = "windows"))]
    let shell: &[&str] = &["sh"];

    let id = start_session(&tmp, shell);

    // Wait for the shell to emit its initial prompt into the output log.
    let initial = wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(3),
    );
    assert!(
        initial.is_some(),
        "shell produced no output within 3 s; session = {id}"
    );

    // Send `echo <unique_marker>` followed by Enter.
    const MARKER: &str = "oly_e2e_native_echo_marker";
    send_line(&tmp, &id, &format!("echo {MARKER}"));

    // Poll until the marker text appears in the rendered log.
    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(MARKER),
        Duration::from_secs(3),
    );
    assert!(
        result.is_some(),
        "marker '{MARKER}' did not appear in logs within 3 s.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 2: text sent in two separate `oly input` calls
//
// Verifies that sending text first (without Enter) and then Enter as a
// separate `oly input` invocation still results in the command executing.
// ============================================================================

#[test]
fn e2e_two_separate_input_calls_execute_command() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_two_inputs");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let shell: &[&str] = &["cmd.exe"];
    #[cfg(not(target_os = "windows"))]
    let shell: &[&str] = &["sh"];

    let id = start_session(&tmp, shell);

    wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(3),
    )
    .expect("shell produced no output within 3 s");

    // Call 1: send the text without a newline.
    const MARKER: &str = "oly_e2e_two_inputs_marker";
    send_text_only(&tmp, &id, &format!("echo {MARKER}"));

    // Brief pause to ensure the first IPC call completes before the second.
    sleep(Duration::from_millis(100));

    // Call 2: send Enter as a separate key-only input.
    send_key(&tmp, &id, "enter");

    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(MARKER),
        Duration::from_secs(3),
    );
    assert!(
        result.is_some(),
        "marker '{MARKER}' not found after two separate inputs.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 3: multiple echo commands – ordering is preserved in logs
//
// Verifies that multiple sequential `oly input` calls are executed in order.
// ============================================================================

#[test]
fn e2e_multiple_commands_appear_in_order() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_ordering");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let shell: &[&str] = &["cmd.exe"];
    #[cfg(not(target_os = "windows"))]
    let shell: &[&str] = &["sh"];

    let id = start_session(&tmp, shell);

    wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(3),
    )
    .expect("shell produced no output within 3 s");

    send_line(&tmp, &id, "echo oly_e2e_order_first");
    send_line(&tmp, &id, "echo oly_e2e_order_second");
    send_line(&tmp, &id, "echo oly_e2e_order_third");

    // Wait until the last marker is visible, then check ordering.
    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains("oly_e2e_order_third"),
        Duration::from_secs(3),
    );
    assert!(
        result.is_some(),
        "third marker not found in logs.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );

    let log = fetch_logs(&tmp, &id);
    let pos_first = log.find("oly_e2e_order_first");
    let pos_second = log.find("oly_e2e_order_second");
    let pos_third = log.find("oly_e2e_order_third");

    assert!(
        pos_first.is_some() && pos_second.is_some() && pos_third.is_some(),
        "one or more markers missing.\nLogs:\n{log}"
    );
    assert!(
        pos_first.unwrap() < pos_second.unwrap() && pos_second.unwrap() < pos_third.unwrap(),
        "markers appeared out of order.\nLogs:\n{log}"
    );
}

// ============================================================================
// Test 4: PowerShell interactive session (skipped if pwsh is not on PATH)
//
// Verifies: oly input works with PowerShell 7+ (cross-platform) by sending
// Write-Host and checking the output marker appears in oly logs.
// ============================================================================

#[test]
fn e2e_powershell_write_host_marker_appears_in_logs() {
    if !program_exists("pwsh") {
        // pwsh is not available in this environment; skip gracefully.
        eprintln!("SKIP e2e_powershell_write_host_marker_appears_in_logs: pwsh not found on PATH");
        return;
    }

    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_pwsh");
    let _daemon = start_daemon(&tmp);

    // -NoLogo  suppresses the copyright banner.
    // -NoProfile  avoids running $PROFILE which may produce extra noise.
    let id = start_session(&tmp, &["pwsh", "-NoLogo", "-NoProfile"]);

    // PowerShell startup is slower than sh/cmd; allow up to 15 s.
    let initial = wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(15),
    );
    assert!(
        initial.is_some(),
        "pwsh produced no output within 15 s; session = {id}"
    );

    const MARKER: &str = "oly_e2e_pwsh_write_host_marker";
    // Write-Host outputs directly to the terminal (bypassing the pipeline),
    // which makes it appear in PTY output regardless of pipeline redirection.
    send_line(&tmp, &id, &format!("Write-Host {MARKER}"));

    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(MARKER),
        Duration::from_secs(15),
    );
    assert!(
        result.is_some(),
        "marker '{MARKER}' not found in PowerShell logs after 15 s.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 5: Ctrl+C interrupts a running command (non-Windows only)
//
// Verifies: `oly input --key ctrl+c` sends SIGINT to the child process,
// which interrupts a running `sleep` and returns the shell to its prompt.
// ============================================================================

#[test]
#[cfg(not(target_os = "windows"))]
fn e2e_ctrl_c_interrupts_sleep_and_returns_prompt() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_ctrl_c");
    let _daemon = start_daemon(&tmp);

    let id = start_session(&tmp, &["sh"]);

    wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(3),
    )
    .expect("sh produced no output within 3 s");

    // Start a long-running command.
    send_line(&tmp, &id, "sleep 60");
    sleep(Duration::from_millis(400));

    // Interrupt it with Ctrl-C.
    send_key(&tmp, &id, "ctrl+c");

    // After SIGINT, sh should return to a prompt (another '$' / '#' appears).
    // We count prompt characters: after interrupt there should be at least two
    // (one from before the sleep command, one after it).
    let recovered = wait_for_log(
        &tmp,
        &id,
        |log| {
            log.matches('$').count() >= 2
                || log.contains("^C")
                || log.contains("Interrupt")
                || log.contains("interrupt")
        },
        Duration::from_secs(3),
    );
    assert!(
        recovered.is_some(),
        "sh did not recover from Ctrl-C within 3 s.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 6: Special key sequences arrive correctly (shift+tab, arrow keys)
//
// We use a `cat` / `type` session and send escape sequences; the session
// must echo them back (cat echoes stdin to stdout).  We verify the raw bytes
// arrived at the PTY by checking that *some* output appeared after each key.
//
// Note: cat/type on Windows doesn't exist the same way; skip on Windows.
// ============================================================================

#[test]
#[cfg(not(target_os = "windows"))]
fn e2e_special_keys_reach_pty_without_error() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_special_keys");
    let _daemon = start_daemon(&tmp);

    // Use sh so we can test key sequences without worrying about cat echoing.
    let id = start_session(&tmp, &["sh"]);

    wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(3),
    )
    .expect("sh produced no output within 3 s");

    // Send shift+tab, arrow keys, home, end – all must not cause an IPC error.
    for key in &["shift+tab", "up", "down", "left", "right", "home", "end"] {
        let output = oly_cmd(&tmp)
            .args(["input", &id, "--key", key])
            .output()
            .expect("oly input failed to execute");
        assert!(
            output.status.success(),
            "`oly input --key {key}` returned non-zero.\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Send a verifiable command afterwards to confirm the session is still running.
    const MARKER: &str = "oly_e2e_special_keys_alive";
    send_line(&tmp, &id, &format!("echo {MARKER}"));

    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(MARKER),
        Duration::from_secs(3),
    );
    assert!(
        result.is_some(),
        "session appears dead after special-key inputs.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 7: `oly logs` output is correct – session that has already exited
//
// Starts a non-interactive command that exits immediately, then verifies
// `oly logs` still returns the output after the session has stopped.
// ============================================================================

#[test]
fn e2e_logs_available_after_session_exits() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_logs_after_exit");
    let _daemon = start_daemon(&tmp);

    // Use a command that runs, prints something, then exits.
    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "echo", "oly_e2e_exit_output_marker"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "echo oly_e2e_exit_output_marker"];

    let id = start_session(&tmp, cmd);

    const MARKER: &str = "oly_e2e_exit_output_marker";

    // The session exits quickly; poll until the marker appears.
    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(MARKER),
        Duration::from_secs(3),
    );
    assert!(
        result.is_some(),
        "marker '{MARKER}' not found after session exit.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 8: local streaming attach reports child termination to the terminal
//
// Verifies: `oly start <cmd>` attaches immediately, renders child output, and
// prints `Session <id> has ended.` when the child exits shortly afterwards.
// ============================================================================

#[test]
fn e2e_local_attach_reports_session_end_on_child_exit() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_attach_stream_done");
    let _daemon = start_daemon(&tmp);

    const MARKER: &str = "oly_e2e_attach_stream_done_marker";

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &[
        "cmd.exe",
        "/c",
        &format!("ping 127.0.0.1 -n 2 >nul & echo {MARKER}"),
    ];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", &format!("sleep 1; echo {MARKER}")];

    let output = oly_cmd(&tmp)
        .args(["start", cmd[0], cmd[1], cmd[2]])
        .output()
        .expect("`oly start` failed to execute");

    assert!(
        output.status.success(),
        "`oly start` exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(MARKER),
        "attached output did not contain child marker '{MARKER}'.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Session ") && stdout.contains(" has ended."),
        "attached output did not report child termination.\nstdout:\n{stdout}"
    );
}

// ============================================================================
// Test 9: spawn failure is surfaced as a clear non-zero CLI error
//
// Verifies: when the daemon cannot spawn the requested command, `oly start`
// exits non-zero and returns an actionable error message.
// ============================================================================

#[test]
fn e2e_start_spawn_failure_exits_nonzero_with_clear_error() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_spawn_failure");
    let _daemon = start_daemon(&tmp);

    let output = oly_cmd(&tmp)
        .args(["start", "--detach", "oly_command_that_does_not_exist_12345"])
        .output()
        .expect("`oly start` failed to execute");

    assert!(
        !output.status.success(),
        "`oly start` should exit non-zero for spawn failure"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("spawn") || stderr.contains("not found") || stderr.contains("error"),
        "expected clear spawn-failure message, got: {stderr}"
    );
}

#[test]
fn e2e_start_respects_explicit_cwd() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_start_cwd");
    let cwd = tmp.join("requested-cwd");
    fs::create_dir_all(&cwd).expect("create requested cwd");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "cd"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "pwd"];

    let cwd_str = cwd.display().to_string();
    let mut args = vec!["start", "--detach", "--cwd", cwd_str.as_str()];
    args.extend_from_slice(cmd);

    let output = oly_cmd(&tmp)
        .args(&args)
        .output()
        .expect("`oly start --cwd` failed to execute");
    assert!(
        output.status.success(),
        "`oly start --cwd` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(id.len(), 7, "expected 7-char session ID, got: {id:?}");

    let logged = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(&cwd_str),
        Duration::from_secs(3),
    );
    assert!(
        logged.is_some(),
        "expected logs to contain cwd {cwd_str:?}.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

// ============================================================================
// Test 10: interactive ops fail gracefully after session is evicted from memory
//
// Verifies: with short eviction TTL, completed sessions are evicted and
// `oly input` returns a clear non-zero error rather than hanging/crashing.
// ============================================================================

#[test]
fn e2e_evicted_session_input_fails_gracefully() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_evicted_session");

    fs::create_dir_all(tmp.join("oly")).expect("create state dir");
    fs::write(
        tmp.join("oly").join("config.json"),
        r#"{
  "session_eviction_seconds": 1
}"#,
    )
    .expect("write config override");

    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "echo", "oly_e2e_eviction_marker"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "echo oly_e2e_eviction_marker"];

    let id = start_session(&tmp, cmd);

    let seen = wait_for_log(
        &tmp,
        &id,
        |log| log.contains("oly_e2e_eviction_marker"),
        Duration::from_secs(3),
    );
    assert!(seen.is_some(), "session did not produce expected output");

    // First prune pass records `completed_at` for the finished session.
    let _ = fetch_logs(&tmp, &id);

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut last_stderr = String::new();
    let mut saw_failure = false;

    while Instant::now() < deadline {
        let output = oly_cmd(&tmp)
            .args(["input", &id, "--text", "still_there?"])
            .output()
            .expect("`oly input` failed to execute");

        last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            saw_failure = true;
            break;
        }

        sleep(Duration::from_millis(250));
    }

    assert!(
        saw_failure,
        "`oly input` remained successful past eviction timeout; last stderr: {last_stderr}"
    );
    assert!(
        last_stderr.contains("evicted")
            || last_stderr.contains("not found")
            || last_stderr.contains("error"),
        "expected graceful eviction error message, got: {last_stderr}"
    );
}

// ============================================================================
// Test 11: federation API key + join handshake behavior on primary
//
// Verifies:
//   1) `oly api-key add` prints a 64-hex key.
//   2) `oly api-key ls` contains the created key name.
//   3) `/api/nodes/join` accepts valid key + unique name.
//   4) Duplicate node name is rejected.
//   5) Same key can be reused for a different node name.
//   6) `oly api-key remove` causes subsequent joins with that key to fail.
// ============================================================================

#[test]
fn e2e_federation_api_keys_and_join_handshake() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_federation_join");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);

    let add = oly_cmd(&tmp)
        .args(["api-key", "add", "mykey"])
        .output()
        .expect("`oly api-key add` failed to execute");
    assert!(
        add.status.success(),
        "`oly api-key add` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let add_stdout = String::from_utf8_lossy(&add.stdout).to_string();
    let key = add_stdout
        .lines()
        .last()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    assert_eq!(key.len(), 64, "expected 64-char API key, got: {key}");
    assert!(
        key.chars().all(|ch| ch.is_ascii_hexdigit()),
        "expected hex API key, got: {key}"
    );

    let list = oly_cmd(&tmp)
        .args(["api-key", "ls"])
        .output()
        .expect("`oly api-key ls` failed to execute");
    assert!(
        list.status.success(),
        "`oly api-key ls` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("mykey"),
        "expected key name in list output, got:\n{list_stdout}"
    );

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let ws_url = format!("ws://127.0.0.1:{port}/api/nodes/join");

        let (mut ws1, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect worker1 websocket");
        ws1.send(WsMessage::Text(
            json!({"type": "join", "name": "worker1", "key": key})
                .to_string()
                .into(),
        ))
        .await
        .expect("send worker1 join message");
        let first = timeout(Duration::from_secs(2), ws1.next())
            .await
            .expect("timed out waiting for worker1 join response")
            .expect("worker1 websocket closed")
            .expect("worker1 websocket read failed");
        let first_text = match first {
            WsMessage::Text(text) => text,
            other => panic!("unexpected worker1 response frame: {other:?}"),
        };
        let first_json: serde_json::Value =
            serde_json::from_str(&first_text).expect("parse worker1 join response");
        assert_eq!(
            first_json.get("type").and_then(|v| v.as_str()),
            Some("joined"),
            "expected joined for worker1, got: {first_json}"
        );

        let nodes_resp = reqwest::get(format!("http://127.0.0.1:{port}/api/nodes"))
            .await
            .expect("GET /api/nodes failed");
        assert!(
            nodes_resp.status().is_success(),
            "GET /api/nodes was not 2xx"
        );
        let nodes_body = nodes_resp.text().await.expect("read /api/nodes body");
        let nodes_json: serde_json::Value =
            serde_json::from_str(&nodes_body).expect("parse /api/nodes");
        let has_worker1 = nodes_json.as_array().is_some_and(|arr| {
            arr.iter().any(|item| {
                item.get("name").and_then(|v| v.as_str()) == Some("worker1")
                    && item.get("connected").and_then(|v| v.as_bool()) == Some(true)
            })
        });
        assert!(
            has_worker1,
            "expected worker1 in /api/nodes, got: {nodes_json}"
        );

        let (mut ws_dup, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect duplicate websocket");
        ws_dup
            .send(WsMessage::Text(
                json!({"type": "join", "name": "worker1", "key": key})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send duplicate join message");
        let dup = timeout(Duration::from_secs(2), ws_dup.next())
            .await
            .expect("timed out waiting for duplicate join response")
            .expect("duplicate websocket closed")
            .expect("duplicate websocket read failed");
        let dup_text = match dup {
            WsMessage::Text(text) => text,
            other => panic!("unexpected duplicate response frame: {other:?}"),
        };
        let dup_json: serde_json::Value =
            serde_json::from_str(&dup_text).expect("parse duplicate join response");
        assert_eq!(
            dup_json.get("type").and_then(|v| v.as_str()),
            Some("error"),
            "expected error for duplicate node name, got: {dup_json}"
        );
        let dup_msg = dup_json
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            dup_msg.contains("already connected"),
            "expected duplicate-name rejection message, got: {dup_msg}"
        );

        let (mut ws2, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect worker2 websocket");
        ws2.send(WsMessage::Text(
            json!({"type": "join", "name": "worker2", "key": key})
                .to_string()
                .into(),
        ))
        .await
        .expect("send worker2 join message");
        let second = timeout(Duration::from_secs(2), ws2.next())
            .await
            .expect("timed out waiting for worker2 join response")
            .expect("worker2 websocket closed")
            .expect("worker2 websocket read failed");
        let second_text = match second {
            WsMessage::Text(text) => text,
            other => panic!("unexpected worker2 response frame: {other:?}"),
        };
        let second_json: serde_json::Value =
            serde_json::from_str(&second_text).expect("parse worker2 join response");
        assert_eq!(
            second_json.get("type").and_then(|v| v.as_str()),
            Some("joined"),
            "expected joined for worker2 key reuse, got: {second_json}"
        );

        let _ = ws1.close(None).await;
        let _ = ws2.close(None).await;
    });

    let remove = oly_cmd(&tmp)
        .args(["api-key", "remove", "mykey"])
        .output()
        .expect("`oly api-key remove` failed to execute");
    assert!(
        remove.status.success(),
        "`oly api-key remove` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&remove.stderr)
    );

    rt.block_on(async {
        let ws_url = format!("ws://127.0.0.1:{port}/api/nodes/join");
        let (mut ws3, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect worker3 websocket");
        ws3.send(WsMessage::Text(
            json!({"type": "join", "name": "worker3", "key": key})
                .to_string()
                .into(),
        ))
        .await
        .expect("send worker3 join message");

        let third = timeout(Duration::from_secs(2), ws3.next())
            .await
            .expect("timed out waiting for worker3 join response")
            .expect("worker3 websocket closed")
            .expect("worker3 websocket read failed");
        let third_text = match third {
            WsMessage::Text(text) => text,
            other => panic!("unexpected worker3 response frame: {other:?}"),
        };
        let third_json: serde_json::Value =
            serde_json::from_str(&third_text).expect("parse worker3 join response");
        assert_eq!(
            third_json.get("type").and_then(|v| v.as_str()),
            Some("error"),
            "expected error for removed key, got: {third_json}"
        );
        let msg = third_json
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            msg.contains("unauthorized"),
            "expected unauthorized after key removal, got: {msg}"
        );
    });
}

#[test]
fn e2e_federation_primary_secondary_full_lifecycle() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_lifecycle_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_lifecycle_secondary");
    let port = pick_free_port();

    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    let add = oly_cmd(&primary_tmp)
        .args(["api-key", "add", "fedkey"])
        .output()
        .expect("`oly api-key add` failed to execute");
    assert!(
        add.status.success(),
        "`oly api-key add` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let key = String::from_utf8_lossy(&add.stdout)
        .lines()
        .last()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    assert_eq!(key.len(), 64, "expected 64-char key, got: {key}");

    let join = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--key",
            &key,
            &format!("http://127.0.0.1:{port}"),
        ])
        .output()
        .expect("`oly join start` failed to execute");
    assert!(
        join.status.success(),
        "`oly join start` exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&join.stdout),
        String::from_utf8_lossy(&join.stderr)
    );

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    let connected = rt.block_on(wait_for_node_connected(port, "worker1", 10));
    assert!(
        connected,
        "worker1 did not appear in /api/nodes after join start"
    );

    #[cfg(target_os = "windows")]
    let remote_shell: &[&str] = &["cmd.exe"];
    #[cfg(not(target_os = "windows"))]
    let remote_shell: &[&str] = &["sh"];

    let mut start_args = vec!["start", "--detach", "--node", "worker1"];
    start_args.extend_from_slice(remote_shell);
    let start = oly_cmd(&primary_tmp)
        .args(&start_args)
        .output()
        .expect("`oly start --node` failed to execute");
    assert!(
        start.status.success(),
        "`oly start --node` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let session_id = String::from_utf8_lossy(&start.stdout).trim().to_string();
    assert_eq!(
        session_id.len(),
        7,
        "expected 7-char remote session ID, got: {session_id:?}"
    );

    let remote_ls = oly_cmd(&primary_tmp)
        .args(["ls", "--node", "worker1"])
        .output()
        .expect("`oly ls --node` failed to execute");
    assert!(
        remote_ls.status.success(),
        "`oly ls --node` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&remote_ls.stderr)
    );
    let remote_ls_out = String::from_utf8_lossy(&remote_ls.stdout);
    assert!(
        remote_ls_out.contains(&session_id),
        "remote list did not contain session id.\nOutput:\n{remote_ls_out}"
    );

    let local_ls = oly_cmd(&primary_tmp)
        .args(["ls"])
        .output()
        .expect("`oly ls` failed to execute");
    assert!(
        local_ls.status.success(),
        "`oly ls` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&local_ls.stderr)
    );
    let local_ls_out = String::from_utf8_lossy(&local_ls.stdout);
    assert!(
        !local_ls_out.contains(&session_id),
        "local list unexpectedly included remote session id.\nOutput:\n{local_ls_out}"
    );

    const REMOTE_MARKER: &str = "oly_federation_remote_marker";
    let input = oly_cmd(&primary_tmp)
        .args([
            "input",
            &session_id,
            "--node",
            "worker1",
            "--text",
            &format!("echo {REMOTE_MARKER}"),
            "--key",
            "enter",
        ])
        .output()
        .expect("`oly input --node` failed to execute");
    assert!(
        input.status.success(),
        "`oly input --node` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&input.stderr)
    );

    let logs_seen = wait_for_log_node(
        &primary_tmp,
        "worker1",
        &session_id,
        |log| log.contains(REMOTE_MARKER),
        Duration::from_secs(5),
    );
    assert!(
        logs_seen.is_some(),
        "remote marker not found in node-proxied logs.\nLogs:\n{}",
        fetch_logs_node(&primary_tmp, "worker1", &session_id)
    );

    let stop = oly_cmd(&primary_tmp)
        .args(["stop", &session_id, "--node", "worker1"])
        .output()
        .expect("`oly stop --node` failed to execute");
    assert!(
        stop.status.success(),
        "`oly stop --node` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let attach = oly_cmd(&primary_tmp)
        .args(["attach", &session_id, "--node", "worker1"])
        .output()
        .expect("`oly attach --node` failed to execute");
    assert!(
        attach.status.success(),
        "`oly attach --node` exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&attach.stdout),
        String::from_utf8_lossy(&attach.stderr)
    );

    drop(secondary);
    let no_nodes = rt.block_on(wait_for_no_nodes(port, 10));
    assert!(
        no_nodes,
        "node list did not become empty after secondary shutdown"
    );

    let secondary_restart = start_daemon(&secondary_tmp);
    let reconnected = rt.block_on(wait_for_node_connected(port, "worker1", 10));
    assert!(
        reconnected,
        "worker1 did not auto-reconnect after secondary daemon restart"
    );

    let join_stop = oly_cmd(&secondary_tmp)
        .args(["join", "stop", "--name", "worker1"])
        .output()
        .expect("`oly join stop` failed to execute");
    assert!(
        join_stop.status.success(),
        "`oly join stop` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&join_stop.stderr)
    );

    let no_nodes_after_join_stop = rt.block_on(wait_for_no_nodes(port, 10));
    assert!(
        no_nodes_after_join_stop,
        "node list did not become empty after join stop"
    );

    let join_ls = oly_cmd(&secondary_tmp)
        .args(["join", "ls"])
        .output()
        .expect("`oly join ls` failed to execute");
    assert!(
        join_ls.status.success(),
        "`oly join ls` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&join_ls.stderr)
    );
    let join_ls_out = String::from_utf8_lossy(&join_ls.stdout);
    assert!(
        join_ls_out.contains("No active joins."),
        "expected join config removal after join stop.\nOutput:\n{join_ls_out}"
    );

    drop(secondary_restart);
}
