/// Integration tests verifying that CLI commands return non-zero exit codes
/// with clear error messages when the daemon is unavailable or a session is
/// not found (Milestone 2 Verification – error paths).
///
/// Each test uses an isolated temporary state directory so it never
/// accidentally connects to a real running daemon.
mod e2e;

use std::{env, fs, path::PathBuf, process::Command};

// Path to the compiled `oly` binary, set by Cargo when building tests.
fn oly_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oly"))
}

/// Build a `Command` for `oly` with a self-contained state directory so the
/// tests never interact with a real daemon.
fn oly_cmd(tmp_dir: &PathBuf) -> Command {
    let mut cmd = Command::new(oly_bin());

    // OLY_STATE_DIR directly overrides resolve_state_dir() — no
    // platform-specific env hacks needed.
    cmd.env("OLY_STATE_DIR", tmp_dir.join("oly"));

    // OLY_SOCKET_NAME prevents accidentally connecting to a real daemon
    // running on the default named pipe (Windows) or socket (Unix).
    cmd.env(
        "OLY_SOCKET_NAME",
        format!("oly-cli-test-{}", tmp_dir.display()),
    );

    cmd
}

fn make_tmp_dir(name: &str) -> PathBuf {
    let dir = env::temp_dir().join(format!("oly_test_{name}"));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

// ---------------------------------------------------------------------------
// oly ls – works without daemon (reads from disk)
// ---------------------------------------------------------------------------

#[test]
fn list_without_daemon_succeeds_gracefully() {
    let tmp = make_tmp_dir("list_no_daemon");
    let output = oly_cmd(&tmp)
        .args(["ls"])
        .output()
        .expect("failed to run oly ls");

    // oly ls reads persisted session metadata from disk and must not require
    // a live daemon – it should always exit 0 even with an empty state dir.
    assert!(
        output.status.success(),
        "`oly ls` should exit 0 without a daemon (reads from disk); stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No sessions") || stdout.contains("ID"),
        "expected list output, got: {stdout}"
    );
    if stdout.contains("No sessions") {
        assert!(
            stdout.contains("Start one with: oly start --detach <cmd>"),
            "expected next-step hint for empty list, got: {stdout}"
        );
    }
}

#[test]
fn list_json_without_daemon_prints_machine_readable_output() {
    let tmp = make_tmp_dir("list_json_no_daemon");
    let output = oly_cmd(&tmp)
        .args(["ls", "--json"])
        .output()
        .expect("failed to run oly ls --json");

    assert!(
        output.status.success(),
        "`oly ls --json` should exit 0 without a daemon; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should be valid JSON");

    assert_eq!(value["items"], serde_json::json!([]));
    assert_eq!(value["total"], serde_json::json!(0));
    assert_eq!(value["offset"], serde_json::json!(0));
    assert_eq!(value["limit"], serde_json::json!(10));
    assert!(
        !stdout.contains("No sessions"),
        "json output should not mix table/hint text into stdout: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// oly --help – short, neutral language
// ---------------------------------------------------------------------------

#[test]
fn help_text_is_neutral_and_simple() {
    let tmp = make_tmp_dir("help_text_neutral");
    let output = oly_cmd(&tmp)
        .args(["--help"])
        .output()
        .expect("failed to run oly --help");

    assert!(output.status.success(), "`oly --help` should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Agent guidance")
            && !stdout.contains("agent-driven")
            && !stdout.contains("agents consuming"),
        "help text should not distinguish users, got: {stdout}"
    );
}

// ---------------------------------------------------------------------------
// oly skill - prints embedded skill markdown
// ---------------------------------------------------------------------------

#[test]
fn skill_command_prints_embedded_markdown() {
    let tmp = make_tmp_dir("skill_markdown");
    let output = oly_cmd(&tmp)
        .args(["skill"])
        .output()
        .expect("failed to run oly skill");

    assert!(output.status.success(), "`oly skill` should exit 0");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = include_str!("../.github/skills/oly/SKILL.md");
    assert_eq!(
        stdout, expected,
        "`oly skill` should print embedded markdown"
    );
}

// ---------------------------------------------------------------------------
// oly stop – daemon unavailable
// ---------------------------------------------------------------------------

#[test]
fn stop_without_daemon_exits_nonzero() {
    let tmp = make_tmp_dir("stop_no_daemon");
    let output = oly_cmd(&tmp)
        .args(["stop", "abc1234"])
        .output()
        .expect("failed to run oly stop");

    assert!(
        !output.status.success(),
        "`oly stop` should exit non-zero when daemon is not running"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error"),
        "expected error message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// oly start – daemon unavailable
// ---------------------------------------------------------------------------

#[test]
fn start_without_daemon_exits_nonzero() {
    let tmp = make_tmp_dir("start_no_daemon");
    let output = oly_cmd(&tmp)
        .args(["start", "--detach", "echo", "hello"])
        .output()
        .expect("failed to run oly start");

    assert!(
        !output.status.success(),
        "`oly start` should exit non-zero when daemon is not running"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error"),
        "expected error message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// oly send – daemon unavailable
// ---------------------------------------------------------------------------

#[test]
fn input_without_daemon_exits_nonzero() {
    let tmp = make_tmp_dir("input_no_daemon");
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "hello"])
        .output()
        .expect("failed to run oly send");

    assert!(
        !output.status.success(),
        "`oly send` should exit non-zero when daemon is not running"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error"),
        "expected error message, got: {stderr}"
    );
}

#[test]
fn input_missing_oly_file_exits_nonzero() {
    let tmp = make_tmp_dir("input_missing_oly_file");
    let missing_file = tmp.join("does-not-exist.txt");
    let chunk = format!("oly-file:{}", missing_file.display());
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", &chunk])
        .output()
        .expect("failed to run oly send");

    assert!(
        !output.status.success(),
        "`oly send {chunk}` should exit non-zero when the source file is missing"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist")
            || stderr.contains("not found")
            || stderr.contains("error"),
        "expected missing-file message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// oly logs – session not found on disk
// ---------------------------------------------------------------------------

#[test]
fn logs_session_not_found_exits_nonzero() {
    let tmp = make_tmp_dir("logs_not_found");
    // Create the state/sessions directory so storage can scan it (empty)
    fs::create_dir_all(tmp.join("oly").join("sessions")).expect("create sessions dir");

    let output = oly_cmd(&tmp)
        .args(["logs", "abc1234"])
        .output()
        .expect("failed to run oly logs");

    assert!(
        !output.status.success(),
        "`oly logs` should exit non-zero for a missing session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("session") || stderr.contains("error"),
        "expected session-not-found message, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// oly send – key validation errors (no daemon required)
//
// Key spec errors are caught before any IPC attempt.
// These tests verify that invalid key: chunks produce clear, non-zero exits.
// ---------------------------------------------------------------------------

#[test]
fn input_unsupported_key_spec_exits_nonzero_with_message() {
    let tmp = make_tmp_dir("input_bad_key");
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "key:foobar"])
        .output()
        .expect("failed to run oly send");

    // "foobar" is not a recognised key name → parse error before IPC.
    assert!(
        !output.status.success(),
        "`oly send key:foobar` should exit non-zero (unsupported key)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported") || stderr.contains("error"),
        "expected unsupported-key message, got: {stderr}"
    );
}

#[test]
fn input_modifier_only_key_exits_nonzero_with_message() {
    let tmp = make_tmp_dir("input_modifier_only");
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "key:ctrl"])
        .output()
        .expect("failed to run oly send");

    // Lone modifier → error.
    assert!(
        !output.status.success(),
        "`oly send key:ctrl` (modifier alone) should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("modifier") || stderr.contains("error"),
        "expected modifier-only error message, got: {stderr}"
    );
}

#[test]
fn input_ctrl_multichar_key_exits_nonzero() {
    let tmp = make_tmp_dir("input_ctrl_multichar");
    // "ctrl+ab" – ctrl sequences require exactly one character.
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "key:ctrl+ab"])
        .output()
        .expect("failed to run oly send");

    assert!(
        !output.status.success(),
        "`oly send key:ctrl+ab` should exit non-zero (multi-char ctrl)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error"),
        "expected error message, got: {stderr}"
    );
}

#[test]
fn input_empty_key_value_exits_nonzero() {
    let tmp = make_tmp_dir("input_empty_key");
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "key:"])
        .output()
        .expect("failed to run oly send");

    assert!(
        !output.status.success(),
        "`oly send key:` should exit non-zero (empty key)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error"),
        "expected error message for empty key, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// oly send – valid key specs exit non-zero only because daemon unavailable
// (verifies that valid specs are accepted by the parser and reach IPC stage)
// ---------------------------------------------------------------------------

#[test]
fn input_valid_ctrl_c_reaches_daemon_check() {
    let tmp = make_tmp_dir("input_valid_ctrl");
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "key:ctrl+c"])
        .output()
        .expect("failed to run oly send");

    // Key is valid → gets past parser → fails at daemon connection.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should exit non-zero when daemon is unavailable"
    );
    assert!(
        !stderr.contains("unsupported") && !stderr.contains("modifier"),
        "error should be about daemon, not key parsing; got: {stderr}"
    );
}

#[test]
fn input_valid_shift_tab_reaches_daemon_check() {
    let tmp = make_tmp_dir("input_valid_shift_tab");
    let output = oly_cmd(&tmp)
        .args(["send", "abc1234", "key:shift+tab"])
        .output()
        .expect("failed to run oly send");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        !stderr.contains("unsupported"),
        "shift+tab should be a valid key spec; got: {stderr}"
    );
}

#[test]
fn input_valid_arrow_keys_reach_daemon_check() {
    for key in &["up", "down", "left", "right"] {
        let tmp = make_tmp_dir(&format!("input_arrow_{key}"));
        let key_chunk = format!("key:{key}");
        let output = oly_cmd(&tmp)
            .args(["send", "abc1234", &key_chunk])
            .output()
            .expect("failed to run oly send");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "`oly send key:{key}` should fail (no daemon), not from key parse error"
        );
        assert!(
            !stderr.contains("unsupported"),
            "arrow key '{key}' should be a valid key spec; got: {stderr}"
        );
    }
}

#[test]
fn input_live_daemon_session_error_is_not_reported_as_unavailable() {
    let tmp = e2e::make_tmp_dir("input_live_daemon_missing_session");
    let _daemon = e2e::start_daemon(&tmp);
    let missing_id = "let x = 123;;";

    let output = e2e::oly_cmd(&tmp)
        .args(["send", missing_id, "hello"])
        .output()
        .expect("failed to run oly send against live daemon");

    assert!(
        !output.status.success(),
        "`oly send` should exit non-zero when the daemon rejects an unknown session"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("session not running: {missing_id}")),
        "expected precise session error from live daemon, got: {stderr}"
    );
    assert!(
        !stderr.contains("daemon is unavailable"),
        "live daemon request failures should not be mislabeled as availability issues: {stderr}"
    );
}
