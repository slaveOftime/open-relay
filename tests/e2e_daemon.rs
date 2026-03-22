mod e2e;

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

fn node_ws_json_frame(value: serde_json::Value) -> WsMessage {
    WsMessage::Binary(value.to_string().into_bytes().into())
}

fn expect_node_ws_json(frame: WsMessage, context: &str) -> serde_json::Value {
    match frame {
        WsMessage::Binary(bytes) => {
            serde_json::from_slice(&bytes).unwrap_or_else(|err| panic!("parse {context}: {err}"))
        }
        other => panic!("unexpected {context} frame: {other:?}"),
    }
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
        ws1.send(node_ws_json_frame(
            json!({"type": "join", "name": "worker1", "key": key}),
        ))
        .await
        .expect("send worker1 join message");
        let first = timeout(Duration::from_secs(2), ws1.next())
            .await
            .expect("timed out waiting for worker1 join response")
            .expect("worker1 websocket closed")
            .expect("worker1 websocket read failed");
        let first_json = expect_node_ws_json(first, "worker1 join response");
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
            .send(node_ws_json_frame(
                json!({"type": "join", "name": "worker1", "key": key}),
            ))
            .await
            .expect("send duplicate join message");
        let dup = timeout(Duration::from_secs(2), ws_dup.next())
            .await
            .expect("timed out waiting for duplicate join response")
            .expect("duplicate websocket closed")
            .expect("duplicate websocket read failed");
        let dup_json = expect_node_ws_json(dup, "duplicate join response");
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
        ws2.send(node_ws_json_frame(
            json!({"type": "join", "name": "worker2", "key": key}),
        ))
        .await
        .expect("send worker2 join message");
        let second = timeout(Duration::from_secs(2), ws2.next())
            .await
            .expect("timed out waiting for worker2 join response")
            .expect("worker2 websocket closed")
            .expect("worker2 websocket read failed");
        let second_json = expect_node_ws_json(second, "worker2 join response");
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
        ws3.send(node_ws_json_frame(
            json!({"type": "join", "name": "worker3", "key": key}),
        ))
        .await
        .expect("send worker3 join message");

        let third = timeout(Duration::from_secs(2), ws3.next())
            .await
            .expect("timed out waiting for worker3 join response")
            .expect("worker3 websocket closed")
            .expect("worker3 websocket read failed");
        let third_json = expect_node_ws_json(third, "worker3 join response");
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
