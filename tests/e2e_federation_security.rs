mod e2e;
// Raw adversarial peers reuse the production crypto primitives, not a second
// connector. CLI experiments below always launch the actual binary connector.
#[allow(dead_code)]
#[path = "../src/error.rs"]
mod error;
#[allow(dead_code)]
#[path = "../src/federation_crypto.rs"]
mod federation_crypto;

use e2e::*;
use federation_crypto as crypto;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{fs, path::Path, time::Duration};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn id_pub_key(tmp: &Path) -> String {
    fs::read_to_string(tmp.join("oly/identity_key.pub"))
        .unwrap()
        .trim()
        .into()
}

fn register(tmp: &Path, name: &str, key: &ed25519_dalek::SigningKey) {
    let output = oly_cmd(tmp)
        .args([
            "node",
            "add",
            name,
            "--role",
            "secondary",
            "--key",
            &crypto::public_key_line(key),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn node_info(tmp: &Path, name: &str) -> Value {
    let output = oly_cmd(tmp)
        .args(["node", "info", name, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn wait_for_connector_error(tmp: &Path, expected: Option<&str>) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let node = node_info(tmp, "primary");
        assert_ne!(
            node["status"], "connected",
            "failed handshake reported connected"
        );
        if let Some(error) = node["last_error"].as_str()
            && !error.is_empty()
            && expected.is_none_or(|expected| error.contains(expected))
            && matches!(node["status"].as_str(), Some("retrying" | "waiting"))
        {
            return node;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "connector failure did not surface in node info: {node}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// Runtime status/retry diagnostics are deliberately not persisted.
fn node_config(tmp: &Path, name: &str) -> Value {
    let mut node = node_info(tmp, name);
    for field in ["status", "last_error", "next_retry_at"] {
        node.as_object_mut().unwrap().remove(field);
    }
    node
}

async fn receive(socket: &mut Socket) -> Vec<u8> {
    loop {
        match timeout(Duration::from_secs(7), socket.next())
            .await
            .expect("frame deadline")
            .expect("socket ended")
            .expect("socket read")
        {
            Message::Binary(bytes) => return bytes.to_vec(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}
async fn send(socket: &mut Socket, value: Value) {
    socket
        .send(Message::Binary(serde_json::to_vec(&value).unwrap().into()))
        .await
        .unwrap();
}
async fn assert_empty(port: u16) {
    let body = reqwest::get(format!("http://127.0.0.1:{port}/api/nodes"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let nodes: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        nodes,
        json!([]),
        "failed or incomplete handshake registered a node"
    );
}
async fn introduced(
    port: u16,
    key: &ed25519_dalek::SigningKey,
    name: &str,
    primary: &str,
) -> (Socket, crypto::Channel, String) {
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/nodes/join"))
            .await
            .unwrap();
    let nonce = crypto::fresh_nonce();
    send(&mut socket, json!({"type":"hello", "version":2, "name":name, "public_key":crypto::public_key_line(key), "nonce":nonce})).await;
    let challenge: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
    assert_eq!(challenge["type"], "challenge");
    let primary_nonce: [u8; 32] = serde_json::from_value(challenge["nonce"].clone()).unwrap();
    let primary = crypto::parse_public_key(primary).unwrap();
    let transcript = crypto::build_transcript(
        2,
        "secondary-proof",
        name,
        key.verifying_key().as_bytes(),
        primary.as_bytes(),
        &nonce,
        &primary_nonce,
    );
    let signature = crypto::sign_transcript(key, &transcript);
    let channel = crypto::Channel::new(key, &primary, &transcript, true).unwrap();
    (socket, channel, signature)
}
async fn prove(socket: &mut Socket, channel: &mut crypto::Channel, signature: &str) {
    send(socket, json!({"type":"join", "signature":signature})).await;
    let frame = receive(socket).await;
    assert_eq!(frame[0], 0x21, "KeyConfirm must be sealed");
    assert_eq!(u64::from_be_bytes(frame[1..9].try_into().unwrap()), 0);
    let confirmation = channel
        .decode(&frame, crypto::decode_handshake::<Value>)
        .unwrap();
    assert_eq!(confirmation["type"], "key_confirm");
    assert_eq!(
        confirmation["transcript_hash"],
        json!(channel.transcript_hash())
    );
}
async fn ready(socket: &mut Socket, channel: &mut crypto::Channel) {
    let frame = channel
        .encode(
            &serde_json::to_vec(
                &json!({"type":"ready", "transcript_hash":channel.transcript_hash()}),
            )
            .unwrap(),
        )
        .unwrap();
    socket.send(Message::Binary(frame.into())).await.unwrap();
}

#[test]
fn primary_requires_fresh_proof_and_encrypted_readiness() {
    let tmp = make_tmp_dir("security_primary");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    register(&tmp, "worker", &key);
    let primary = id_pub_key(&tmp);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut socket, mut channel, signature) = introduced(port, &key, "worker", &primary).await;
        prove(&mut socket, &mut channel, &signature).await;
        assert_empty(port).await; // KeyConfirm alone must not publish a node.
        // Replay the same proof on a fresh connection: fresh primary nonce fences it.
        let (mut replay, _, _) = introduced(port, &key, "worker", &primary).await;
        send(&mut replay, json!({"type":"join", "signature":signature})).await;
        let error: Value = serde_json::from_slice(&receive(&mut replay).await).unwrap();
        assert_eq!(error["type"], "error");
        assert_empty(port).await;
        // Correctly RE-SIGN another name, unlike the old tampered-name test.
        let (mut wrong_name, _, proof) = introduced(port, &key, "victim", &primary).await;
        send(&mut wrong_name, json!({"type":"join", "signature":proof})).await;
        let error: Value = serde_json::from_slice(&receive(&mut wrong_name).await).unwrap();
        assert_eq!(error["type"], "error");
        assert_empty(port).await;
        ready(&mut socket, &mut channel).await;
        let frame = receive(&mut socket).await;
        assert_eq!(frame[0], 0x21);
        assert_eq!(u64::from_be_bytes(frame[1..9].try_into().unwrap()), 1);
        let joined = channel
            .decode(&frame, crypto::decode_handshake::<Value>)
            .unwrap();
        assert_eq!(joined["type"], "joined");
        assert!(wait_for_node_connected(port, "worker", 2).await);
        // Failed proof and duplicate fully-confirmed socket cannot evict the owner.
        let (mut duplicate, mut other_channel, proof) =
            introduced(port, &key, "worker", &primary).await;
        prove(&mut duplicate, &mut other_channel, &proof).await;
        ready(&mut duplicate, &mut other_channel).await;
        let error = other_channel
            .decode(
                &receive(&mut duplicate).await,
                crypto::decode_handshake::<Value>,
            )
            .unwrap();
        assert_eq!(error["type"], "error");
        assert!(wait_for_node_connected(port, "worker", 2).await);
        // Plaintext injection on the authenticated channel is terminal.
        send(&mut socket, json!({"type":"pong"})).await;
        assert!(wait_for_no_nodes(port, 3).await);
    });
}

#[test]
fn primary_rejects_malformed_order_legacy_and_missing_ready() {
    let tmp = make_tmp_dir("security_order");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    register(&tmp, "worker", &key);
    let primary = id_pub_key(&tmp);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        for value in [
            json!({"type":"join", "signature":"sig"}),
            json!({"type":"join", "name":"worker", "auth":{"method":"api_key", "key":"old"}}),
            json!({"type":"join", "name":"worker", "auth":{"method":"ssh_key", "public_key":crypto::public_key_line(&key), "signature":"old"}}),
            json!({"type":"hello", "version":1, "name":"worker", "public_key":crypto::public_key_line(&key), "nonce":vec![0;32]}),
        ] {
            let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/nodes/join")).await.unwrap();
            send(&mut socket, value).await;
            let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
            assert_eq!(error["type"], "error"); assert_empty(port).await;
        }
        let (mut socket, _, _) = introduced(port, &key, "worker", &primary).await;
        send(&mut socket, json!({"type":"hello", "version":2, "name":"worker", "public_key":crypto::public_key_line(&key), "nonce":vec![0;32]})).await;
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"], "error");
        let (mut socket, mut channel, proof) = introduced(port, &key, "worker", &primary).await;
        prove(&mut socket, &mut channel, &proof).await;
        send(&mut socket, json!({"type":"ready", "transcript_hash":channel.transcript_hash()})).await;
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"], "error"); assert_empty(port).await;
        let (mut socket, mut channel, proof) = introduced(port, &key, "worker", &primary).await;
        prove(&mut socket, &mut channel, &proof).await;
        // No Ready: one overall deadline, not an idle-resettable per-frame timer.
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"], "error"); assert!(error["message"].as_str().unwrap().contains("timed out"));
        assert_empty(port).await;
    });
}

#[test]
fn replayed_application_frame_has_only_one_observable_side_effect() {
    let tmp = make_tmp_dir("security_frame_replay");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    register(&tmp, "worker", &key);
    let primary = id_pub_key(&tmp);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = reqwest::Client::new();
        let events = client
            .get(format!("http://127.0.0.1:{port}/api/sessions/events"))
            .send()
            .await
            .unwrap();
        assert!(events.status().is_success());
        let mut events = events.bytes_stream();
        let (mut socket, mut channel, proof) = introduced(port, &key, "worker", &primary).await;
        prove(&mut socket, &mut channel, &proof).await;
        ready(&mut socket, &mut channel).await;
        let joined = channel
            .decode(
                &receive(&mut socket).await,
                crypto::decode_handshake::<Value>,
            )
            .unwrap();
        assert_eq!(joined["type"], "joined");
        // A benign forwarded event is externally visible over the real SSE path.
        let marker = "oly_replay_marker_unique";
        let payload = serde_json::to_vec(&json!({"type":"session_event","payload":{
            "event":"resync_required","reason":marker
        }}))
        .unwrap();
        let frame = channel.encode(&payload).unwrap();
        socket
            .send(Message::Binary(frame.clone().into()))
            .await
            .unwrap();
        let mut observed = String::new();
        timeout(Duration::from_secs(3), async {
            while !observed.contains(marker) {
                observed.push_str(&String::from_utf8_lossy(
                    &events.next().await.unwrap().unwrap(),
                ));
            }
        })
        .await
        .unwrap();
        socket
            .send(Message::Binary(frame.clone().into()))
            .await
            .unwrap();
        assert!(
            wait_for_no_nodes(port, 3).await,
            "replay did not close connection"
        );
        timeout(Duration::from_secs(3), async {
            while !observed.contains("\"connected\":false") {
                observed.push_str(&String::from_utf8_lossy(
                    &events.next().await.unwrap().unwrap(),
                ));
            }
        })
        .await
        .unwrap();
        assert_eq!(
            observed.matches(marker).count(),
            1,
            "replay duplicated observable event: {observed}"
        );
        // A frame from this old session cannot authenticate a fresh session.
        let (mut fresh, mut new_channel, proof) = introduced(port, &key, "worker", &primary).await;
        prove(&mut fresh, &mut new_channel, &proof).await;
        ready(&mut fresh, &mut new_channel).await;
        new_channel
            .decode(
                &receive(&mut fresh).await,
                crypto::decode_handshake::<Value>,
            )
            .unwrap();
        fresh.send(Message::Binary(frame.into())).await.unwrap();
        assert!(wait_for_no_nodes(port, 3).await);
    });
}

#[test]
fn failed_channel_fails_pending_rpc_and_removes_owned_registry_entry() {
    let tmp = make_tmp_dir("security_pending_rpc");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    register(&tmp, "worker", &key);
    let primary = id_pub_key(&tmp);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut socket, mut channel, proof) = introduced(port, &key, "worker", &primary).await;
        prove(&mut socket, &mut channel, &proof).await;
        ready(&mut socket, &mut channel).await;
        channel
            .decode(
                &receive(&mut socket).await,
                crypto::decode_handshake::<Value>,
            )
            .unwrap();
        let child = oly_cmd(&tmp)
            .args(["ls", "--json", "--node", "worker"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        loop {
            let payload = channel
                .decode(&receive(&mut socket).await, |bytes| {
                    use std::io::Read;
                    let json = if bytes.starts_with(b"ONW1") {
                        let mut json = Vec::new();
                        flate2::read::GzDecoder::new(&bytes[4..]).read_to_end(&mut json)?;
                        json
                    } else {
                        bytes.to_vec()
                    };
                    Ok(serde_json::from_slice::<Value>(&json)?)
                })
                .unwrap();
            if payload["type"] == "rpc" {
                break;
            }
            assert_eq!(payload["type"], "ping");
        }
        // Caller is now genuinely waiting for a response; invalid AEAD closes it.
        socket
            .send(Message::Binary(vec![0x21; 25].into()))
            .await
            .unwrap();
        let output = timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
        )
        .await
        .expect("pending RPC remained hung after channel failure")
        .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("not connected"));
        assert!(wait_for_no_nodes(port, 2).await);
    });
}

/// Trust boundary: a relayed attach whose owning node disappears
/// mid-stream must fail loudly with the interruption reason — never a
/// fabricated completion (`final_offset: 0` / "has ended").
#[test]
fn relayed_attach_reports_interruption_instead_of_fabricated_completion() {
    let tmp = make_tmp_dir("security_relay_interrupt");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    register(&tmp, "worker", &key);
    let primary = id_pub_key(&tmp);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut socket, mut channel, proof) = introduced(port, &key, "worker", &primary).await;
        prove(&mut socket, &mut channel, &proof).await;
        ready(&mut socket, &mut channel).await;
        channel
            .decode(
                &receive(&mut socket).await,
                crypto::decode_handshake::<Value>,
            )
            .unwrap();

        let child = oly_cmd(&tmp)
            .args(["attach", "abc1234", "--node", "worker"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        // Answer the relayed subscribe with an init frame, then make the
        // owning node unreachable mid-stream.
        let rpc_id = loop {
            let payload = channel
                .decode(&receive(&mut socket).await, |bytes| {
                    use std::io::Read;
                    let json = if bytes.starts_with(b"ONW1") {
                        let mut json = Vec::new();
                        flate2::read::GzDecoder::new(&bytes[4..]).read_to_end(&mut json)?;
                        json
                    } else {
                        bytes.to_vec()
                    };
                    Ok(serde_json::from_slice::<Value>(&json)?)
                })
                .unwrap();
            match payload["type"].as_str().unwrap() {
                "rpc" => {
                    assert_eq!(payload["request"]["type"], "attach_subscribe");
                    break payload["id"].as_str().unwrap().to_string();
                }
                "ping" => continue,
                other => panic!("unexpected relayed frame: {other}"),
            }
        };
        let init = json!({
            "type": "attach_stream_init",
            "data": "",
            "end_offset": 0,
            "running": true,
            "bracketed_paste_mode": false,
        });
        let frame = channel
            .encode(
                &serde_json::to_vec(&json!({
                    "type": "rpc_stream_frame",
                    "id": rpc_id,
                    "response": init,
                    "done": false,
                }))
                .unwrap(),
            )
            .unwrap();
        socket.send(Message::Binary(frame.into())).await.unwrap();

        // Let the init frame reach the CLI, then drop the node connection.
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(socket);

        let output = timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
        )
        .await
        .expect("relayed attach hung after the owning node vanished")
        .unwrap();
        assert!(
            !output.status.success(),
            "an interrupted attach must fail loudly"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stdout.contains("has ended"),
            "fabricated completion printed to stdout: {stdout}"
        );
        assert!(
            stderr.contains("not connected") || stderr.contains("closed before completion"),
            "interruption reason missing; stderr: {stderr}"
        );
        assert!(wait_for_no_nodes(port, 3).await);
    });
}

#[test]
fn browser_sessions_still_work_and_machine_bearers_do_not() {
    let tmp = make_tmp_dir("security_browser_auth");
    let port = pick_free_port();
    let password = "isolated-p3-browser-password";
    let _daemon = start_daemon_http_authenticated(&tmp, port, password);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let client = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        for bearer in [None, Some("obsolete-api-key-looking-token")] {
            let mut request = client.get(format!("{base}/api/nodes"));
            if let Some(bearer) = bearer {
                request = request.bearer_auth(bearer);
            }
            assert_eq!(
                request.send().await.unwrap().status(),
                reqwest::StatusCode::UNAUTHORIZED
            );
        }
        let response = client
            .post(format!("{base}/api/auth/login"))
            .header("content-type", "application/json")
            .body(json!({"password":password}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let cookie = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        let token = body["token"].as_str().unwrap();
        assert_eq!(
            client
                .get(format!("{base}/api/nodes"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{base}/api/nodes"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            client
                .post(format!("{base}/api/auth/logout"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        assert_eq!(
            client
                .get(format!("{base}/api/nodes"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    });
}

#[test]
fn primary_rejects_invalid_proof_weak_identity_and_oversized_handshake() {
    let tmp = make_tmp_dir("security_invalid_proof");
    let port = pick_free_port();
    let _daemon = start_daemon_http(&tmp, port);
    let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    register(&tmp, "worker", &key);
    let primary = id_pub_key(&tmp);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (mut socket,_,_) = introduced(port,&key,"worker",&primary).await;
        send(&mut socket,json!({"type":"join","signature":crypto::b64_encode(&[0;64])})).await;
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"],"error");
        let other = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
        let (mut socket,_,signature) = introduced(port,&other,"worker",&primary).await;
        send(&mut socket,json!({"type":"join","signature":signature})).await;
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"],"error");
        let (mut socket,mut channel,proof) = introduced(port,&key,"worker",&primary).await;
        prove(&mut socket,&mut channel,&proof).await;
        let ready = channel.encode(&serde_json::to_vec(&json!({"type":"ready","transcript_hash":vec![0;32]})).unwrap()).unwrap();
        socket.send(Message::Binary(ready.into())).await.unwrap();
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"],"error");
        for value in [
            json!({"type":"hello","version":2,"name":"worker","public_key":format!("ed25519 {}",crypto::b64_encode(&[0;32])),"nonce":vec![0;32]}),
            json!({"type":"hello","version":2,"name":"x".repeat(5000),"public_key":crypto::public_key_line(&key),"nonce":vec![0;32]}),
        ] {
            let (mut socket,_) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/nodes/join")).await.unwrap();
            send(&mut socket,value).await;
            let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
            assert_eq!(error["type"],"error");
        }
        // Entirely idle upgraded sockets are bounded too.
        let (mut socket,_) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/api/nodes/join")).await.unwrap();
        let error: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
        assert_eq!(error["type"],"error"); assert_empty(port).await;
    });
}

#[test]
fn configuration_mutations_fence_handshakes_from_challenge_and_readiness() {
    for (operation, proof_sent) in ["disable", "update", "rm"]
        .into_iter()
        .flat_map(|op| [(op, false), (op, true)])
    {
        let tmp = make_tmp_dir(&format!("security_config_fence_{operation}_{proof_sent}"));
        let port = pick_free_port();
        let _daemon = start_daemon_http(&tmp, port);
        let key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
        register(&tmp, "worker", &key);
        let primary = id_pub_key(&tmp);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (mut stale, mut channel, signature) =
                introduced(port, &key, "worker", &primary).await;
            if proof_sent {
                prove(&mut stale, &mut channel, &signature).await;
            }
            assert_empty(port).await;
            let mutate = |args: &[&str]| {
                let output = oly_cmd(&tmp).args(args).output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            match operation {
                "disable" => {
                    mutate(&["node", "disable", "worker"]);
                    mutate(&["node", "enable", "worker"]);
                }
                "update" => {
                    let replacement = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
                    mutate(&[
                        "node",
                        "update",
                        "worker",
                        "--key",
                        &crypto::public_key_line(&replacement),
                    ]);
                    mutate(&[
                        "node",
                        "update",
                        "worker",
                        "--key",
                        &crypto::public_key_line(&key),
                    ]);
                }
                "rm" => {
                    mutate(&["node", "rm", "worker"]);
                    register(&tmp, "worker", &key);
                }
                _ => unreachable!(),
            }
            // Even restoration of the same name/key cannot revive an old
            // transcript, whether it paused before Join or encrypted Ready.
            if !proof_sent {
                prove(&mut stale, &mut channel, &signature).await;
            }
            ready(&mut stale, &mut channel).await;
            let error = channel
                .decode(
                    &receive(&mut stale).await,
                    crypto::decode_handshake::<Value>,
                )
                .unwrap();
            assert_eq!(error["type"], "error");
            assert_empty(port).await;
            let (mut fresh, mut fresh_channel, signature) =
                introduced(port, &key, "worker", &primary).await;
            prove(&mut fresh, &mut fresh_channel, &signature).await;
            ready(&mut fresh, &mut fresh_channel).await;
            let joined = fresh_channel
                .decode(
                    &receive(&mut fresh).await,
                    crypto::decode_handshake::<Value>,
                )
                .unwrap();
            assert_eq!(joined["type"], "joined");
            assert!(wait_for_node_connected(port, "worker", 2).await);
        });
    }
}

#[test]
fn real_connector_timeout_and_disable_interrupt_stalled_handshake() {
    for cancel in [false, true] {
        let tmp = make_tmp_dir(if cancel {
            "security_cancel"
        } else {
            "security_timeout"
        });
        let _daemon = start_daemon(&tmp);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (hello_tx, hello_rx) = tokio::sync::oneshot::channel();
        let peer = rt.spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            assert!(matches!(socket.next().await, Some(Ok(Message::Binary(_)))));
            hello_tx.send(()).unwrap();
            // Never send Challenge; the real connector must stop waiting.
            let close_deadline = if cancel { 3 } else { 7 };
            let closed = timeout(Duration::from_secs(close_deadline), socket.next())
                .await
                .expect("stalled handshake socket was not closed within its deadline");
            assert!(
                !matches!(closed, Some(Ok(Message::Binary(_)))),
                "connector sent an application frame before Challenge"
            );
            if cancel {
                assert!(
                    timeout(Duration::from_millis(1300), listener.accept())
                        .await
                        .is_err(),
                    "disabled connector retried"
                );
            }
        });
        let pin = crypto::public_key_line(&ed25519_dalek::SigningKey::from_bytes(
            &crypto::fresh_nonce(),
        ));
        let output = oly_cmd(&tmp)
            .args([
                "node",
                "add",
                "primary",
                "--role",
                "primary",
                "--as",
                "worker",
                "--key",
                &pin,
                "--url",
                &format!("http://127.0.0.1:{port}"),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        rt.block_on(async {
            timeout(Duration::from_secs(3), hello_rx)
                .await
                .unwrap()
                .unwrap();
        });
        let started = std::time::Instant::now();
        if cancel {
            let stop = oly_cmd(&tmp)
                .args(["node", "disable", "primary"])
                .output()
                .unwrap();
            assert!(
                stop.status.success(),
                "{}",
                String::from_utf8_lossy(&stop.stderr)
            );
        }
        if cancel {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "disable did not interrupt handshake"
            );
            let node = node_info(&tmp, "primary");
            assert_eq!(node["enabled"], false);
            assert_eq!(node["status"], "disabled");
            assert_eq!(node["key"], pin, "disable deleted the primary pin");
        } else {
            wait_for_connector_error(&tmp, Some("timed out"));
        }
        rt.block_on(async {
            timeout(Duration::from_secs(8), peer)
                .await
                .unwrap()
                .unwrap();
        });
    }
}

#[test]
fn real_connector_wrong_pin_fails_before_success_and_can_update_pin() {
    let primary_tmp = make_tmp_dir("security_pin_primary");
    let secondary_tmp = make_tmp_dir("security_pin_secondary");
    let port = pick_free_port();
    let _primary = start_daemon_http(&primary_tmp, port);
    let _secondary = start_daemon(&secondary_tmp);
    let key = ed25519_dalek::SigningKey::from_bytes(
        &fs::read(secondary_tmp.join("oly/identity_key"))
            .unwrap()
            .try_into()
            .unwrap(),
    );
    register(&primary_tmp, "worker", &key);
    let wrong = crypto::public_key_line(&ed25519_dalek::SigningKey::from_bytes(
        &crypto::fresh_nonce(),
    ));
    let output = oly_cmd(&secondary_tmp)
        .args([
            "node",
            "add",
            "primary",
            "--role",
            "primary",
            "--as",
            "worker",
            "--key",
            &wrong,
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let failed = wait_for_connector_error(&secondary_tmp, None);
    assert_eq!(failed["key"], wrong);
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(assert_empty(port));
    // Correct the pin in place, preserving the resource and advertised name.
    let output = oly_cmd(&secondary_tmp)
        .args([
            "node",
            "update",
            "primary",
            "--key",
            &id_pub_key(&primary_tmp),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        assert!(wait_for_node_connected(port, "worker", 10).await);
    });
}

#[test]
fn identities_and_node_configuration_survive_daemon_restart() {
    let primary_tmp = make_tmp_dir("security_restart_primary");
    let secondary_tmp = make_tmp_dir("security_restart_secondary");
    let port = pick_free_port();
    let primary = start_daemon_http(&primary_tmp, port);
    let secondary = start_daemon(&secondary_tmp);
    let primary_seed = fs::read(primary_tmp.join("oly/identity_key")).unwrap();
    let secondary_seed = fs::read(secondary_tmp.join("oly/identity_key")).unwrap();
    let key = ed25519_dalek::SigningKey::from_bytes(&secondary_seed.clone().try_into().unwrap());
    register(&primary_tmp, "worker", &key);
    let output = oly_cmd(&secondary_tmp)
        .args([
            "node",
            "add",
            "primary",
            "--role",
            "primary",
            "--as",
            "worker",
            "--key",
            &id_pub_key(&primary_tmp),
            "--url",
            &format!("http://127.0.0.1:{port}"),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        assert!(wait_for_node_connected(port, "worker", 10).await);
    });
    let saved_primary = node_config(&secondary_tmp, "primary");
    let saved_secondary = node_config(&primary_tmp, "worker");
    drop(secondary);
    drop(primary);
    let _primary = start_daemon_http(&primary_tmp, port);
    let _secondary = start_daemon(&secondary_tmp);
    assert_eq!(
        fs::read(primary_tmp.join("oly/identity_key")).unwrap(),
        primary_seed
    );
    assert_eq!(
        fs::read(secondary_tmp.join("oly/identity_key")).unwrap(),
        secondary_seed
    );
    assert_eq!(node_config(&secondary_tmp, "primary"), saved_primary);
    assert_eq!(node_config(&primary_tmp, "worker"), saved_secondary);
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        assert!(wait_for_node_connected(port, "worker", 5).await);
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while node_info(&secondary_tmp, "primary")["status"] != "connected" {
        assert!(
            std::time::Instant::now() < deadline,
            "restarted connector never confirmed the primary"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn real_connector_rejects_plaintext_joined_at_each_confirmation_boundary() {
    for boundary in 0..3 {
        let secondary_tmp = make_tmp_dir(&format!("security_fake_{boundary}"));
        let _secondary = start_daemon(&secondary_tmp);
        let primary_key = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
        let pin = crypto::public_key_line(&primary_key);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = rt.spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let hello = match socket.next().await.unwrap().unwrap() { Message::Binary(b) => serde_json::from_slice::<Value>(&b).unwrap(), other => panic!("{other:?}") };
            if boundary > 0 {
                let nonce = crypto::fresh_nonce();
                socket.send(Message::Binary(serde_json::to_vec(&json!({"type":"challenge","nonce":nonce})).unwrap().into())).await.unwrap();
                let proof = match socket.next().await.unwrap().unwrap() { Message::Binary(b) => serde_json::from_slice::<Value>(&b).unwrap(), other => panic!("{other:?}") };
                let secondary = crypto::parse_public_key(hello["public_key"].as_str().unwrap()).unwrap();
                let secondary_nonce = serde_json::from_value(hello["nonce"].clone()).unwrap();
                let transcript = crypto::build_transcript(2,"secondary-proof",hello["name"].as_str().unwrap(),secondary.as_bytes(),primary_key.verifying_key().as_bytes(),&secondary_nonce,&nonce);
                assert!(crypto::verify_transcript(&secondary, proof["signature"].as_str().unwrap(), &transcript));
                if boundary == 2 {
                    let mut channel = crypto::Channel::new(&primary_key,&secondary,&transcript,false).unwrap();
                    let confirm = channel.encode(&serde_json::to_vec(&json!({"type":"key_confirm","transcript_hash":channel.transcript_hash()})).unwrap()).unwrap();
                    socket.send(Message::Binary(confirm.into())).await.unwrap();
                    let ready = match socket.next().await.unwrap().unwrap() { Message::Binary(b) => b, other => panic!("{other:?}") };
                    let ready = channel.decode(&ready,crypto::decode_handshake::<Value>).unwrap();
                    assert_eq!(ready["type"],"ready");
                }
            }
            socket.send(Message::Binary(br#"{"type":"joined"}"#.to_vec().into())).await.unwrap();
            // Even a forged application RPC must never reach local IPC.
            let rpc = json!({"type":"rpc","id":"attack","request":{"type":"start","cmd":"oly_security_should_not_spawn","args":[],"disable_notifications":true}});
            let _ = socket.send(Message::Binary(serde_json::to_vec(&rpc).unwrap().into())).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let output = oly_cmd(&secondary_tmp)
            .args([
                "node",
                "add",
                "primary",
                "--role",
                "primary",
                "--as",
                "worker",
                "--key",
                &pin,
                "--url",
                &format!("http://127.0.0.1:{port}"),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        wait_for_connector_error(&secondary_tmp, None);
        rt.block_on(async {
            timeout(Duration::from_secs(3), peer)
                .await
                .unwrap()
                .unwrap();
        });
        let log = fs::read_to_string(secondary_tmp.join("daemon-stderr.log")).unwrap();
        assert!(!log.contains("joined primary after encrypted confirmation"));
        assert!(
            !log.contains("oly_security_should_not_spawn"),
            "pre-confirmation RPC reached daemon"
        );
        let inventory = oly_cmd(&secondary_tmp)
            .args(["ls", "--json"])
            .output()
            .unwrap();
        assert!(inventory.status.success());
        let sessions: Value = serde_json::from_slice(&inventory.stdout).unwrap();
        assert_eq!(
            sessions["items"],
            json!([]),
            "pre-confirmation RPC created a session"
        );
        assert_eq!(sessions["total"], 0);
    }
}
