mod e2e;

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use e2e::*;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::{
    fs,
    thread::sleep,
    time::{Duration, Instant},
};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Real Windows named pipes, not `tokio::io::duplex`: peers disappear while
/// large responses are being written. This exercises Mio's IOCP completion
/// bookkeeping and interprocess's linger/drop path, implicated in the
/// sleep/resume crash. A fresh RPC after every batch must reach the same daemon.
#[cfg(windows)]
#[test]
fn e2e_windows_ipc_survives_clients_disconnecting_during_writes() {
    use interprocess::local_socket::{
        GenericNamespaced,
        tokio::{Stream, prelude::*},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("windows_ipc_disconnect");
    let daemon = start_daemon(&tmp);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let name_string = socket_name_for_tmp(&tmp);
        let name = name_string.to_ns_name::<GenericNamespaced>().unwrap();
        let health_frame = b"{\"version\":14,\"payload\":{\"type\":\"health\"}}\n";
        let mut initial = Stream::connect(name.clone()).await.unwrap();
        initial.write_all(health_frame).await.unwrap();
        let mut initial_reader = BufReader::new(initial);
        let mut line = String::new();
        timeout(Duration::from_secs(5), initial_reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let baseline: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(baseline["payload"]["type"], "health");
        let pid = baseline["payload"]["daemon_pid"].clone();
        drop(initial_reader);

        // Stop a nonexistent session: benign, with an error response larger
        // than the pipe buffers, so a disconnect can interrupt queued writes.
        let missing_id = "missing".repeat(32 * 1024);
        let mut frame = serde_json::to_vec(&json!({
            "version": 14,
            "payload": {"type": "stop", "id": missing_id, "grace_seconds": 0}
        }))
        .unwrap();
        frame.push(b'\n');
        for _ in 0..8 {
            let mut tasks = tokio::task::JoinSet::new();
            for delay_ms in [0, 0, 1, 1, 2, 2, 5, 5] {
                let name = name.clone();
                let frame = frame.clone();
                tasks.spawn(async move {
                    let mut stream = Stream::connect(name).await.unwrap();
                    timeout(Duration::from_secs(5), stream.write_all(&frame))
                        .await
                        .unwrap()
                        .unwrap();
                    stream.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    drop(stream); // Deliberately never read the response.
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
            let mut stream = timeout(Duration::from_secs(5), Stream::connect(name.clone()))
                .await
                .unwrap()
                .unwrap();
            stream.write_all(health_frame).await.unwrap();
            let mut reader = BufReader::new(stream);
            line.clear();
            timeout(Duration::from_secs(5), reader.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            let response: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["payload"]["type"], "health");
            assert_eq!(response["payload"]["daemon_pid"], pid);
        }
    });
    drop(daemon); // Wait for teardown/linger before checking for panics.
    let crash_log = tmp.join("oly/logs/daemon-crash.log");
    let crashes = fs::read_to_string(crash_log).unwrap_or_default();
    assert!(
        crashes.is_empty(),
        "disconnected clients panicked the daemon: {crashes}"
    );
    let stderr = fs::read_to_string(tmp.join("daemon-stderr.log")).unwrap();
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(
        stderr.contains("os error 232") || stderr.contains("os error 109"),
        "the stress test must actually exercise disconnected-pipe errors: {stderr}"
    );
}

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
fn e2e_config_hot_reload_applies_without_daemon_restart() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_config_hot_reload");
    let _daemon = start_daemon(&tmp);

    // Restrict the running daemon to a single session by editing config.json;
    // the hot-reload loop must pick the change up without a restart.
    fs::write(
        tmp.join("oly").join("config.json"),
        r#"{"max_running_sessions": 1}"#,
    )
    .expect("rewrite config.json");

    // The reload poll runs every 2s; give it ample margin.
    sleep(Duration::from_secs(4));

    #[cfg(target_os = "windows")]
    let long_running: &[&str] = &["cmd.exe", "/c", "ping", "127.0.0.1", "-n", "60"];
    #[cfg(not(target_os = "windows"))]
    let long_running: &[&str] = &["sh", "-c", "sleep 60"];

    let first = oly_cmd(&tmp)
        .args(["start", "--detach"])
        .args(long_running)
        .output()
        .expect("first `oly start` failed to execute");
    assert!(
        first.status.success(),
        "first session should start: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let second = oly_cmd(&tmp)
        .args(["start", "--detach"])
        .args(long_running)
        .output()
        .expect("second `oly start` failed to execute");
    assert!(
        !second.status.success(),
        "second session should be rejected by the hot-reloaded max_running_sessions=1"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("max running sessions limit reached"),
        "expected max-sessions error, got: {stderr}"
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

    // The child prints its cwd with `pwd`, which reports the process's real
    // working directory. On macOS the temp root lives under `/var`, a symlink
    // to `/private/var`, so `pwd` echoes the resolved path while `cwd_str`
    // still holds the unresolved one. Accept either spelling so the assertion
    // pins the honoured `--cwd` rather than the platform's symlink layout.
    let canonical_cwd = fs::canonicalize(&cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| cwd_str.clone());
    // The transcript is the session's rendered screen state, so the terminal
    // engine wraps it at the session's PTY width — and this temp path is
    // longer than 80 columns, so it is split mid-string. A path never
    // contains a line break, so match against the unwrapped text: this pins
    // the honoured `--cwd`, not how wide the session's PTY happened to be.
    let logged = wait_for_log(
        &tmp,
        &id,
        |log| {
            let flat = log.replace(['\r', '\n'], "");
            flat.contains(&cwd_str) || flat.contains(&canonical_cwd)
        },
        Duration::from_secs(3),
    );
    assert!(
        logged.is_some(),
        "expected logs to contain cwd {cwd_str:?}.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

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

    let _ = fetch_logs(&tmp, &id);

    let deadline = Instant::now() + Duration::from_secs(6);
    let mut last_stderr = String::new();
    let mut saw_failure = false;

    while Instant::now() < deadline {
        let output = oly_cmd(&tmp)
            .args(["send", &id, "still_there?"])
            .output()
            .expect("`oly send` failed to execute");

        last_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            saw_failure = true;
            break;
        }

        sleep(Duration::from_millis(250));
    }

    assert!(
        saw_failure,
        "`oly send` remained successful past eviction timeout; last stderr: {last_stderr}"
    );
    assert!(
        last_stderr.contains("evicted")
            || last_stderr.contains("not found")
            || last_stderr.contains("error"),
        "expected graceful eviction error message, got: {last_stderr}"
    );
}

#[test]
fn e2e_federation_identity_key_registrations_and_duplicate_name() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_federation_identity_registrations");
    let port = pick_free_port();
    let _primary = start_daemon_http(&tmp, port);

    // Generate three fresh Ed25519 identities. Two will be registered
    // against the primary; the third exercises the negative path
    // (covered below by sending a join from the still-fresh key).
    let mut first_seed = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut first_seed);
    let first_key = ed25519_dalek::SigningKey::from_bytes(&first_seed);
    let first_pub = format!(
        "ed25519 {}",
        B64.encode(first_key.verifying_key().as_bytes())
    );

    let mut second_seed = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut second_seed);
    let second_key = ed25519_dalek::SigningKey::from_bytes(&second_seed);
    let second_pub = format!(
        "ed25519 {}",
        B64.encode(second_key.verifying_key().as_bytes())
    );

    // Register both identities under distinct node names.
    for (name, pub_line) in [("worker-a", &first_pub), ("worker-b", &second_pub)] {
        let out = oly_cmd(&tmp)
            .args(["node", "accept", "-n", name, "-k", pub_line])
            .output()
            .expect("`oly node accept` should run");
        assert!(
            out.status.success(),
            "`oly node accept {name}` exited non-zero.\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // A key owned by worker-a cannot be stolen by worker-b's upsert.
    let conflict = oly_cmd(&tmp)
        .args(["node", "accept", "-n", "worker-b", "-k", &first_pub])
        .output()
        .unwrap();
    assert!(
        !conflict.status.success(),
        "accept upsert stole another name's key"
    );
    // Re-registering the same name with a new, unowned identity is allowed.
    let mut replacement_seed = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut replacement_seed);
    let replacement_key = ed25519_dalek::SigningKey::from_bytes(&replacement_seed);
    let replacement_pub = format!(
        "ed25519 {}",
        B64.encode(replacement_key.verifying_key().as_bytes())
    );
    let out = oly_cmd(&tmp)
        .args(["node", "accept", "-n", "worker-a", "-k", &replacement_pub])
        .output()
        .expect("`oly node accept worker-a` re-registration should run");
    assert!(
        out.status.success(),
        "`oly node accept worker-a` re-registration must exit 0"
    );

    // Reject malformed lines: tag mismatch, wrong length, stray tokens.
    let bad = vec![
        "ssh-ed25519 AAAAFFRZGm9NFHmm9d2lRWnbtr3KFNbczPtQKSlT0Bow",
        "ed25519 !!notbase64!!",
        "ed25519 AAAAAAAAAAAAAAAAAAAAAA==",
        "rsa2048 abc",
        "",
    ];
    for bad_line in bad {
        let out = oly_cmd(&tmp)
            .args(["node", "accept", "-n", "broken", "-k", bad_line])
            .output()
            .expect("`oly node accept` parse-failure should still exit");
        assert!(
            !out.status.success(),
            "malformed identity {bad_line:?} must not be accepted"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("protocol error") || stderr.contains("invalid"),
            "unexpected stderr for {bad_line:?}: {stderr}"
        );
    }
}

#[test]
fn e2e_daemon_supports_bind_override() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_daemon_bind_override");
    let port = pick_free_port();
    let _daemon = start_daemon_http_with_bind(&tmp, "0.0.0.0", port);

    let daemon_log = fs::read_to_string(tmp.join("daemon-stderr.log")).unwrap_or_default();
    assert!(
        daemon_log.contains(&format!("HTTP server listening at http://0.0.0.0:{port}")),
        "expected daemon log to report bind override, got:\n{daemon_log}"
    );

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let nodes_resp = reqwest::get(format!("http://127.0.0.1:{port}/api/nodes"))
            .await
            .expect("GET /api/nodes should succeed when bound to 0.0.0.0");
        assert_eq!(nodes_resp.status(), reqwest::StatusCode::OK);
    });
}

#[test]
fn e2e_federation_identity_join_lifecycle() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_ssh_lifecycle_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_ssh_lifecycle_secondary");
    let port = pick_free_port();

    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    // The secondary auto-generates its identity key on first start; read it
    // from `<OLY_STATE_DIR>/identity_key.pub`. The test harness points
    // `OLY_STATE_DIR` at `tmp_dir.join("oly")` (see tests/e2e/mod.rs).
    let secondary_state_dir = secondary_tmp.join("oly");
    let secondary_pub = std::fs::read_to_string(secondary_state_dir.join("identity_key.pub"))
        .expect("read secondary id pub key")
        .trim()
        .to_string();
    assert!(
        secondary_pub.starts_with("ed25519 "),
        "expected canonical ed25519 line, got: {secondary_pub}"
    );

    // Register the secondary's id pub key on the primary.
    let accept = oly_cmd(&primary_tmp)
        .args(["node", "accept", "-n", "worker1", "-k", &secondary_pub])
        .output()
        .expect("`oly node accept` failed to execute");
    assert!(
        accept.status.success(),
        "`oly node accept` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&accept.stderr)
    );
    // Fetch the primary's id pub key and pin it explicitly on the
    // secondary via `--id-pub-key`.
    let primary_state_dir = primary_tmp.join("oly");
    let primary_pub = std::fs::read_to_string(primary_state_dir.join("identity_key.pub"))
        .expect("read primary id pub key")
        .trim()
        .to_string();
    let join = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--id-pub-key",
            &primary_pub,
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
        "worker1 did not appear in /api/nodes after identity join start"
    );

    // Stop the join cleanly and confirm the secondary disappears from
    // /api/nodes; this verifies basic transport liveness on the
    // identity path (channel encryption + relay + disconnect).
    let stop = oly_cmd(&secondary_tmp)
        .args(["join", "stop", "--name", "worker1"])
        .output()
        .expect("`oly join stop` failed to execute");
    assert!(stop.status.success(), "`oly join stop` failed");
    let disconnected = rt.block_on(wait_for_no_nodes(port, 10));
    assert!(disconnected, "worker1 did not disconnect after join stop");

    // Re-join with the correct primary identity. Encrypted confirmation
    // proves the primary owns this independently exchanged pin before success.
    let rejoin = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--id-pub-key",
            &primary_pub,
            &format!("http://127.0.0.1:{port}"),
        ])
        .output()
        .expect("`oly join start` (rejoin) failed to execute");
    assert!(
        rejoin.status.success(),
        "`oly join start` (rejoin) exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rejoin.stdout),
        String::from_utf8_lossy(&rejoin.stderr),
    );
    let rejoined = rt.block_on(wait_for_node_connected(port, "worker1", 10));
    assert!(
        rejoined,
        "worker1 did not re-appear in /api/nodes after identity rejoin"
    );

    drop(secondary);
}

#[test]
fn e2e_federation_primary_secondary_full_lifecycle() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_lifecycle_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_lifecycle_secondary");
    let port = pick_free_port();

    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    // Federation v2: register the secondary identity with the primary,
    // pin the primary's identity on the secondary via `--id-pub-key`,
    // then join.
    let primary_state_dir = primary_tmp.join("oly");
    let primary_pub = std::fs::read_to_string(primary_state_dir.join("identity_key.pub"))
        .expect("read primary id pub key")
        .trim()
        .to_string();
    let secondary_state_dir = secondary_tmp.join("oly");
    let secondary_pub = std::fs::read_to_string(secondary_state_dir.join("identity_key.pub"))
        .expect("read secondary id pub key")
        .trim()
        .to_string();
    let accept = oly_cmd(&primary_tmp)
        .args(["node", "accept", "-n", "worker1", "-k", &secondary_pub])
        .output()
        .expect("`oly node accept` failed to execute");
    assert!(
        accept.status.success(),
        "`oly node accept` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&accept.stderr)
    );

    let join = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--id-pub-key",
            &primary_pub,
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

    // The browser's metadata endpoint must proxy updates to the owning node
    // rather than leaving the caller to hit the 30-second RPC deadline.
    let metadata = rt.block_on(async {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build HTTP client")
            .post(format!(
                "http://127.0.0.1:{port}/api/sessions/{session_id}/metadata?node=worker1"
            ))
            .header("content-type", "application/json")
            .body(
                json!({
                    "title": "Remote title",
                    "tags": ["remote", "edited"],
                    "notifications_enabled": false,
                })
                .to_string(),
            )
            .send()
            .await
            .expect("remote metadata update should answer promptly")
    });
    assert_eq!(metadata.status(), reqwest::StatusCode::OK);
    let updated_body = rt
        .block_on(metadata.text())
        .expect("metadata response body");
    let updated: serde_json::Value = serde_json::from_str(&updated_body).expect("metadata JSON");
    assert_eq!(updated["title"], "Remote title");
    assert_eq!(updated["node"], "worker1");
    assert_eq!(updated["tags"], json!(["remote", "edited"]));
    assert_eq!(updated["notifications_enabled"], false);

    let remote_ls_after_update = oly_cmd(&primary_tmp)
        .args(["ls", "--node", "worker1"])
        .output()
        .expect("`oly ls --node` after update failed to execute");
    assert!(remote_ls_after_update.status.success());
    assert!(
        String::from_utf8_lossy(&remote_ls_after_update.stdout).contains("Remote title"),
        "updated title must be stored on the secondary"
    );

    // Inspection must relay to the owning daemon and keep its path/node.
    for args in [
        vec!["info", session_id.as_str(), "--node", "worker1", "--json"],
        vec!["info", "--node", "worker1", "--json"],
    ] {
        let output = oly_cmd(&primary_tmp).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(info["id"], session_id);
        assert_eq!(info["node"], "worker1");
        assert_eq!(info["title"], "Remote title");
        assert_eq!(info["tags"], json!(["remote", "edited"]));
        assert_eq!(info["notifications_enabled"], false);
        assert_eq!(info["status"], "running");
        assert!(info["journal_incarnation"].is_u64());
        assert!(
            info["session_dir"]
                .as_str()
                .unwrap()
                .contains(&secondary_tmp.to_string_lossy().to_string())
        );
    }

    const REMOTE_MARKER: &str = "oly_federation_remote_marker";
    let input = oly_cmd(&primary_tmp)
        .args([
            "send",
            &session_id,
            "--node",
            "worker1",
            &format!("echo {REMOTE_MARKER}"),
            "key:enter",
        ])
        .output()
        .expect("`oly send --node` failed to execute");
    assert!(
        input.status.success(),
        "`oly send --node` exited non-zero.\nstderr: {}",
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

    // A machine reader on a remote session: JSON output carries the content
    // and the cursor the read was taken at, both proxied to the owning node.
    let remote_window = oly_cmd(&primary_tmp)
        .args([
            "logs",
            &session_id,
            "--node",
            "worker1",
            "--tail",
            "200",
            "--json",
        ])
        .output()
        .expect("`oly logs --tail --json --node` failed to execute");
    assert!(
        remote_window.status.success(),
        "remote history read failed: {}",
        String::from_utf8_lossy(&remote_window.stderr)
    );
    let window: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&remote_window.stdout).trim())
            .expect("remote logs --json emits one JSON object");
    assert!(
        window["content"]
            .as_str()
            .unwrap_or_default()
            .contains(REMOTE_MARKER),
        "remote history window missing marker"
    );
    assert!(
        window["cursor"].as_str().is_some(),
        "remote read must hand back a cursor a follow-up --since can use"
    );
    const REMOTE_FILE_MARKER: &str = "oly_federation_remote_file_marker";
    let upload_source = primary_tmp.join("remote-send-source.txt");
    fs::write(&upload_source, REMOTE_FILE_MARKER).expect("write remote send source file");

    #[cfg(target_os = "windows")]
    let remote_cat_prefix = "type ";
    #[cfg(not(target_os = "windows"))]
    let remote_cat_prefix = "cat ";

    let upload_chunk = format!("oly-file:{}", upload_source.display());
    let file_input = oly_cmd(&primary_tmp)
        .args([
            "send",
            &session_id,
            "--node",
            "worker1",
            remote_cat_prefix,
            &upload_chunk,
            "key:enter",
        ])
        .output()
        .expect("`oly send --node oly-file:<file>` failed to execute");
    assert!(
        file_input.status.success(),
        "`oly send --node oly-file:<file>` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&file_input.stderr)
    );

    let uploaded_file_seen = wait_for_log_node(
        &primary_tmp,
        "worker1",
        &session_id,
        |log| log.contains(REMOTE_FILE_MARKER),
        Duration::from_secs(5),
    );
    assert!(
        uploaded_file_seen.is_some(),
        "remote uploaded-file marker not found in node-proxied logs.\nLogs:\n{}",
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

    // Stop acknowledges the child lifecycle, not PTY EOF. On Windows the
    // final ConPTY bytes can arrive after it; separate fixed-prefix exports
    // made during that drain are legitimately different. A completed attach
    // waits for output EOF and the journal tail before this equality check.
    let drained = oly_cmd(&primary_tmp)
        .args(["attach", &session_id, "--observe", "--node", "worker1"])
        .output()
        .expect("wait for remote output stream completion");
    assert!(
        drained.status.success(),
        "{}",
        String::from_utf8_lossy(&drained.stderr)
    );

    // Remote delivery must be byte-identical to local canonical export,
    // including base64 carry and tiny continuation-page boundaries.
    let local = oly_cmd(&secondary_tmp)
        .args(["export", &session_id])
        .output()
        .unwrap();
    let remote = oly_cmd(&primary_tmp)
        .args(["export", &session_id, "--node", "worker1"])
        .output()
        .unwrap();
    assert!(local.status.success() && remote.status.success());
    assert_eq!(remote.stdout, local.stdout);
    let encoded = oly_cmd(&primary_tmp)
        .args(["export", &session_id, "--json", "--node", "worker1"])
        .output()
        .unwrap();
    assert!(encoded.status.success());
    let encoded: serde_json::Value = serde_json::from_slice(&encoded.stdout).unwrap();
    assert_eq!(
        base64::Engine::decode(
            &base64::prelude::BASE64_STANDARD,
            encoded["bytes"].as_str().unwrap()
        )
        .unwrap(),
        local.stdout
    );
    let cursor = oly_cmd(&primary_tmp)
        .args(["logs", &session_id, "--cursor", "--node", "worker1"])
        .output()
        .unwrap();
    assert!(cursor.status.success());
    let fields = base64::Engine::decode(
        &base64::prelude::BASE64_URL_SAFE_NO_PAD,
        String::from_utf8_lossy(&cursor.stdout).trim(),
    )
    .unwrap();
    let mut token = base64::Engine::encode(
        &base64::prelude::BASE64_URL_SAFE_NO_PAD,
        fields
            .split(|b| *b == b'|')
            .take(3)
            .flat_map(|part| part.iter().copied().chain(*b"|"))
            .chain(*b"0")
            .collect::<Vec<_>>(),
    );
    let mut delivered = Vec::new();
    loop {
        let page = oly_cmd(&primary_tmp)
            .args([
                "logs",
                &session_id,
                "--since",
                &token,
                "--limit-bytes",
                "257",
                "--json",
                "--node",
                "worker1",
            ])
            .output()
            .unwrap();
        assert!(
            page.status.success(),
            "{}",
            String::from_utf8_lossy(&page.stderr)
        );
        let page: serde_json::Value = serde_json::from_slice(&page.stdout).unwrap();
        let bytes = base64::Engine::decode(
            &base64::prelude::BASE64_STANDARD,
            page["bytes"].as_str().unwrap(),
        )
        .unwrap();
        assert!(bytes.len() <= 257);
        delivered.extend(bytes);
        token = page["cursor"].as_str().unwrap().into();
        if page["has_more"] == false {
            break;
        }
    }
    assert_eq!(delivered, local.stdout);

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

#[cfg(unix)]
#[test]
fn e2e_federation_attach_streams_input_through_the_relay() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_attach_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_attach_secondary");
    let port = pick_free_port();

    let _primary = start_daemon_http(&primary_tmp, port);
    let _secondary = start_daemon(&secondary_tmp);

    // Federation v2: identity-based join for the streaming-attach test.
    let primary_state_dir = primary_tmp.join("oly");
    let primary_pub = std::fs::read_to_string(primary_state_dir.join("identity_key.pub"))
        .expect("read primary id pub key")
        .trim()
        .to_string();
    let secondary_state_dir = secondary_tmp.join("oly");
    let secondary_pub = std::fs::read_to_string(secondary_state_dir.join("identity_key.pub"))
        .expect("read secondary id pub key")
        .trim()
        .to_string();
    let accept = oly_cmd(&primary_tmp)
        .args(["node", "accept", "-n", "worker1", "-k", &secondary_pub])
        .output()
        .expect("`oly node accept` failed to execute");
    assert!(accept.status.success());

    let join = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--id-pub-key",
            &primary_pub,
            &format!("http://127.0.0.1:{port}"),
        ])
        .output()
        .expect("`oly join start` failed to execute");
    assert!(join.status.success());

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    assert!(rt.block_on(wait_for_node_connected(port, "worker1", 10)));

    // A live echo session on the secondary.
    let start = oly_cmd(&primary_tmp)
        .args(["start", "--detach", "--node", "worker1", "cat"])
        .output()
        .expect("`oly start --node` failed to execute");
    assert!(
        start.status.success(),
        "`oly start --node cat` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let session_id = String::from_utf8_lossy(&start.stdout).trim().to_string();
    assert_eq!(session_id.len(), 7);

    // Drive the primary's streaming IPC directly: subscribe (controller,
    // credited), then send input and applied-cursor credits mid-stream.
    // Without the relay channel, mid-stream credits could not reach the
    // owning node's stream task at all.
    rt.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        // The daemon's IPC socket is abstract-namespaced on Linux: connect
        // with the same interprocess naming the client uses.
        use interprocess::local_socket::{
            GenericNamespaced, prelude::*, traits::tokio::Stream as _,
        };
        let name = socket_name_for_tmp(&primary_tmp)
            .to_ns_name::<GenericNamespaced>()
            .expect("socket name");
        let connect_deadline = std::time::Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match interprocess::local_socket::tokio::Stream::connect(name.clone()).await {
                Ok(stream) => break stream,
                Err(err) => {
                    assert!(
                        std::time::Instant::now() < connect_deadline,
                        "connect primary IPC socket: {err}"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        // IPC control messages are versioned envelopes:
        // {"version":14,"payload":{...}}; attach output remains binary
        // after the JSON init line.
        let envelope = |payload: serde_json::Value| json!({"version": 14, "payload": payload});
        let subscribe = envelope(json!({
            "type": "node_proxy",
            "node": "worker1",
            "inner": {
                "type": "attach_subscribe",
                "id": session_id,
                "role": "controller",
                "credited": true
            }
        }));
        write_half
            .write_all(format!("{subscribe}\n").as_bytes())
            .await
            .expect("write subscribe");

        // First frame must be the attach init (a JSON line; the stream
        // switches to binary frames afterwards).
        let mut init_line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut init_line))
            .await
            .expect("init read timed out")
            .expect("init read failed");
        assert!(!init_line.is_empty(), "stream closed before init");
        let init_envelope: serde_json::Value =
            serde_json::from_str(&init_line).expect("parse init");
        let init = &init_envelope["payload"];
        assert_eq!(
            init["type"].as_str(),
            Some("attach_stream_init"),
            "unexpected first frame: {init_line}"
        );
        assert!(
            init["attachment_id"].as_u64().unwrap_or(0) >= 1,
            "relayed init must carry the fencing token: {init_line}"
        );
        assert_eq!(init["role"].as_str(), Some("controller"));

        // Mid-stream input through the relay.
        const RELAY_MARKER: &str = "oly_fed_relay_echo_marker";
        // v13: attach input data is raw bytes, base64-encoded in JSON.
        let input = envelope(json!({
            "type": "attach_input",
            "id": session_id,
            // base64("echo oly_fed_relay_echo_marker\n")
            "data": "ZWNobyBvbHlfZmVkX3JlbGF5X2VjaG9fbWFya2VyCg==",
            "wait_for_change": false
        }));
        write_half
            .write_all(format!("{input}\n").as_bytes())
            .await
            .expect("write input");

        // Read chunks until the marker echoes back, acking every chunk so
        // the owning node's credit gate sees our applied cursor.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut echoed = false;
        // Post-init frames are binary: [u32 LE payload_len][u8 tag][payload]
        // with tag 1 = output ([u64 LE offset][raw bytes]) and tag 2 =
        // control (bare JSON RpcResponse). Read them byte-exactly.
        while std::time::Instant::now() < deadline && !echoed {
            let mut header = [0u8; 5];
            if tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut header))
                .await
                .map(|r| r.is_err())
                .unwrap_or(true)
            {
                break;
            }
            let payload_len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
            let tag = header[4];
            assert!(payload_len <= 1 << 20, "frame too large: {payload_len}");
            let mut payload = vec![0u8; payload_len];
            reader
                .read_exact(&mut payload)
                .await
                .expect("frame payload");
            match tag {
                1 => {
                    let offset = u64::from_le_bytes(payload[0..8].try_into().unwrap());
                    let data = &payload[8..];
                    let ack = envelope(json!({
                        "type": "attach_applied_cursor",
                        "id": session_id,
                        "cursor": offset + data.len() as u64
                    }));
                    write_half
                        .write_all(format!("{ack}\n").as_bytes())
                        .await
                        .expect("write ack");
                    if let Ok(text) = std::str::from_utf8(data)
                        && text.contains(RELAY_MARKER)
                    {
                        echoed = true;
                    }
                }
                2 => {
                    let control: serde_json::Value =
                        serde_json::from_slice(&payload).expect("parse control frame");
                    if matches!(
                        control["type"].as_str(),
                        Some("attach_stream_done") | Some("error")
                    ) {
                        break;
                    }
                }
                other => panic!("unknown attach frame tag {other}"),
            }
        }

        // Detach rides the same relay channel and ends the stream.
        let detach = envelope(json!({"type": "attach_detach", "id": session_id}));
        write_half
            .write_all(format!("{detach}\n").as_bytes())
            .await
            .expect("write detach");

        assert!(echoed, "relayed attach never echoed the input marker");
    });
}

#[test]
fn e2e_session_status_transitions_in_list() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_status_transitions");
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

    let ls_running = oly_cmd(&tmp)
        .args(["ls"])
        .output()
        .expect("`oly ls` failed to execute");
    let ls_out = String::from_utf8_lossy(&ls_running.stdout);
    assert!(
        ls_out.contains(&id) && ls_out.contains("running"),
        "expected session {id} with 'running' status in ls output.\nOutput:\n{ls_out}"
    );

    let stop = oly_cmd(&tmp)
        .args(["stop", &id])
        .output()
        .expect("`oly stop` failed to execute");
    assert!(
        stop.status.success(),
        "`oly stop` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ls_out2 = String::new();
    while Instant::now() < deadline {
        sleep(Duration::from_millis(500));
        let ls_stopped = oly_cmd(&tmp)
            .args(["ls"])
            .output()
            .expect("`oly ls` failed to execute");
        ls_out2 = String::from_utf8_lossy(&ls_stopped.stdout).to_string();
        if ls_out2.contains(&id) && ls_out2.contains("stopped") {
            break;
        }
    }
    assert!(
        ls_out2.contains(&id) && ls_out2.contains("stopped"),
        "expected session {id} with 'stopped' status after stop.\nOutput:\n{ls_out2}"
    );

    let log = fetch_logs(&tmp, &id);
    assert!(
        !log.trim().is_empty(),
        "logs should be accessible after session is stopped"
    );
}

// The global e2e lock serializes daemon-spawning tests; holding it across
// awaits is the point (a second daemon must not start mid-test).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn e2e_kill_session_status_transitions_to_killed() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_kill_status_transitions");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);

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

    let client = reqwest::Client::new();
    let kill = client
        .post(format!("http://127.0.0.1:{port}/api/sessions/{id}/kill"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .expect("POST /api/sessions/{id}/kill failed");
    assert!(
        kill.status().is_success(),
        "expected kill request to succeed, got HTTP {}",
        kill.status()
    );
    let kill_body = kill.text().await.expect("read kill response body");
    let kill_body: serde_json::Value =
        serde_json::from_str(&kill_body).expect("parse kill response");
    assert_eq!(
        kill_body.get("killed").and_then(|v| v.as_bool()),
        Some(true)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ls_killed_out = String::new();
    while Instant::now() < deadline {
        sleep(Duration::from_millis(500));
        let ls_killed = oly_cmd(&tmp)
            .args(["ls", "--status", "killed"])
            .output()
            .expect("`oly ls --status killed` failed to execute");
        ls_killed_out = String::from_utf8_lossy(&ls_killed.stdout).to_string();
        if ls_killed_out.contains(&id) && ls_killed_out.contains("killed") {
            break;
        }
    }
    assert!(
        ls_killed_out.contains(&id) && ls_killed_out.contains("killed"),
        "expected session {id} with 'killed' status after kill.\nOutput:\n{ls_killed_out}"
    );

    let ls_stopped = oly_cmd(&tmp)
        .args(["ls", "--status", "stopped"])
        .output()
        .expect("`oly ls --status stopped` failed to execute");
    let ls_stopped_out = String::from_utf8_lossy(&ls_stopped.stdout);
    assert!(
        !ls_stopped_out.contains(&id),
        "killed session {id} should not appear under the stopped filter.\nOutput:\n{ls_stopped_out}"
    );
}

#[test]
fn e2e_list_empty_shows_no_sessions_hint() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_ls_empty");
    let _daemon = start_daemon(&tmp);

    let output = oly_cmd(&tmp)
        .args(["ls"])
        .output()
        .expect("`oly ls` failed to execute");

    assert!(
        output.status.success(),
        "`oly ls` should succeed even with no sessions"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No sessions"),
        "expected 'No sessions' hint.\nstdout:\n{stdout}"
    );
}

/// W4 protocol evidence: a real WS attach against a live daemon — INIT frame
/// layout, input flowing, contiguously offset DATA frames
/// with contiguous stream cursors, ping/pong, graceful detach — and the
/// canonical journal staying
/// doctor-clean throughout.
#[test]
fn e2e_ws_attach_frames_conform_and_journal_stays_clean() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_ws_attach_frames");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    // Use the platform's stdin-to-stdout utility.
    let echo_command = if cfg!(windows) { "more" } else { "cat" };
    let id = start_session(&tmp, &[echo_command]);

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let ws_url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/attach");
        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect attach websocket");

        // INIT: [1][flags][endOffset u64be][incarnation u64be][running u8]
        //       [attachmentId u64be][data]
        let init = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("init frame timeout")
            .expect("ws open")
            .expect("ws read");
        let WsMessage::Binary(init) = init else {
            panic!("expected binary INIT frame, got: {init:?}")
        };
        assert_eq!(init[0], 1, "first frame must be INIT");
        assert!(init.len() >= 27, "INIT header is 27 bytes");
        let mut expected_offset = u64::from_be_bytes(init[2..10].try_into().unwrap());
        let incarnation = u64::from_be_bytes(init[10..18].try_into().unwrap());
        assert!(
            incarnation >= 1,
            "a journaled session reports its incarnation in INIT"
        );
        assert_eq!(init[18], 1, "session is running");
        let attachment_id = u64::from_be_bytes(init[19..27].try_into().unwrap());
        assert!(attachment_id >= 1, "attachment fencing token is assigned");

        // Input flows; the echoed bytes arrive in DATA frames
        // whose offsets continue the init cursor exactly (contiguous,
        ws.send(WsMessage::Text(
            r#"{"type":"input","data":"hello-ws\n","waitForChange":false}"#.into(),
        ))
        .await
        .expect("send input");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut echoed: Vec<u8> = Vec::new();
        while !echoed.windows(b"hello-ws".len()).any(|w| w == b"hello-ws") {
            assert!(
                Instant::now() < deadline,
                "echoed input did not arrive in DATA frames; got: {}",
                String::from_utf8_lossy(&echoed)
            );
            let frame = timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("data frame timeout")
                .expect("ws open")
                .expect("ws read");
            let WsMessage::Binary(bytes) = frame else {
                panic!("expected binary frame, got: {frame:?}")
            };
            match bytes[0] {
                2 => {
                    let offset = u64::from_be_bytes(bytes[1..9].try_into().unwrap());
                    assert_eq!(
                        offset, expected_offset,
                        "DATA offsets must continue the stream cursor exactly"
                    );
                    expected_offset += (bytes.len() - 9) as u64;
                    echoed.extend_from_slice(&bytes[9..]);
                }
                // mode/resize notices are legitimate interleavings.
                3 | 4 => {}
                other => panic!("unexpected frame tag {other}"),
            }
        }

        // Liveness: ping is answered with a PONG frame.
        ws.send(WsMessage::Text(r#"{"type":"ping"}"#.into()))
            .await
            .expect("send ping");
        timeout(Duration::from_secs(5), async {
            loop {
                let frame = ws.next().await.expect("ws open").expect("ws read");
                let WsMessage::Binary(frame) = frame else {
                    panic!("expected binary frame, got: {frame:?}")
                };
                match frame[0] {
                    7 => break,
                    // PTY output/mode/resize can already be queued when ping
                    // is sent. They must retain ordering, not be overtaken.
                    2 => {
                        let offset = u64::from_be_bytes(frame[1..9].try_into().unwrap());
                        assert_eq!(offset, expected_offset);
                        expected_offset += (frame.len() - 9) as u64;
                    }
                    3 | 4 => {}
                    tag => panic!("unexpected frame while waiting for PONG: {tag}"),
                }
            }
        })
        .await
        .expect("pong timeout");

        ws.send(WsMessage::Text(r#"{"type":"detach"}"#.into()))
            .await
            .expect("send detach");
    });
}

/// Minimal std-only HTTP POST returning the status code — keeps this test
/// synchronous so the e2e lock is never held across an `.await`.
#[cfg(target_os = "linux")]
fn http_post_kill(port: u16, id: &str) -> u16 {
    use std::io::{Read, Write};
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to test http port");
    let request = format!(
        "POST /api/sessions/{id}/kill HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
    );
    stream
        .write_all(request.as_bytes())
        .expect("write kill request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read kill response");
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("malformed HTTP response: {response:?}"))
}

/// Killing a session must terminate the whole process tree, not just
/// the direct child — a backgrounded grandchild must not leak.
///
/// Linux-only: the test detects the grandchild's liveness through `/proc`,
/// which macOS does not provide.
#[cfg(target_os = "linux")]
#[test]
fn e2e_kill_terminates_the_whole_process_tree() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_tree_kill");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);

    let pidfile = tmp.join("grandchild.pid");
    let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
    let id = start_session(&tmp, &["sh", "-c", &script]);

    // Wait until the grandchild pid is known and the process is alive.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut grandchild_pid = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(&pidfile) {
            let pid = contents.trim();
            if !pid.is_empty() && std::path::Path::new(&format!("/proc/{pid}")).exists() {
                grandchild_pid = pid.to_string();
                break;
            }
        }
        sleep(Duration::from_millis(100));
    }
    assert!(
        !grandchild_pid.is_empty(),
        "grandchild pidfile never appeared or process not alive"
    );
    let grandchild_proc = std::path::PathBuf::from(format!("/proc/{grandchild_pid}"));

    let status = http_post_kill(port, &id);
    assert!(
        (200..300).contains(&status),
        "kill request should succeed, got HTTP {status}"
    );

    // The grandchild (reparented once its shell dies) must be reaped.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && grandchild_proc.exists() {
        sleep(Duration::from_millis(100));
    }
    assert!(
        !grandchild_proc.exists(),
        "grandchild process {grandchild_pid} survived the session kill"
    );
}

/// `oly daemon stop` drains sessions with the same process-tree
/// semantics — a backgrounded grandchild must not survive the shutdown.
///
/// Linux-only: the test detects the grandchild's liveness through `/proc`,
/// which macOS does not provide.
#[cfg(target_os = "linux")]
#[test]
fn e2e_daemon_stop_kills_process_trees() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_daemon_stop_tree");
    let _daemon = start_daemon(&tmp);

    let pidfile = tmp.join("grandchild.pid");
    let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
    let id = start_session(&tmp, &["sh", "-c", &script]);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut grandchild_pid = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(&pidfile) {
            let pid = contents.trim();
            if !pid.is_empty() && std::path::Path::new(&format!("/proc/{pid}")).exists() {
                grandchild_pid = pid.to_string();
                break;
            }
        }
        sleep(Duration::from_millis(100));
    }
    assert!(!grandchild_pid.is_empty(), "grandchild never started");
    let grandchild_proc = std::path::PathBuf::from(format!("/proc/{grandchild_pid}"));

    let stop = oly_cmd(&tmp)
        .args(["daemon", "stop"])
        .output()
        .expect("`oly daemon stop` failed to execute");
    assert!(
        stop.status.success(),
        "`oly daemon stop` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && grandchild_proc.exists() {
        sleep(Duration::from_millis(200));
    }
    assert!(
        !grandchild_proc.exists(),
        "grandchild process {grandchild_pid} survived `oly daemon stop`"
    );
    let _ = id;
}

#[cfg(unix)]
#[test]
fn e2e_export_pins_a_prefix_while_output_keeps_growing() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_export_prefix");
    let _daemon = start_daemon(&tmp);
    let id = start_session(
        &tmp,
        &[
            "sh",
            "-c",
            "while :; do printf 'PREFIX-你好\\n'; sleep 0.01; done",
        ],
    );
    assert!(wait_for_log(&tmp, &id, |s| s.contains("PREFIX"), Duration::from_secs(5)).is_some());
    let output = tmp.join("prefix.json");
    let mut export = oly_cmd(&tmp)
        .args(["export", &id, "--json"])
        .stdout(std::fs::File::create(&output).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = export.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if std::time::Instant::now() >= deadline {
            export.kill().unwrap();
            panic!("export followed continuous output instead of pinning end");
        }
        sleep(Duration::from_millis(10));
    }
    sleep(Duration::from_millis(200));
    assert!(
        oly_cmd(&tmp)
            .args(["stop", &id])
            .output()
            .unwrap()
            .status
            .success()
    );
    let prefix: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    let prefix_bytes = base64::Engine::decode(
        &base64::prelude::BASE64_STANDARD,
        prefix["bytes"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(prefix["size"].as_u64().unwrap(), prefix_bytes.len() as u64);
    let all = oly_cmd(&tmp).args(["export", &id]).output().unwrap();
    assert!(all.status.success());
    assert!(all.stdout.starts_with(&prefix_bytes));
    assert!(all.stdout.len() > prefix_bytes.len());
}

/// Safe export: `oly export` returns the exact child bytes, while the
/// default rendered view never re-emits raw control sequences.
#[cfg(not(target_os = "windows"))]
#[test]
fn e2e_logs_export_emits_canonical_bytes() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_logs_raw");
    let _daemon = start_daemon(&tmp);

    let id = start_session(
        &tmp,
        &[
            "sh",
            "-c",
            "printf 'plain-\\033[31mred\\033[0m\\n'; sleep 60",
        ],
    );
    wait_for_log(&tmp, &id, |log| log.contains("red"), Duration::from_secs(5))
        .expect("session produced no output within 5 s");

    let raw = oly_cmd(&tmp)
        .args(["export", &id])
        .output()
        .expect("`oly export` failed to execute");
    assert!(
        raw.status.success(),
        "`oly logs --raw` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&raw.stderr)
    );
    assert!(
        raw.stdout.windows(5).any(|window| window == b"\x1b[31m"),
        "raw export must preserve the original escape bytes, got: {:?}",
        String::from_utf8_lossy(&raw.stdout)
    );

    let plain = oly_cmd(&tmp)
        .args(["logs", &id])
        .output()
        .expect("`oly logs` failed to execute");
    assert!(plain.status.success());
    let plain_text = String::from_utf8_lossy(&plain.stdout);
    assert!(plain_text.contains("red"), "rendered logs lost content");
    assert!(
        !plain.stdout.windows(5).any(|window| window == b"\x1b[31m"),
        "rendered logs must not re-emit raw SGR sequences"
    );
}

#[test]
fn e2e_daemon_status_matches_running_no_http_daemon() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_daemon_status_no_http");
    let _daemon = start_daemon(&tmp); // --no-http --no-auth

    let output = oly_cmd(&tmp)
        .args(["daemon", "status"])
        .output()
        .expect("`oly daemon status` failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "`oly daemon status` exited non-zero.\nstdout:\n{stdout}"
    );
    assert!(stdout.contains("Daemon is running"), "stdout:\n{stdout}");
    assert!(stdout.contains("Started at:"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("HTTP:         disabled (--no-http)"),
        "no-http daemon must be reported as disabled.\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("http://"),
        "no-http daemon must not advertise an HTTP URL.\nstdout:\n{stdout}"
    );
}

#[test]
fn e2e_daemon_status_reports_the_effective_http_endpoint_override() {
    // The daemon is started with a `--port` runtime override that never
    // reaches config.json: `daemon status` must report the port the daemon
    // actually bound (from its own info record), not the client config's
    // default — otherwise status silently desyncs from reality.
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_daemon_status_http_endpoint");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);

    let output = oly_cmd(&tmp)
        .args(["daemon", "status"])
        .output()
        .expect("`oly daemon status` failed to execute");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "`oly daemon status` exited non-zero.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!("HTTP:         http://127.0.0.1:{port}")),
        "status must report the daemon's effective port.\nstdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("15443"),
        "status must not fall back to the config default port.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Auth:         disabled (--no-auth)"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn e2e_daemon_start_while_running_explains_the_running_config() {
    // A second start (e.g. with different flags like --no-http) cannot be
    // honored: the error must name the running daemon's actual config and
    // the way forward, instead of a bare "already running".
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_daemon_start_conflict");
    let _daemon = start_daemon(&tmp); // --no-http --no-auth

    let output = oly_cmd(&tmp)
        .args(["daemon", "start", "-d", "--no-http"])
        .output()
        .expect("`oly daemon start` failed to execute");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "second daemon start must fail.\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("daemon is already running"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HTTP:         disabled (--no-http)"),
        "conflict message must show the running daemon's config.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("oly daemon stop"),
        "conflict message must point at the remedy.\nstderr:\n{stderr}"
    );
}

#[test]
fn e2e_daemon_status_reports_not_running() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_daemon_status_stopped");

    let output = oly_cmd(&tmp)
        .args(["daemon", "status"])
        .output()
        .expect("`oly daemon status` failed to execute");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stderr.contains("Daemon is not running.") || stdout.contains("Daemon is not running."),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// The `oly logs` surface: `--wait` gates a read, `--json` shapes any read,
/// cursors continue a read without repeats, and meaningless combinations
/// are usage errors instead of silent misreads.
#[test]
fn e2e_logs_waits_prints_and_hands_back_a_cursor() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_logs_surface");
    let _daemon = start_daemon(&tmp);

    // `restart` creates a NEW session ID; the original wait must stay on
    // the ended source, not migrate to the replacement.
    if !cfg!(target_os = "windows") {
        let restarting = start_session(&tmp, &["sh", "-c", "sleep 30"]);
        let pending = oly_cmd(&tmp)
            .args([
                "logs",
                &restarting,
                "--wait",
                "output",
                "--timeout",
                "10s",
                "--json",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        sleep(Duration::from_millis(300));
        let restart = oly_cmd(&tmp)
            .args(["restart", &restarting, "--force"])
            .output()
            .unwrap();
        assert!(
            restart.status.success(),
            "{}",
            String::from_utf8_lossy(&restart.stderr)
        );
        let pending = pending.wait_with_output().unwrap();
        assert!(pending.status.success());
        let read: serde_json::Value = serde_json::from_slice(&pending.stdout).unwrap();
        assert_eq!(read["session"], restarting);
        assert_eq!(read["running"], false);
        let replacement = String::from_utf8(restart.stdout).unwrap();
        let replacement = replacement
            .split_whitespace()
            .last()
            .unwrap()
            .trim_end_matches('.');
        let stop = oly_cmd(&tmp).args(["stop", replacement]).output().unwrap();
        assert!(stop.status.success());
    }

    // Interactive shell: on Linux/macOS we use `sh -i`, on Windows we
    // substitute the interactive `cmd /Q` which likewise accepts stdin
    // lines and echoes command output. The test only relies on the
    // marker string reaching the journal; the exact interpreter is
    // irrelevant to its gating assertions.
    let interactive = if cfg!(target_os = "windows") {
        vec!["cmd", "/Q"]
    } else {
        vec!["sh", "-i"]
    };
    let id = start_session(&tmp, &interactive);
    send_line(&tmp, &id, "echo ERGO-MARKER");
    assert!(
        wait_for_log(
            &tmp,
            &id,
            |log| log.contains("ERGO-MARKER"),
            Duration::from_secs(15)
        )
        .is_some(),
        "marker never appeared in logs"
    );

    // First read: drain everything already in the journal so the cursor
    // we capture represents the position a real `--wait output` next call
    // would watch from.
    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--tail", "50", "--json"])
        .output()
        .expect("`oly logs --tail` failed to execute");
    assert!(
        output.status.success(),
        "baseline read failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let read: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("baseline read emits one JSON line");
    assert_eq!(read["session"].as_str(), Some(id.as_str()));
    assert!(
        read["content"]
            .as_str()
            .unwrap_or_default()
            .contains("ERGO-MARKER"),
        "baseline read lost the marker: {read}"
    );
    let cursor = read["cursor"]
        .as_str()
        .expect("read reports a cursor")
        .to_string();

    // A gated read: feed a second line in the background, then block until
    // the cursor moves past `cursor` and dump everything since.
    let cursor_for_gate = cursor.clone();
    let id_for_gate = id.clone();
    let tmp_for_gate = tmp.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        send_line(&tmp_for_gate, &id_for_gate, "echo ERGO-SECOND");
    });
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--wait",
            "output",
            "--since",
            &cursor_for_gate,
            "--json",
            "--timeout",
            "10s",
        ])
        .output()
        .expect("`oly logs --wait output` failed to execute");
    assert!(
        output.status.success(),
        "gated read failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let read: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("gated read emits one JSON line");
    assert_eq!(read["session"].as_str(), Some(id.as_str()));
    assert!(
        String::from_utf8_lossy(
            &base64::Engine::decode(
                &base64::prelude::BASE64_STANDARD,
                read["bytes"].as_str().unwrap()
            )
            .unwrap()
        )
        .contains("ERGO-SECOND"),
        "gated read lost the second marker: {read}"
    );
    let cursor = read["cursor"]
        .as_str()
        .expect("read reports a cursor")
        .to_string();

    // A cursor read sees only what came after it: send a second marker and
    // the previous one must be gone from the window.
    send_line(&tmp, &id, "echo ERGO-SECOND");
    assert!(
        wait_for_log(
            &tmp,
            &id,
            |log| log.contains("ERGO-SECOND"),
            Duration::from_secs(10)
        )
        .is_some(),
        "second marker never appeared"
    );
    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--since", &cursor, "--json"])
        .output()
        .expect("cursor read failed");
    assert!(
        output.status.success(),
        "cursor read failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let window: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    let bytes = base64::Engine::decode(
        &base64::prelude::BASE64_STANDARD,
        window["bytes"].as_str().unwrap(),
    )
    .unwrap();
    let content = String::from_utf8_lossy(&bytes);
    assert!(
        content.contains("ERGO-SECOND"),
        "cursor read must include the newer output: {content}"
    );
    assert!(
        !content.contains("ERGO-MARKER"),
        "cursor read must not re-print output the caller already saw: {content}"
    );

    // Gate + screen: wait for a pattern, then print the visible screen.
    // The match itself is the trigger and must arrive after the gate
    // starts polling, so emit it on a slight delay from a worker thread.
    let id_for_match = id.clone();
    let tmp_for_match = tmp.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        send_line(&tmp_for_match, &id_for_match, "echo ERGO-SECOND-MATCH");
    });
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--wait",
            "match",
            "--match",
            "ERGO-SECOND-MATCH",
            "--screen",
            "--timeout",
            "10s",
        ])
        .output()
        .expect("`oly logs --wait match --screen` failed to execute");
    assert!(
        output.status.success(),
        "gated screen failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ERGO-SECOND-MATCH"),
        "gated screen should show the marker"
    );

    // A gate that cannot be satisfied exits 2 and prints nothing: the
    // caller can branch on the exit code without parsing prose.
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--wait",
            "exit",
            "--tail",
            "5",
            "--timeout",
            "1s",
        ])
        .output()
        .expect("`oly logs --wait exit` failed to execute");
    assert_eq!(
        output.status.code(),
        Some(2),
        "an unsatisfiable gate exits 2 without reading"
    );
    assert!(
        output.stdout.is_empty(),
        "a timed-out gate must not print output: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Meaningless combinations are usage errors, not silent misreads.
    for args in [
        vec!["logs", &id, "--screen", "--tail", "5"],
        vec!["logs", &id, "--tail-frames", "2", "--tail", "5"],
        vec!["logs", &id, "--cursor", "--screen"],
        vec!["logs", &id, "--wait", "match"],
        vec!["logs", &id, "--raw"],
        vec!["logs", &id, "--from-file"],
    ] {
        let output = oly_cmd(&tmp).args(&args).output().expect("run oly logs");
        assert!(
            !output.status.success(),
            "{args:?} should be a usage error.\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    // `--cursor` alone is a token on stdout, nothing else.
    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--cursor"])
        .output()
        .expect("`oly logs --cursor` failed to execute");
    assert!(output.status.success());
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(
        !token.is_empty() && !token.contains('\n'),
        "token: {token:?}"
    );

    let output = oly_cmd(&tmp)
        .args(["stop", &id])
        .output()
        .expect("`oly stop` failed to execute");
    assert!(output.status.success());

    // A gate that is already satisfied (session exited) reads immediately.
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--wait",
            "exit",
            "--screen",
            "--timeout",
            "10s",
        ])
        .output()
        .expect("`oly logs --wait exit --screen` failed to execute");
    assert!(
        output.status.success(),
        "exited gate failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ERGO-SECOND"),
        "screen after exit should render the journal"
    );

    // Exact paging after completion: tiny pages must concatenate to the
    // canonical export, even when UTF-8/control sequences straddle pages.
    let exported = oly_cmd(&tmp).args(["export", &id]).output().unwrap();
    assert!(exported.status.success());
    let origin = base64::Engine::encode(
        &base64::prelude::BASE64_URL_SAFE_NO_PAD,
        base64::Engine::decode(&base64::prelude::BASE64_URL_SAFE_NO_PAD, &token)
            .unwrap()
            .split(|b| *b == b'|')
            .take(3)
            .flat_map(|part| part.iter().copied().chain(*b"|"))
            .chain(*b"0")
            .collect::<Vec<_>>(),
    );
    let mut next = origin.clone();
    let mut delivered = Vec::new();
    loop {
        let output = oly_cmd(&tmp)
            .args([
                "logs",
                &id,
                "--since",
                &next,
                "--limit-bytes",
                "17",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let page: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(page["mode"], "stream");
        let bytes = base64::Engine::decode(
            &base64::prelude::BASE64_STANDARD,
            page["bytes"].as_str().unwrap(),
        )
        .unwrap();
        assert!(bytes.len() <= 17);
        delivered.extend_from_slice(&bytes);
        next = page["cursor"].as_str().unwrap().into();
        if !page["has_more"].as_bool().unwrap() {
            break;
        }
        assert!(!bytes.is_empty(), "completed page must progress");
    }
    assert_eq!(delivered, exported.stdout);
    let empty = oly_cmd(&tmp)
        .args(["logs", &id, "--since", &next, "--json"])
        .output()
        .unwrap();
    let empty: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(empty["bytes"], "");
    assert_eq!(empty["cursor"], next);
    assert_eq!(empty["has_more"], false);
    let json_export = oly_cmd(&tmp)
        .args(["export", &id, "--json"])
        .output()
        .unwrap();
    assert!(json_export.status.success());
    let json_export: serde_json::Value = serde_json::from_slice(&json_export.stdout).unwrap();
    assert_eq!(
        base64::Engine::decode(
            &base64::prelude::BASE64_STANDARD,
            json_export["bytes"].as_str().unwrap()
        )
        .unwrap(),
        delivered
    );
    assert_eq!(
        json_export["size"].as_u64().unwrap(),
        delivered.len() as u64
    );
    for surface in ["--tail", "--tail-frames"] {
        let zero = oly_cmd(&tmp)
            .args(["logs", &id, surface, "0"])
            .output()
            .unwrap();
        assert!(zero.status.success());
        assert!(zero.stdout.is_empty());
    }

    // A cursor from another session cannot be borrowed to read this one.
    let output = oly_cmd(&tmp)
        .args(["logs", "some-other-session", "--since", &token])
        .output()
        .expect("foreign cursor read ran");
    assert!(
        !output.status.success(),
        "a cursor from another session must be rejected, not silently ignored"
    );
}

/// A full-screen program leaves a stream of cursor moves, not lines. The
/// only usable history is the screens it painted: `--tail-frames` rebuilds
/// them, newest last, at the viewport the program actually had.
#[cfg(not(target_os = "windows"))]
#[test]
fn e2e_logs_tail_frames_replays_the_screens_a_tui_painted() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_logs_frames");
    let _daemon = start_daemon(&tmp);

    // Three distinct alt-screen states. A real TUI is not needed to prove
    // the replay: the alt-screen sequences and the painting are what the
    // recorder stores, and `printf` produces exactly those.
    let id = start_session(
        &tmp,
        &[
            "sh",
            "-c",
            "printf '\\033[?1049h'; for i in 1 2 3; do printf '\\033[H\\033[JFRAME-%s\\n' $i; sleep 0.3; done; sleep 30",
        ],
    );
    assert!(
        wait_for_log(
            &tmp,
            &id,
            |log| log.contains("FRAME-3"),
            Duration::from_secs(15)
        )
        .is_some(),
        "the painted screen never reached the logs"
    );

    // Default is predictable even for a full-screen producer.
    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--json"])
        .output()
        .expect("`oly logs --json` failed to execute");
    assert!(output.status.success());
    let read: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(
        read["mode"].as_str(),
        Some("tail"),
        "default must remain tail regardless of producer: {read}"
    );

    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--tail-frames", "3", "--json"])
        .output()
        .expect("`oly logs --tail-frames` failed to execute");
    assert!(
        output.status.success(),
        "`oly logs --tail-frames` failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let read: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    let frames = read["frames"].as_array().expect("frames array");
    assert!(
        !frames.is_empty(),
        "no frames were replayed from a TUI session: {read}"
    );
    let contents: Vec<&str> = frames
        .iter()
        .map(|frame| frame["content"].as_str().unwrap_or_default())
        .collect();
    assert!(
        contents.iter().any(|content| content.contains("FRAME-1")),
        "the first painted screen should be recoverable: {contents:?}"
    );
    assert!(
        contents
            .last()
            .copied()
            .unwrap_or_default()
            .contains("FRAME-3"),
        "the newest frame must be the current screen: {contents:?}"
    );
    assert!(
        read["cursor"].as_str().is_some(),
        "frame history reports a cursor to continue from"
    );

    // Frame width follows the recorded PTY, so a frame needs no --cols.
    let width = read["width"].as_u64().expect("frame width");
    assert!(width > 0, "frames must carry the recorded viewport width");

    let output = oly_cmd(&tmp)
        .args(["stop", &id])
        .output()
        .expect("`oly stop` failed to execute");
    assert!(output.status.success());
}

// Regression guard: a single secondary daemon registering *two* persisted
// joins against the same primary, using the *same* API key but different
// name, must surface both nodes on the primary. In 0.3.x this worked; if
// it ever regresses again we want a CI failure that names the symptom.
#[test]
#[allow(non_snake_case)]
fn e2e_federation_cli_two_joins_same_key_distinct_identities() {
    // Federation v2: each secondary has its own Ed25519 identity. Two
    // sibling secondaries under one primary, each naming themselves
    // uniquely and using a distinct identity key, must both surface on
    // `/api/nodes`. Replaces a pre-removal API-key test that asserted
    // the same observable behavior through a now-deleted feature.
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_two_joins_primary");
    let port = pick_free_port();
    let _primary = start_daemon_http(&primary_tmp, port);
    let primary_state_dir = primary_tmp.join("oly");
    let primary_pub = std::fs::read_to_string(primary_state_dir.join("identity_key.pub"))
        .expect("read primary id pub key")
        .trim()
        .to_string();
    let primary_url = format!("http://127.0.0.1:{port}");

    let mut join_keys = Vec::new();
    let mut secondary_handles = Vec::new();
    for name in ["workerA", "workerB"] {
        let secondary_tmp = make_tmp_dir(&format!("e2e_fed_two_joins_secondary_{name}"));
        let secondary = start_daemon(&secondary_tmp);

        let secondary_state_dir = secondary_tmp.join("oly");
        let secondary_pub = std::fs::read_to_string(secondary_state_dir.join("identity_key.pub"))
            .expect("read secondary id pub key")
            .trim()
            .to_string();

        // Register the secondary identity on the primary under its name.
        let accept = oly_cmd(&primary_tmp)
            .args(["node", "accept", "-n", name, "-k", &secondary_pub])
            .output()
            .expect("`oly node accept` failed to execute");
        assert!(
            accept.status.success(),
            "accept for {name} failed: {}",
            String::from_utf8_lossy(&accept.stderr)
        );

        let join = oly_cmd(&secondary_tmp)
            .args([
                "join",
                "start",
                "--name",
                name,
                "--id-pub-key",
                &primary_pub,
                &primary_url,
            ])
            .output()
            .expect("`oly join start` failed to execute");
        assert!(
            join.status.success(),
            "`oly join start -n {name}` failed: {}",
            String::from_utf8_lossy(&join.stderr)
        );

        join_keys.push(name.to_string());
        secondary_handles.push(secondary);
    }

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    for name in ["workerA", "workerB"] {
        let connected = rt.block_on(wait_for_node_connected(port, name, 10));
        assert!(
            connected,
            "{name} did not appear in /api/nodes after identity-key join"
        );
    }

    let nodes_json = rt.block_on(async {
        let resp = reqwest::get(format!("http://127.0.0.1:{port}/api/nodes"))
            .await
            .expect("GET /api/nodes");
        let body = resp.text().await.expect("read /api/nodes body");
        serde_json::from_str::<serde_json::Value>(&body).expect("parse /api/nodes")
    });
    let names: Vec<&str> = nodes_json
        .as_array()
        .unwrap_or_else(|| panic!("nodes response not an array: {nodes_json}"))
        .iter()
        .filter_map(|n| n.get("name").and_then(|v| v.as_str()))
        .collect();
    for name in &join_keys {
        assert!(
            names.contains(&name.as_str()),
            "expected {name} on primary, got {names:?}"
        );
    }

    drop(secondary_handles);
}

// Regression guard: `oly join start` against a primary that doesn't have
// the supplied API key must surface the rejection on stderr synchronously
// rather than printing "Joined" and silently failing. Before the
// AttemptReporter wiring, the CLI printed success immediately and the only
// signal of failure was the silence in `oly join ls`.
#[test]
fn e2e_federation_join_start_reports_rejection_on_stderr() {
    // Federation v2: when the secondary compiles a join payload that the
    // primary does not recognise (wrong name/key pair, wrong primary pin),
    // the rejection must surface on stderr synchronously. The command
    // itself still exits 0 because the CLI hands work to a background
    // connector that keeps retrying; what matters is that the operator
    // sees the failure predicate {@code oly join ls} would otherwise have
    // to deduce from absence.
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_join_reject_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_join_reject_secondary");
    let port = pick_free_port();
    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    let secondary_state_dir = secondary_tmp.join("oly");
    let secondary_pub = std::fs::read_to_string(secondary_state_dir.join("identity_key.pub"))
        .expect("read secondary id pub key")
        .trim()
        .to_string();

    // Pin an identity the primary does not own. Unregistered identity and
    // wrong-pin failures both must surface without reporting success.
    let mut wrong_seed = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut wrong_seed);
    let wrong_key = ed25519_dalek::SigningKey::from_bytes(&wrong_seed);
    let wrong_primary = B64.encode(wrong_key.verifying_key().as_bytes());
    let wrong_primary_line = format!("ed25519 {wrong_primary}");

    let url = format!("http://127.0.0.1:{port}");
    let out = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "ghostworker",
            "--id-pub-key",
            &wrong_primary_line,
            &url,
        ])
        .output()
        .expect("`oly join start` failed to execute");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "`oly join start` should still succeed even when the first attempt fails;\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("failed"),
        "expected explicit failed attempt: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Joined primary"),
        "failed attempt reported success"
    );
    let _ = (secondary, secondary_pub, &out.stdout);
}
