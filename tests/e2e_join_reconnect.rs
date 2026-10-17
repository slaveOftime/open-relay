mod e2e;
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

fn id_pub_key(tmp: &Path) -> String {
    fs::read_to_string(tmp.join("oly/identity_key.pub"))
        .unwrap()
        .trim()
        .into()
}

#[cfg(target_os = "linux")]
fn default_cmd(tmp: &Path) -> std::process::Command {
    let mut cmd = oly_cmd(tmp);
    cmd.env_remove("OLY_SOCKET_NAME");
    // Exercise real default routing without inheriting the operator's distro.
    cmd.env("WSL_DISTRO_NAME", "oly-isolation-test");
    cmd
}

#[cfg(target_os = "linux")]
struct DefaultDaemon {
    child: std::process::Child,
    tmp: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl DefaultDaemon {
    fn start(tmp: &Path, port: Option<u16>) -> Self {
        let mut cmd = default_cmd(tmp);
        cmd.args(["daemon", "start", "--foreground-internal", "--no-auth"]);
        if let Some(port) = port {
            cmd.args(["--port", &port.to_string()]);
        } else {
            cmd.arg("--no-http");
        }
        let mut daemon = Self {
            child: cmd
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(fs::File::create(tmp.join("daemon-stderr.log")).unwrap())
                .spawn()
                .unwrap(),
            tmp: tmp.into(),
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited at startup"
            );
            let status = default_cmd(tmp)
                .args(["daemon", "status", "--json"])
                .output()
                .unwrap();
            if let Ok(value) = serde_json::from_slice::<Value>(&status.stdout)
                && value["daemon_pid"] == daemon.child.id()
            {
                return daemon;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "default daemon did not answer its own IPC"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for DefaultDaemon {
    fn drop(&mut self) {
        let _ = default_cmd(&self.tmp).args(["daemon", "stop"]).output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// No test socket override: separate state directories must reach their own
/// daemons, and the secondary must appear on the actual primary's registry.
#[cfg(target_os = "linux")]
#[test]
fn default_linux_ipc_isolates_daemons_and_joins_the_correct_primary() {
    let primary_tmp = make_tmp_dir("default_join_primary");
    let secondary_tmp = make_tmp_dir("default_join_secondary");
    let port = pick_free_port();
    let primary = DefaultDaemon::start(&primary_tmp, Some(port));
    let secondary = DefaultDaemon::start(&secondary_tmp, None);
    assert_ne!(primary.child.id(), secondary.child.id());
    let accept = default_cmd(&primary_tmp)
        .args([
            "node",
            "add",
            "worker",
            "--role",
            "secondary",
            "--key",
            &id_pub_key(&secondary_tmp),
        ])
        .output()
        .unwrap();
    assert!(
        accept.status.success(),
        "{}",
        String::from_utf8_lossy(&accept.stderr)
    );
    let join = default_cmd(&secondary_tmp)
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
        join.status.success(),
        "{}",
        String::from_utf8_lossy(&join.stderr)
    );
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(wait_for_node_connected(port, "worker", 5)));
    // The primary registry insertion precedes the connector's sealed Joined
    // response; wait for the secondary's own live state rather than racing it.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let ls = default_cmd(&secondary_tmp)
            .args(["node", "ls", "--json"])
            .output()
            .unwrap();
        assert!(ls.status.success());
        let nodes: Value = serde_json::from_slice(&ls.stdout).unwrap();
        assert_eq!(nodes["total"], 1);
        assert_eq!(nodes["items"][0]["name"], "primary");
        assert_eq!(nodes["items"][0]["role"], "primary");
        assert_eq!(nodes["items"][0]["as"], "worker");
        if nodes["items"][0]["status"] == "connected" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "connector not connected: {nodes}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let primary_ls = default_cmd(&primary_tmp)
        .args(["node", "ls", "--json"])
        .output()
        .unwrap();
    assert!(primary_ls.status.success());
    let nodes: Value = serde_json::from_slice(&primary_ls.stdout).unwrap();
    assert_eq!(nodes["total"], 1);
    assert_eq!(nodes["items"][0]["name"], "worker");
    assert_eq!(nodes["items"][0]["role"], "secondary");
    assert_eq!(nodes["items"][0]["status"], "connected");
}

#[test]
fn updating_and_removing_a_primary_leaves_no_orphan_connector() {
    let primary_tmp = make_tmp_dir("replace_join_primary");
    let secondary_tmp = make_tmp_dir("replace_join_secondary");
    let port = pick_free_port();
    let _primary = start_daemon_http(&primary_tmp, port);
    let _secondary = start_daemon(&secondary_tmp);
    let accepted = oly_cmd(&primary_tmp)
        .args([
            "node",
            "add",
            "worker",
            "--role",
            "secondary",
            "--key",
            &id_pub_key(&secondary_tmp),
        ])
        .output()
        .unwrap();
    assert!(accepted.status.success());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let url = format!("http://127.0.0.1:{port}");
    let added = oly_cmd(&secondary_tmp)
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
            &url,
        ])
        .output()
        .unwrap();
    assert!(added.status.success());
    assert!(rt.block_on(wait_for_node_connected(port, "worker", 10)));
    for _ in 0..3 {
        // Replacing a live connector must cancel both its socket and retry
        // task. This path intentionally cannot upgrade to federation.
        let changed = oly_cmd(&secondary_tmp)
            .args([
                "node",
                "update",
                "primary",
                "--url",
                &format!("{url}/unavailable"),
            ])
            .output()
            .unwrap();
        assert!(changed.status.success());
        assert!(rt.block_on(wait_for_no_nodes(port, 5)));
        let restored = oly_cmd(&secondary_tmp)
            .args(["node", "update", "primary", "--url", &url])
            .output()
            .unwrap();
        assert!(restored.status.success());
        assert!(rt.block_on(wait_for_node_connected(port, "worker", 10)));
    }
    // Permanent removal, unlike disable, must delete the saved resource.
    let removed = oly_cmd(&secondary_tmp)
        .args(["node", "rm", "primary"])
        .output()
        .unwrap();
    assert!(removed.status.success());
    assert!(rt.block_on(wait_for_no_nodes(port, 5)));
    // Observe the whole backoff window, not just one empty snapshot: detached
    // old connectors must never publish the removed node again.
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            let response = client
                .get(format!("http://127.0.0.1:{port}/api/nodes"))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            let nodes: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
            assert_eq!(nodes, json!([]), "removed connector reappeared");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let ls = oly_cmd(&secondary_tmp)
        .args(["node", "ls", "--json"])
        .output()
        .unwrap();
    assert!(ls.status.success());
    let nodes: Value = serde_json::from_slice(&ls.stdout).unwrap();
    assert_eq!(nodes["items"], json!([]));
    assert_eq!(nodes["total"], 0);
}

async fn receive(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> Vec<u8> {
    loop {
        match timeout(Duration::from_secs(7), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        {
            Message::Binary(bytes) => return bytes.to_vec(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected handshake frame: {other:?}"),
        }
    }
}

async fn send_json(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    value: Value,
) {
    socket
        .send(Message::Binary(serde_json::to_vec(&value).unwrap().into()))
        .await
        .unwrap();
}

async fn mock_primary_handshake(
    listener: &tokio::net::TcpListener,
    primary: &ed25519_dalek::SigningKey,
) -> (
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    crypto::Channel,
) {
    let (tcp, _) = timeout(Duration::from_secs(55), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
    let hello: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["version"], 2);
    assert_eq!(hello["name"], "worker");
    let secondary = crypto::parse_public_key(hello["public_key"].as_str().unwrap()).unwrap();
    let secondary_nonce = serde_json::from_value(hello["nonce"].clone()).unwrap();
    let nonce = crypto::fresh_nonce();
    send_json(&mut socket, json!({"type":"challenge","nonce":nonce})).await;
    let transcript = crypto::build_transcript(
        2,
        "secondary-proof",
        "worker",
        secondary.as_bytes(),
        primary.verifying_key().as_bytes(),
        &secondary_nonce,
        &nonce,
    );
    let proof: Value = serde_json::from_slice(&receive(&mut socket).await).unwrap();
    assert!(crypto::verify_transcript(
        &secondary,
        proof["signature"].as_str().unwrap(),
        &transcript
    ));
    let mut channel = crypto::Channel::new(primary, &secondary, &transcript, false).unwrap();
    let confirmation = channel
        .encode(
            &serde_json::to_vec(
                &json!({"type":"key_confirm","transcript_hash":channel.transcript_hash()}),
            )
            .unwrap(),
        )
        .unwrap();
    socket
        .send(Message::Binary(confirmation.into()))
        .await
        .unwrap();
    let ready = channel
        .decode(
            &receive(&mut socket).await,
            crypto::decode_handshake::<Value>,
        )
        .unwrap();
    assert_eq!(ready["type"], "ready");
    let joined = channel.encode(br#"{"type":"joined"}"#).unwrap();
    socket.send(Message::Binary(joined.into())).await.unwrap();
    (socket, channel)
}

/// With two simultaneous outbound joins, abandon/reconnect a half-open link
/// while the healthy primary keeps its registration and can proxy local RPCs.
#[test]
fn two_primary_joins_recover_independently_when_one_stops_authenticated_traffic() {
    let tmp = make_tmp_dir("silent_primary_reconnect");
    let _daemon = start_daemon(&tmp);
    let healthy_tmp = make_tmp_dir("healthy_other_primary");
    let healthy_port = pick_free_port();
    let _healthy = start_daemon_http(&healthy_tmp, healthy_port);
    let accepted = oly_cmd(&healthy_tmp)
        .args([
            "node",
            "add",
            "stable",
            "--role",
            "secondary",
            "--key",
            &id_pub_key(&tmp),
        ])
        .output()
        .unwrap();
    assert!(accepted.status.success());
    let healthy_join = oly_cmd(&tmp)
        .args([
            "node",
            "add",
            "healthy-primary",
            "--role",
            "primary",
            "--as",
            "stable",
            "--key",
            &id_pub_key(&healthy_tmp),
            "--url",
            &format!("http://127.0.0.1:{healthy_port}"),
        ])
        .output()
        .unwrap();
    assert!(healthy_join.status.success());
    let rt = tokio::runtime::Runtime::new().unwrap();
    assert!(rt.block_on(wait_for_node_connected(healthy_port, "stable", 10)));
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let primary = ed25519_dalek::SigningKey::from_bytes(&crypto::fresh_nonce());
    let pin = crypto::public_key_line(&primary);
    let peer = rt.spawn(async move {
        let (mut socket, _) = mock_primary_handshake(&listener, &primary).await;
        let started = tokio::time::Instant::now();
        let mut pings = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = pings.tick() => {
                    // This primary's failure must not freeze the other join.
                    assert!(wait_for_node_connected(healthy_port, "stable", 1).await);
                    let response = reqwest::Client::builder().timeout(Duration::from_secs(2)).build().unwrap()
                        .get(format!("http://127.0.0.1:{healthy_port}/api/sessions?node=stable"))
                        .send().await.unwrap();
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                    let page: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
                    assert_eq!(page["items"], json!([]));
                    assert_eq!(page["total"], 0);
                    if socket.send(Message::Ping(vec![1].into())).await.is_err() { break; }
                }
                frame = socket.next() => {
                    if matches!(frame, None | Some(Err(_)) | Some(Ok(Message::Close(_)))) { break; }
                }
                _ = tokio::time::sleep_until(started + Duration::from_secs(52)) => panic!("secondary did not close the silent authenticated channel"),
            }
        }
        assert!(started.elapsed() >= Duration::from_secs(44));
        let (mut socket, mut channel) = mock_primary_handshake(&listener, &primary).await;
        let ping = channel.encode(br#"{"type":"ping"}"#).unwrap();
        socket.send(Message::Binary(ping.into())).await.unwrap();
        let pong = channel.decode(&receive(&mut socket).await, crypto::decode_handshake::<Value>).unwrap();
        assert_eq!(pong["type"], "pong");
    });
    let join = oly_cmd(&tmp)
        .args([
            "node",
            "add",
            "silent-primary",
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
        join.status.success(),
        "{}",
        String::from_utf8_lossy(&join.stderr)
    );
    rt.block_on(async {
        timeout(Duration::from_secs(60), peer)
            .await
            .unwrap()
            .unwrap();
    });
    assert!(rt.block_on(wait_for_node_connected(healthy_port, "stable", 1)));
    let log = fs::read_to_string(tmp.join("daemon-stderr.log")).unwrap();
    assert!(log.contains("primary keepalive timeout"));
    assert_eq!(
        log.matches("connected to primary after encrypted confirmation")
            .count(),
        3, // healthy primary once, stalled primary initially and after recovery
    );
    let healthy_log = fs::read_to_string(healthy_tmp.join("daemon-stderr.log")).unwrap();
    assert_eq!(
        healthy_log
            .matches("secondary node connected after encrypted readiness")
            .count(),
        1
    );
    assert!(!healthy_log.contains("secondary node disconnected"));
}
