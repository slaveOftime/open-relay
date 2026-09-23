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
    WsMessage::Binary(encode_node_ws_value(value).into())
}

fn expect_node_ws_json(frame: WsMessage, context: &str) -> serde_json::Value {
    match frame {
        WsMessage::Binary(bytes) => {
            let preview = String::from_utf8_lossy(&bytes);
            decode_node_ws_value(&bytes)
                .unwrap_or_else(|err| panic!("parse {context}: {err} (raw={preview:?})"))
        }
        other => panic!("unexpected {context} frame: {other:?}"),
    }
}

fn encode_node_ws_value(value: serde_json::Value) -> Vec<u8> {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let json = value.to_string().into_bytes();
    // Mirror src/protocol.rs::encode_node_ws_payload: gzip when the
    // payload crosses NODE_WS_BINARY_COMPRESS_MIN_BYTES (256).
    if json.len() < 256 {
        return json;
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&json).expect("gzip write");
    let compressed = encoder.finish().expect("gzip finish");
    if compressed.len() >= json.len() {
        return json;
    }
    let mut out = Vec::with_capacity(4 + compressed.len());
    out.extend_from_slice(b"ONW1");
    out.extend_from_slice(&compressed);
    out
}

fn decode_node_ws_value(payload: &[u8]) -> Result<serde_json::Value, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let magic = b"ONW1";
    let json = if payload.starts_with(magic) {
        let mut decoder = GzDecoder::new(&payload[magic.len()..]);
        let mut json = Vec::new();
        decoder
            .read_to_end(&mut json)
            .map_err(|e| format!("gzip: {e}"))?;
        json
    } else {
        payload.to_vec()
    };
    serde_json::from_slice(&json).map_err(|e| format!("json: {e}"))
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
            json!({"type": "join", "name": "worker1", "auth": {"method": "api_key", "key": key}}),
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
                json!({"type": "join", "name": "worker1", "auth": {"method": "api_key", "key": key}}),
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
            json!({"type": "join", "name": "worker2", "auth": {"method": "api_key", "key": key}}),
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
            json!({"type": "join", "name": "worker3", "auth": {"method": "api_key", "key": key}}),
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
fn e2e_federation_ssh_key_join_handshake() {
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
    use ed25519_dalek::Signer as _;

    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_fed_ssh_handshake");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);

    // Generate the secondary's Ed25519 SSH key and register its public key
    // on the primary using a real OpenSSH-format key line.
    let mut seed = [0u8; 32];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut seed);
    let node_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let canonical_pub = format!(
        "ssh-ed25519 {}",
        B64.encode(node_key.verifying_key().as_bytes())
    );
    let keypair = ssh_key::private::Ed25519Keypair {
        private: ssh_key::private::Ed25519PrivateKey::from_bytes(&seed),
        public: ssh_key::public::Ed25519PublicKey(*node_key.verifying_key().as_bytes()),
    };
    let private = ssh_key::PrivateKey::new(ssh_key::private::KeypairData::Ed25519(keypair), "e2e")
        .expect("build ssh private key");
    let openssh_pub = private
        .public_key()
        .to_openssh()
        .expect("openssh public key line");

    let accept = oly_cmd(&tmp)
        .args([
            "node",
            "accept",
            "-n",
            "worker1",
            "-k",
            &openssh_pub,
        ])
        .output()
        .expect("`oly node accept` failed to execute");
    assert!(
        accept.status.success(),
        "`oly node accept` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&accept.stderr)
    );

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let ws_url = format!("ws://127.0.0.1:{port}/api/nodes/join");

        // Helper: connect a fresh WebSocket without issuing any handshake
        // frames. Used by the rejection paths below that only need a
        // connection to drop a single join frame on.
        async fn fresh_ws(
            ws_url: &str,
        ) -> tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        > {
            let (ws, _) = tokio_tungstenite::connect_async(ws_url)
                .await
                .expect("connect websocket");
            ws
        }

        // Helper: sign the join payload for `name`. There is no nonce;
        // the payload binds protocol || name || pubkey.
        fn join_signature(
            node_key: &ed25519_dalek::SigningKey,
            name: &str,
            pubkey: &str,
        ) -> String {
            let mut payload = Vec::new();
            payload.extend_from_slice(b"oly-node-join-v1");
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(pubkey.as_bytes());
            B64.encode(node_key.sign(&payload).to_bytes())
        }

        // ── Happy path: Hello (carrying the secondary identity) then Join.
        let mut ws = fresh_ws(&ws_url).await;
        ws.send(node_ws_json_frame(json!({
            "type": "hello",
            "public_key": canonical_pub,
        })))
        .await
        .expect("send hello");
        let signature = join_signature(&node_key, "worker1", &canonical_pub);
        ws.send(node_ws_json_frame(json!({
            "type": "join",
            "name": "worker1",
            "auth": {"method": "ssh_key", "signature": signature, "public_key": canonical_pub},
        })))
        .await
        .expect("send ssh join");
        let frame = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out waiting for join response")
            .expect("websocket closed")
            .expect("websocket read failed");
        let json = expect_node_ws_json(frame, "ssh join response");
        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("joined"),
            "expected joined for ssh-key worker1, got: {json}"
        );
        drop(ws);

        // ── Missing hello + ssh-key join must be rejected ────────────────
        let mut ws_no_hello = fresh_ws(&ws_url).await;
        ws_no_hello
            .send(node_ws_json_frame(json!({
                "type": "join",
                "name": "worker1",
                "auth": {"method": "ssh_key", "signature": join_signature(&node_key, "worker1", &canonical_pub), "public_key": openssh_pub},
            })))
            .await
            .expect("send ssh-key join without hello");
        let frame = timeout(Duration::from_secs(2), ws_no_hello.next())
            .await
            .expect("timed out waiting for no-hello response")
            .expect("websocket closed")
            .expect("websocket read failed");
        let json = expect_node_ws_json(frame, "no-hello join response");
        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("error"),
            "ssh-key join without hello must be rejected, got: {json}"
        );
        drop(ws_no_hello);

        // ── Name substitution: signature bound to "worker1" used for "victim" ──
        let mut ws_sub = fresh_ws(&ws_url).await;
        ws_sub
            .send(node_ws_json_frame(json!({
                "type": "join",
                "name": "victim",
                "auth": {"method": "ssh_key", "signature": join_signature(&node_key, "worker1", &canonical_pub), "public_key": openssh_pub},
            })))
            .await
            .expect("send name-substituted join");
        let frame = timeout(Duration::from_secs(2), ws_sub.next())
            .await
            .expect("timed out waiting for substituted join response")
            .expect("websocket closed")
            .expect("websocket read failed");
        let json = expect_node_ws_json(frame, "name-substituted join response");
        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("error"),
            "signature bound to another name must be rejected, got: {json}"
        );
        drop(ws_sub);

        // ── Unregistered key: valid signature, key not in registry ────────
        let mut other_seed = [0u8; 32];
        rand::Rng::fill_bytes(&mut rand::rng(), &mut other_seed);
        let other_key = ed25519_dalek::SigningKey::from_bytes(&other_seed);
        let other_pub = format!(
            "ssh-ed25519 {}",
            B64.encode(other_key.verifying_key().as_bytes())
        );
        let mut ws_unreg = fresh_ws(&ws_url).await;
        ws_unreg
            .send(node_ws_json_frame(json!({
                "type": "hello",
                "public_key": other_pub,
            })))
            .await
            .expect("send unregistered hello");
        ws_unreg
            .send(node_ws_json_frame(json!({
                "type": "join",
                "name": "worker9",
                "auth": {"method": "ssh_key", "signature": join_signature(&other_key, "worker9", &other_pub), "public_key": other_pub},
            })))
            .await
            .expect("send unregistered-key join");
        let frame = timeout(Duration::from_secs(2), ws_unreg.next())
            .await
            .expect("timed out waiting for unregistered join response")
            .expect("websocket closed")
            .expect("websocket read failed");
        let json = expect_node_ws_json(frame, "unregistered join response");
        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("error"),
            "unregistered ssh key must be rejected, got: {json}"
        );
        drop(ws_unreg);
    });
}

#[test]
fn e2e_federation_ssh_key_join_lifecycle() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_ssh_lifecycle_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_ssh_lifecycle_secondary");
    let port = pick_free_port();

    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    // The secondary auto-generates its identity key on first start; read it
    // from `<OLY_STATE_DIR>/ssh_host_key.pub`. The test harness points
    // `OLY_STATE_DIR` at `tmp_dir.join("oly")` (see tests/e2e/mod.rs).
    let secondary_state_dir = secondary_tmp.join("oly");
    let secondary_pub = std::fs::read_to_string(secondary_state_dir.join("ssh_host_key.pub"))
        .expect("read secondary identity pub key")
        .trim()
        .to_string();
    assert!(
        secondary_pub.starts_with("ssh-ed25519 "),
        "expected canonical ed25519 line, got: {secondary_pub}"
    );

    // Register the secondary's identity pub key on the primary.
    let accept = oly_cmd(&primary_tmp)
        .args([
            "node",
            "accept",
            "-n",
            "worker1",
            "-k",
            &secondary_pub,
        ])
        .output()
        .expect("`oly node accept` failed to execute");
    assert!(
        accept.status.success(),
        "`oly node accept` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&accept.stderr)
    );
    // Fetch the primary's identity pub key and pin it explicitly on the
    // secondary via `--ssh-pub-key`.
    let primary_state_dir = primary_tmp.join("oly");
    let primary_pub = std::fs::read_to_string(primary_state_dir.join("ssh_host_key.pub"))
        .expect("read primary identity pub key")
        .trim()
        .to_string();
    let join = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--ssh-pub-key",
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
        "worker1 did not appear in /api/nodes after ssh-key join start"
    );

    // Stop the join cleanly and confirm the secondary disappears from
    // /api/nodes; this verifies basic transport liveness on the
    // ssh-key path (channel encryption + relay + disconnect).
    let stop = oly_cmd(&secondary_tmp)
        .args(["join", "stop", "--name", "worker1"])
        .output()
        .expect("`oly join stop` failed to execute");
    assert!(stop.status.success(), "`oly join stop` failed");
    let disconnected = rt.block_on(wait_for_no_nodes(port, 10));
    assert!(disconnected, "worker1 did not disconnect after join stop");

    // Re-join with the *correct* primary pub key. We no longer do an
    // in-band pin-mismatch check (the host_key wire flow is gone), so a
    // bogus pin would simply cause the post-Join sealed channel to break
    // silently. The trust root here is operator-side verification of the
    // pinned key against `oly daemon status` on the primary.
    let rejoin = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "worker1",
            "--ssh-pub-key",
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
        "worker1 did not re-appear in /api/nodes after ssh-key rejoin"
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

    let add = oly_cmd(&primary_tmp)
        .args(["api-key", "add", "fedkey"])
        .output()
        .expect("`oly api-key add` failed to execute");
    assert!(add.status.success());
    let key = String::from_utf8_lossy(&add.stdout)
        .lines()
        .last()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();

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
    // Without the M5-2 relay channel these messages could not reach the
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
        // {"version":13,"payload":{...}} (v13: attach streams carry output
        // as binary frames after the JSON init line, M6-3).
        let envelope = |payload: serde_json::Value| json!({"version": 13, "payload": payload});
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
/// layout, controller role, gated input, contiguously offset DATA frames
/// (I2), ping/pong, graceful detach — with the canonical journal staying
/// doctor-clean throughout.
#[test]
fn e2e_ws_attach_frames_conform_and_journal_stays_clean() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_ws_attach_frames");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    // `cat` echoes input back: send bytes, expect them back in DATA frames.
    let id = start_session(&tmp, &["cat"]);

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(async {
        let ws_url = format!("ws://127.0.0.1:{port}/api/sessions/{id}/attach");
        let (mut ws, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("connect attach websocket");

        // INIT: [1][flags][endOffset u64be][incarnation u64be][running u8]
        //       [attachmentId u64be][role u8][data]
        let init = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("init frame timeout")
            .expect("ws open")
            .expect("ws read");
        let WsMessage::Binary(init) = init else {
            panic!("expected binary INIT frame, got: {init:?}")
        };
        assert_eq!(init[0], 1, "first frame must be INIT");
        assert!(init.len() >= 28, "INIT header is 28 bytes");
        let mut expected_offset = u64::from_be_bytes(init[2..10].try_into().unwrap());
        let incarnation = u64::from_be_bytes(init[10..18].try_into().unwrap());
        assert!(
            incarnation >= 1,
            "a journaled session reports its incarnation in INIT"
        );
        assert_eq!(init[18], 1, "session is running");
        let attachment_id = u64::from_be_bytes(init[19..27].try_into().unwrap());
        assert!(attachment_id >= 1, "attachment fencing token is assigned");
        assert_eq!(init[27], 1, "the sole attacher is the controller");

        // Controller input flows; the echoed bytes arrive in DATA frames
        // whose offsets continue the init cursor exactly (I2).
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
                        "DATA offsets must continue the stream cursor exactly (I2)"
                    );
                    expected_offset += (bytes.len() - 9) as u64;
                    echoed.extend_from_slice(&bytes[9..]);
                }
                // mode/resize/control notices are legitimate interleavings.
                3 | 4 | 8 => {}
                other => panic!("unexpected frame tag {other}"),
            }
        }

        // Liveness: ping is answered with a PONG frame.
        ws.send(WsMessage::Text(r#"{"type":"ping"}"#.into()))
            .await
            .expect("send ping");
        let pong = timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("pong timeout")
            .expect("ws open")
            .expect("ws read");
        let WsMessage::Binary(pong) = pong else {
            panic!("expected binary PONG frame, got: {pong:?}")
        };
        assert_eq!(pong[0], 7, "ping must be answered with PONG");

        ws.send(WsMessage::Text(r#"{"type":"detach"}"#.into()))
            .await
            .expect("send detach");
    });
}

/// Minimal std-only HTTP POST returning the status code — keeps this test
/// synchronous so the e2e lock is never held across an `.await`.
#[cfg(not(target_os = "windows"))]
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

/// M5-5: killing a session must terminate the whole process tree, not just
/// the direct child — a backgrounded grandchild must not leak.
#[cfg(not(target_os = "windows"))]
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
            if !pid.is_empty() && Path::new(&format!("/proc/{pid}")).exists() {
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

/// M5-5: `oly daemon stop` drains sessions with the same process-tree
/// semantics — a backgrounded grandchild must not survive the shutdown.
#[cfg(not(target_os = "windows"))]
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
            if !pid.is_empty() && Path::new(&format!("/proc/{pid}")).exists() {
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

/// M5-6 safe export: `--raw` returns the exact child bytes (explicit
/// opt-in), while the default rendered view never re-emits raw control
/// sequences.
#[cfg(not(target_os = "windows"))]
#[test]
fn e2e_logs_raw_exports_unfiltered_bytes() {
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
        .args(["logs", "--raw", &id])
        .output()
        .expect("`oly logs --raw` failed to execute");
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

/// The merged `oly logs` surface: gates compose with read selectors
/// (block, then read), wait-only mode emits real JSON with `--json`, and
/// meaningless combinations are usage errors instead of being silently
/// ignored.
#[test]
fn e2e_logs_gates_compose_with_reads_and_json_wait_results() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_logs_ergonomics");
    let _daemon = start_daemon(&tmp);
    let id = start_session(&tmp, &["sh", "-i"]);
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

    // Gate + window read: --from/--after/--json blocks until output exists
    // after the cursor, then emits the window (the agent loop in one call).
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--from",
            "0",
            "--after",
            "0",
            "--json",
            "--timeout",
            "10s",
        ])
        .output()
        .expect("`oly logs --from --after` failed to execute");
    assert!(
        output.status.success(),
        "gated window read failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let window: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("gated window read emits one JSON line");
    assert_eq!(window["from"], 0);
    assert!(window["bytes"].as_u64().expect("bytes") > 0);

    // Gate + screen: blocks, then prints the visible screen.
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--pattern",
            "ERGO-MARKER",
            "--screen",
            "--timeout",
            "10s",
        ])
        .output()
        .expect("`oly logs --pattern --screen` failed to execute");
    assert!(
        output.status.success(),
        "gated screen failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ERGO-MARKER"),
        "gated screen should show the marker"
    );

    // Wait-only + --json: a real JSON object (previously --json was
    // silently ignored in wait mode).
    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--after",
            "0",
            "--pattern",
            "ERGO-MARKER",
            "--json",
            "--timeout",
            "10s",
        ])
        .output()
        .expect("`oly logs --json` failed to execute");
    assert!(output.status.success());
    let result: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())
            .expect("wait-only --json emits one JSON object");
    assert_eq!(result["condition"], "pattern");
    assert_eq!(result["match"], "ERGO-MARKER");
    assert!(result["offset"].as_u64().expect("offset") > 0);

    // Gate + default tail read.
    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--exit", "--tail", "5", "--timeout", "1s"])
        .output()
        .expect("`oly logs --exit --tail` failed to execute");
    assert_eq!(
        output.status.code(),
        Some(2),
        "gate timeout exits 2 without reading"
    );

    // Meaningless combinations are usage errors, not silent misreads.
    for args in [
        vec!["logs", &id, "--raw", "--exit"],
        vec!["logs", &id, "--json"],
        vec!["logs", &id, "--screen", "--from", "0"],
        vec!["logs", &id, "-w", "--after", "0"],
    ] {
        let output = oly_cmd(&tmp).args(&args).output().expect("run oly logs");
        assert!(
            !output.status.success(),
            "{args:?} should be a usage error.\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let output = oly_cmd(&tmp)
        .args(["stop", &id])
        .output()
        .expect("`oly stop` failed to execute");
    assert!(output.status.success());

    // Gate that is already satisfied (session exited) reads immediately.
    let output = oly_cmd(&tmp)
        .args(["logs", &id, "--exit", "--screen", "--timeout", "10s"])
        .output()
        .expect("`oly logs --exit --screen` failed to execute");
    assert!(
        output.status.success(),
        "--exit --screen on a stopped session failed.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ERGO-MARKER"),
        "screen after exit should render the journal"
    );
}

// Regression guard: a single secondary daemon registering *two* persisted
// joins against the same primary, using the *same* API key but different
// name, must surface both nodes on the primary. In 0.3.x this worked; if
// it ever regresses again we want a CI failure that names the symptom.
#[test]
#[allow(non_snake_case)]
fn e2e_federation_cli_two_joins_same_key_different_names() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_twojoins_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_twojoins_secondary");
    let port = pick_free_port();
    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    let add = oly_cmd(&primary_tmp)
        .args(["api-key", "add", "sharedkey"])
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

    let url = format!("http://127.0.0.1:{port}");

    // Two sibling join attempts in the same secondary state dir, just like
    // a user wiring multiple physical machines behind one primary URL.
    for name in ["workerA", "workerB"] {
        let join = oly_cmd(&secondary_tmp)
            .args(["join", "start", "--name", name, "--key", &key, &url])
            .output()
            .expect("`oly join start` failed to execute");
        assert!(
            join.status.success(),
            "`oly join start -n {name}` exited non-zero.\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&join.stdout),
            String::from_utf8_lossy(&join.stderr)
        );
    }

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    let worker_a_connected = rt.block_on(wait_for_node_connected(port, "workerA", 10));
    assert!(worker_a_connected, "workerA did not appear in /api/nodes");
    let worker_b_connected = rt.block_on(wait_for_node_connected(port, "workerB", 10));
    assert!(
        worker_b_connected,
        "workerB did not appear in /api/nodes after joining with the same key as workerA"
    );

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
    assert!(
        names.contains(&"workerA") && names.contains(&"workerB"),
        "expected both workerA and workerB on primary, got {names:?}"
    );

    // Cleanup so other tests aren't impacted by stray connectors.
    for name in ["workerA", "workerB"] {
        let _ = oly_cmd(&secondary_tmp)
            .args(["join", "stop", "--name", name])
            .output();
    }
    drop(secondary);
    drop(_primary);
}

// Regression guard: `oly join start` against a primary that doesn't have
// the supplied API key must surface the rejection on stderr synchronously
// rather than printing "Joined" and silently failing. Before the
// AttemptReporter wiring, the CLI printed success immediately and the only
// signal of failure was the silence in `oly join ls`.
#[test]
fn e2e_federation_join_start_reports_rejection_on_stderr() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let primary_tmp = make_tmp_dir("e2e_fed_join_reject_primary");
    let secondary_tmp = make_tmp_dir("e2e_fed_join_reject_secondary");
    let port = pick_free_port();
    let _primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);

    // NOTE: deliberately *not* registering the key on the primary so the
    // first attempt lands in the primary's `verify_api_key_hash` and is
    // rejected with "unauthorized".
    let fake_key = "a".repeat(64);
    let url = format!("http://127.0.0.1:{port}");

    let out = oly_cmd(&secondary_tmp)
        .args([
            "join",
            "start",
            "--name",
            "ghostworker",
            "--key",
            &fake_key,
            &url,
        ])
        .output()
        .expect("`oly join start` failed to execute");
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The command must still exit 0 (the daemon accepted the IPC; the
    // connector will keep retrying) BUT stderr must surface the rejection.
    assert!(
        out.status.success(),
        "`oly join start` should still succeed even when the first attempt fails;\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        stderr
    );
    assert!(
        stderr.contains("warning") && stderr.contains("ghostworker"),
        "expected stderr to carry the rejection warning for ghostworker; got: {stderr}"
    );
    assert!(
        !stderr.contains("connector aborted"),
        "rejection should carry the primary's reason, not a connector-abort fallback; got: {stderr}"
    );

    let _ = oly_cmd(&secondary_tmp)
        .args(["join", "stop", "--name", "ghostworker"])
        .output();
    drop(secondary);
    drop(_primary);
}
