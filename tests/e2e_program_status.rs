//! Benign OSC 7501 producer; real PTY/IPC, isolated daemon, no agent CLI needed.
mod e2e;
use e2e::*;
use serde_json::Value;
use std::time::{Duration, Instant};

fn info(tmp: &std::path::Path, id: &str) -> Value {
    let out = oly_cmd(tmp).args(["info", id, "--json"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn wait_status(tmp: &std::path::Path, id: &str, state: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = info(tmp, id);
        if value["program_status"]["state"] == state {
            return value;
        }
        assert!(Instant::now() < deadline, "expected {state}, got {value}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
#[test]
fn e2e_program_status_query_and_live_transitions() {
    if !program_exists("python3") {
        eprintln!("skipped: python3 required for raw-PTY OSC producer");
        return;
    }
    let tmp = make_tmp_dir("program_status");
    let daemon = start_daemon(&tmp);
    let helper = r#"
import os, tty
# Read protocol responses directly, without canonical input buffering/echo.
tty.setraw(0)
os.write(1, b'\x1b]7501;?\x07\x1b[c')
expected = b'\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x07'
reply = b''
while len(reply) < len(expected):
    reply += os.read(0, len(expected) - len(reply))
assert reply == expected, repr(reply)
# Consume the fallback DA reply too; OSC detection must precede it.
while not os.read(0, 1) == b'c':
    pass
os.write(1, b'query supported\r\n\x1b]7501;state=working:app=fixture\x07')
for state in [b'blocked:kind=permission:msg=QXBwcm92ZT8=', b'working', b'idle', b'error', b'done']:
    os.read(0, 1)
    os.write(1, b'\x1b]7501;state=' + state + b'\x1b\\')
# Wait before exiting so the live done report is observable.
os.read(0, 1)
"#;
    let out = oly_cmd(&tmp)
        .args([
            "start",
            "--detach",
            "--disable-notifications",
            "--",
            "python3",
            "-c",
            helper,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let initial = wait_status(&tmp, &id, "working");
    assert_eq!(initial["status"], "running");
    assert_eq!(initial["program_status"]["app"], "fixture");
    assert_eq!(initial["input_needed"], false);
    for state in ["blocked", "working", "idle", "error", "done"] {
        let send = oly_cmd(&tmp).args(["send", &id, "x"]).output().unwrap();
        assert!(
            send.status.success(),
            "{}",
            String::from_utf8_lossy(&send.stderr)
        );
        let value = wait_status(&tmp, &id, state);
        assert_eq!(value["status"], "running");
        assert_eq!(value["input_needed"], state == "blocked");
        assert!(
            value["program_status"]["app"].is_null(),
            "reports replace metadata"
        );
        if state == "blocked" {
            assert_eq!(value["program_status"]["kind"], "permission");
            assert_eq!(value["program_status"]["msg"], "Approve?");
            let prompt = oly_cmd(&tmp)
                .args(["logs", &id, "--wait", "prompt", "--timeout", "1s"])
                .output()
                .unwrap();
            assert!(
                prompt.status.success(),
                "{}",
                String::from_utf8_lossy(&prompt.stderr)
            );
            let list = oly_cmd(&tmp).args(["ls", "--json"]).output().unwrap();
            assert!(list.status.success());
            let list: Value = serde_json::from_slice(&list.stdout).unwrap();
            assert_eq!(list["items"][0]["program_status"]["state"], "blocked");
            let plain = oly_cmd(&tmp)
                .args(["info", &id, "--color", "never"])
                .output()
                .unwrap();
            assert!(String::from_utf8_lossy(&plain.stdout).contains("Program status (OSC 7501"));
        }
    }
    let logs = oly_cmd(&tmp).args(["export", &id]).output().unwrap();
    assert!(
        logs.status.success(),
        "{}",
        String::from_utf8_lossy(&logs.stderr)
    );
    assert!(String::from_utf8_lossy(&logs.stdout).contains("query supported"));
    assert!(!String::from_utf8_lossy(&logs.stdout).contains("7501"));
    assert!(
        oly_cmd(&tmp)
            .args(["send", &id, "x"])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = info(&tmp, &id);
        if value["status"] != "running" {
            assert_eq!(value["program_status"]["state"], "done");
            assert_eq!(value["exit_code"], 0);
            break;
        }
        assert!(Instant::now() < deadline, "producer did not exit");
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(daemon);
    std::fs::remove_dir_all(tmp).unwrap();
}

#[cfg(windows)]
#[test]
fn e2e_program_status_conpty_default_enables_pi_style_reports() {
    assert!(
        program_exists("node"),
        "native ConPTY fixture requires Node.js"
    );
    let tmp = make_tmp_dir("program_status_conpty");
    let daemon = start_daemon(&tmp);
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/program-status-agent.cjs"
    );
    let out = oly_cmd(&tmp)
        .args([
            "start",
            "--detach",
            "--disable-notifications",
            "--tag",
            "temp",
            "--tag",
            "osc7501",
            "--",
            "node",
            fixture,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let initial = wait_status(&tmp, &id, "idle");
    assert_eq!(initial["program_status"]["app"], "pi-fixture");
    for state in ["working", "blocked", "error", "idle", "done"] {
        let send = oly_cmd(&tmp).args(["send", &id, "x"]).output().unwrap();
        assert!(
            send.status.success(),
            "{}",
            String::from_utf8_lossy(&send.stderr)
        );
        let value = wait_status(&tmp, &id, state);
        assert_eq!(value["status"], "running");
        assert_eq!(value["input_needed"], state == "blocked");
        if state == "blocked" {
            assert_eq!(value["program_status"]["kind"], "permission");
            assert_eq!(value["program_status"]["msg"], "Approve?");
            let prompt = oly_cmd(&tmp)
                .args(["logs", &id, "--wait", "prompt", "--timeout", "1s"])
                .output()
                .unwrap();
            assert!(
                prompt.status.success(),
                "{}",
                String::from_utf8_lossy(&prompt.stderr)
            );
        }
        let listing = oly_cmd(&tmp).args(["ls", "--json"]).output().unwrap();
        assert!(listing.status.success());
        let listing: Value = serde_json::from_slice(&listing.stdout).unwrap();
        assert_eq!(listing["items"][0]["program_status"]["state"], state);
    }
    let exported = oly_cmd(&tmp).args(["export", &id]).output().unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(String::from_utf8_lossy(&exported.stdout).contains("override=1"));
    assert!(!String::from_utf8_lossy(&exported.stdout).contains("7501"));
    assert!(
        oly_cmd(&tmp)
            .args(["send", &id, "x"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let value = info(&tmp, &id);
        if value["status"] != "running" {
            assert_eq!(value["exit_code"], 0);
            assert_eq!(value["program_status"]["state"], "done");
            break;
        }
        assert!(Instant::now() < deadline, "producer did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(daemon);
    std::fs::remove_dir_all(tmp).unwrap();
}
