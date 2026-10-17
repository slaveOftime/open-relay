mod e2e;
use e2e::*;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Output,
    thread,
    time::{Duration, Instant},
};

fn command(tmp: &Path, args: &[&str]) -> Output {
    oly_cmd(tmp).args(args).output().expect("run CLI")
}
fn ok(tmp: &Path, args: &[&str]) -> Output {
    let output = command(tmp, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn info(tmp: &Path, name: &str) -> Value {
    serde_json::from_slice(&ok(tmp, &["node", "info", name, "--json"]).stdout).unwrap()
}
fn list(tmp: &Path, filters: &[&str]) -> Value {
    let mut args = vec!["node", "ls", "--json"];
    args.extend_from_slice(filters);
    serde_json::from_slice(&ok(tmp, &args).stdout).unwrap()
}
fn public_key(tmp: &Path) -> String {
    fs::read_to_string(tmp.join("oly/identity_key.pub"))
        .unwrap()
        .trim()
        .into()
}
fn fake_key(seed: u8) -> String {
    use base64::Engine;
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    format!(
        "ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes())
    )
}
fn wait(tmp: &Path, name: &str, status: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let node = info(tmp, name);
        if node["status"] == status {
            return node;
        }
        assert!(Instant::now() < deadline, "expected {status}: {node}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn unified_inventory_crud_validation_and_disabled_state_survive_restart() {
    let tmp = make_tmp_dir("node_resource");
    let daemon = start_daemon(&tmp);
    let worker_key = fake_key(71);
    let home_key = fake_key(72);
    ok(
        &tmp,
        &[
            "node",
            "add",
            "worker",
            "--role",
            "secondary",
            "--key",
            &worker_key,
        ],
    );
    let url = format!("http://127.0.0.1:{}", pick_free_port());
    ok(
        &tmp,
        &[
            "node", "add", "home", "--role", "primary", "--url", &url, "--as", "laptop", "--key",
            &home_key,
        ],
    );
    let peers = list(&tmp, &[]);
    assert_eq!(peers["total"], 2);
    assert_eq!(peers["items"][0]["name"], "home");
    assert_eq!(peers["items"][0]["as"], "laptop");
    let worker = info(&tmp, "worker");
    assert_eq!(worker["status"], "waiting");
    assert_eq!(worker["url"], Value::Null);
    assert_eq!(worker["as"], Value::Null);
    assert_eq!(worker["key"], worker_key);
    assert!(worker.get("revision").is_none());
    assert_eq!(
        list(&tmp, &["--role", "secondary", "--status", "waiting"])["total"],
        1
    );
    assert_eq!(list(&tmp, &["--status", "connected"])["items"], json!([]));
    let failed = wait(&tmp, "home", "retrying");
    assert!(failed["last_error"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(failed["next_retry_at"].as_str().is_some());

    for args in [
        vec![
            "node",
            "add",
            "worker",
            "--role",
            "secondary",
            "--key",
            &home_key,
        ],
        vec![
            "node",
            "add",
            "duplicate-key",
            "--role",
            "secondary",
            "--key",
            &worker_key,
        ],
        vec![
            "node",
            "add",
            "incomplete",
            "--role",
            "primary",
            "--key",
            &fake_key(73),
        ],
        vec!["node", "update", "home", "--url", "ftp://host"],
        vec!["node", "update", "home", "--as", "bad name"],
        vec!["node", "update", "worker", "--url", "http://host"],
        vec!["node", "update", "worker", "--key", "invalid"],
        vec!["node", "update", "worker", "--key", &home_key],
        vec!["node", "disable", "missing"],
        vec!["node", "rm", "missing"],
    ] {
        let rejected = command(&tmp, &args);
        assert!(!rejected.status.success(), "unexpected success: {args:?}");
        assert!(!rejected.stderr.is_empty(), "missing diagnostic: {args:?}");
    }
    assert_eq!(list(&tmp, &[])["total"], 2);
    assert_eq!(info(&tmp, "worker")["key"], worker_key);
    assert_eq!(info(&tmp, "home")["key"], home_key);
    assert_eq!(info(&tmp, "home")["url"], url);
    assert_eq!(info(&tmp, "home")["as"], "laptop");
    ok(&tmp, &["node", "disable", "worker"]);
    ok(&tmp, &["node", "disable", "home"]);
    // Idempotent enabling/disabling is not an implicit update.
    ok(&tmp, &["node", "disable", "home"]);
    assert_eq!(list(&tmp, &["--status", "disabled"])["total"], 2);
    assert!(!tmp.join("oly/joins.json").exists());
    drop(daemon);

    // Offline attempts must not mutate persisted trust or create new peers.
    assert!(!command(&tmp, &["node", "enable", "home"]).status.success());
    assert!(
        !command(&tmp, &["node", "update", "worker", "--key", &fake_key(74)])
            .status
            .success()
    );
    assert!(
        !command(
            &tmp,
            &[
                "node",
                "add",
                "offline",
                "--role",
                "secondary",
                "--key",
                &fake_key(75)
            ]
        )
        .status
        .success()
    );
    assert!(!command(&tmp, &["node", "rm", "worker"]).status.success());
    let _daemon = start_daemon(&tmp);
    assert_eq!(list(&tmp, &["--status", "disabled"])["total"], 2);
    assert_eq!(info(&tmp, "worker")["key"], worker_key);
    let home = info(&tmp, "home");
    assert_eq!(home["status"], "disabled");
    assert_eq!(home["last_error"], Value::Null);
    assert_eq!(home["next_retry_at"], Value::Null);
    assert_eq!(home["key"], home_key);
    ok(&tmp, &["node", "enable", "worker"]);
    assert_eq!(info(&tmp, "worker")["status"], "waiting");
    ok(&tmp, &["node", "enable", "home"]);
    wait(&tmp, "home", "retrying");
    ok(&tmp, &["node", "rm", "home"]);
    assert!(
        !command(&tmp, &["node", "info", "home", "--json"])
            .status
            .success()
    );
    assert_eq!(list(&tmp, &[])["total"], 1);
}

#[test]
fn inbound_disable_rotation_and_remove_revoke_access_without_stopping_sessions() {
    assert!(
        program_exists("node"),
        "Node.js fixture runtime is required"
    );
    let home = make_tmp_dir("node_home");
    let laptop = make_tmp_dir("node_laptop");
    let port = pick_free_port();
    let _home_daemon = start_daemon_http(&home, port);
    let _laptop_daemon = start_daemon(&laptop);
    let laptop_key = public_key(&laptop);
    let home_key = public_key(&home);
    ok(
        &home,
        &[
            "node",
            "add",
            "laptop",
            "--role",
            "secondary",
            "--key",
            &laptop_key,
        ],
    );
    ok(
        &laptop,
        &[
            "node",
            "add",
            "home",
            "--role",
            "primary",
            "--url",
            &format!("http://127.0.0.1:{port}"),
            "--as",
            "laptop",
            "--key",
            &home_key,
        ],
    );
    wait(&home, "laptop", "connected");
    wait(&laptop, "home", "connected");
    let id = start_session(
        &laptop,
        &[
            "--disable-notifications",
            "node",
            "-e",
            "console.log('node-lifecycle-ready'); setInterval(()=>{}, 1000)",
        ],
    );
    assert!(
        wait_for_log(
            &laptop,
            &id,
            |log| log.contains("node-lifecycle-ready"),
            Duration::from_secs(5)
        )
        .is_some()
    );
    let routed: Value =
        serde_json::from_slice(&ok(&home, &["info", &id, "--node", "laptop", "--json"]).stdout)
            .unwrap();
    assert_eq!(routed["status"], "running");
    // Unified listing must not accidentally make primaries valid --node targets.
    assert!(
        !command(&laptop, &["ls", "--node", "home", "--json"])
            .status
            .success()
    );
    ok(&home, &["node", "enable", "laptop"]);
    assert_eq!(info(&laptop, "home")["status"], "connected");
    ok(&home, &["node", "disable", "laptop"]);
    assert_eq!(info(&home, "laptop")["status"], "disabled");
    wait(&laptop, "home", "retrying");
    assert!(
        !command(&home, &["info", &id, "--node", "laptop", "--json"])
            .status
            .success()
    );
    assert_eq!(info(&home, "laptop")["key"], laptop_key);
    ok(&home, &["node", "enable", "laptop"]);
    wait(&laptop, "home", "connected");
    ok(&home, &["node", "update", "laptop", "--key", &fake_key(76)]);
    assert_eq!(info(&home, "laptop")["status"], "waiting");
    wait(&laptop, "home", "retrying");
    ok(&home, &["node", "update", "laptop", "--key", &laptop_key]);
    wait(&laptop, "home", "connected");
    wait(&home, "laptop", "connected");
    ok(&home, &["node", "rm", "laptop"]);
    assert_eq!(list(&home, &[])["total"], 0);
    wait(&laptop, "home", "retrying");
    assert!(
        !command(&home, &["ls", "--node", "laptop", "--json"])
            .status
            .success()
    );
    let local: Value =
        serde_json::from_slice(&ok(&laptop, &["info", &id, "--json"]).stdout).unwrap();
    assert_eq!(
        local["status"], "running",
        "federation revocation stopped a local process"
    );
    assert!(
        wait_for_log(
            &laptop,
            &id,
            |log| log.contains("node-lifecycle-ready"),
            Duration::from_secs(2)
        )
        .is_some()
    );
    ok(&laptop, &["node", "rm", "home"]);
    ok(&laptop, &["stop", &id, "--grace", "1"]);
}

#[test]
fn concurrent_adds_have_one_authoritative_writer_and_no_lost_entries() {
    let tmp = make_tmp_dir("node_concurrent");
    let _daemon = start_daemon(&tmp);
    let writers: Vec<_> = (0..6)
        .map(|index| {
            let tmp = tmp.clone();
            thread::spawn(move || {
                command(
                    &tmp,
                    &[
                        "node",
                        "add",
                        "one-name",
                        "--role",
                        "secondary",
                        "--key",
                        &fake_key(80 + index),
                    ],
                )
                .status
                .success()
            })
        })
        .collect();
    assert_eq!(
        writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .filter(|success| *success)
            .count(),
        1
    );
    assert_eq!(list(&tmp, &[])["total"], 1);
    let writers: Vec<_> = (0..6)
        .map(|index| {
            let tmp = tmp.clone();
            thread::spawn(move || {
                ok(
                    &tmp,
                    &[
                        "node",
                        "add",
                        &format!("peer-{index}"),
                        "--role",
                        "secondary",
                        "--key",
                        &fake_key(90 + index),
                    ],
                )
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(list(&tmp, &[])["total"], 7);
}
