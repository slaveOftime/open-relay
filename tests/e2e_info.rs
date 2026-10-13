mod e2e;

use e2e::*;
use serde_json::Value;

fn info_json(tmp: &std::path::Path, args: &[&str]) -> Value {
    let output = oly_cmd(tmp).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.stdout.contains(&0x1b),
        "JSON must never contain ANSI styling"
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn e2e_info_inspects_live_and_persisted_sessions_with_safe_output() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("info");
    std::fs::write(
        tmp.join("oly/config.json"),
        r#"{"session_eviction_seconds": 1, "notification_hook": "echo done"}"#,
    )
    .unwrap();
    let daemon = start_daemon(&tmp);

    let empty = oly_cmd(&tmp).args(["info"]).output().unwrap();
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("no sessions found"));

    #[cfg(windows)]
    let shell = ["cmd.exe"];
    #[cfg(not(windows))]
    let shell = ["sh"];
    let start = oly_cmd(&tmp)
        .args([
            "start",
            "--detach",
            "--disable-notifications",
            "--title",
            "full session details",
            "--tag",
            "temp",
            "--tag",
            "info-test",
        ])
        .args(shell)
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stderr)
    );
    let id = String::from_utf8(start.stdout).unwrap().trim().to_string();

    let info = info_json(&tmp, &["info", &id, "--json", "--color", "always"]);
    assert_eq!(info["id"], id);
    assert_eq!(info["title"], "full session details");
    assert_eq!(info["tags"], serde_json::json!(["temp", "info-test"]));
    assert_eq!(info["command"], shell[0]);
    assert_eq!(info["args"], serde_json::json!([]));
    assert_eq!(info["status"], "running");
    assert_eq!(info["notifications_enabled"], false);
    assert!(info["pid"].is_u64());
    assert!(info["exit_code"].is_null());
    assert!(info["journal_incarnation"].is_u64());
    assert!(info["session_dir"].as_str().unwrap().ends_with(&id));
    assert_eq!(info_json(&tmp, &["info", "--json"])["id"], id);

    for mode in ["auto", "never", "always"] {
        let output = oly_cmd(&tmp)
            .args(["info", &id, "--color", mode])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.contains(&0x1b), mode == "always");
        let text = String::from_utf8_lossy(&output.stdout);
        for field in [
            "full session details",
            "Process",
            "Timing & activity",
            "Terminal",
            "Storage & journal",
            "Exit code",
        ] {
            assert!(text.contains(field), "missing {field}: {text}");
        }
    }

    let update = oly_cmd(&tmp)
        .args(["update", &id, "--title", "bad\x1b[31m\nfake row"])
        .output()
        .unwrap();
    assert!(update.status.success());
    let plain = oly_cmd(&tmp)
        .args(["info", &id, "--color", "never"])
        .output()
        .unwrap();
    assert!(plain.status.success());
    assert!(!plain.stdout.contains(&0x1b));
    assert!(String::from_utf8_lossy(&plain.stdout).contains("bad\\u{1b}[31m\\nfake row"));

    for missing in ["missing-session", &id[..3]] {
        let output = oly_cmd(&tmp)
            .args(["info", missing, "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("session not found"));
    }

    let stop = oly_cmd(&tmp)
        .args(["stop", &id, "--grace", "0"])
        .output()
        .unwrap();
    assert!(stop.status.success());
    // A stop acknowledgement precedes the maintenance tick that persists
    // completion. Wait for eviction, which proves we're reading the DB rather
    // than racing shutdown with an unpersisted in-memory final status.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let stopped = loop {
        let info = info_json(&tmp, &["info", &id, "--json"]);
        if info["rows"].is_null() {
            break info;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session was not evicted"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_ne!(stopped["status"], "running");
    assert!(stopped["ended_at"].is_string());

    drop(daemon);
    let restarted_daemon = start_daemon(&tmp);
    let persisted = info_json(&tmp, &["info", &id, "--json"]);
    assert_eq!(persisted["id"], id);
    assert_eq!(persisted["title"], stopped["title"]);
    assert_eq!(persisted["status"], stopped["status"]);
    assert_eq!(persisted["exit_code"], stopped["exit_code"]);
    assert_eq!(persisted["args"], stopped["args"]);
    assert_eq!(
        persisted["journal_incarnation"],
        stopped["journal_incarnation"]
    );
    assert!(
        persisted["rows"].is_null(),
        "evicted geometry is unavailable"
    );
    assert!(persisted["last_total_bytes"].is_u64());
    drop(restarted_daemon);
    std::fs::remove_dir_all(tmp).unwrap();
}
