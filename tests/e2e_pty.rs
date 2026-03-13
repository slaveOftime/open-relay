mod e2e;

use e2e::*;
use std::{fs, thread::sleep, time::Duration};

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

    const MARKER: &str = "oly_e2e_native_echo_marker";
    send_line(&tmp, &id, &format!("echo {MARKER}"));

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

    const MARKER: &str = "oly_e2e_two_inputs_marker";
    send_text_only(&tmp, &id, &format!("echo {MARKER}"));
    sleep(Duration::from_millis(100));
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

#[test]
fn e2e_powershell_write_host_marker_appears_in_logs() {
    if !program_exists("pwsh") {
        eprintln!("SKIP e2e_powershell_write_host_marker_appears_in_logs: pwsh not found on PATH");
        return;
    }

    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_pwsh");
    let _daemon = start_daemon(&tmp);

    let id = start_session(&tmp, &["pwsh", "-NoLogo", "-NoProfile"]);

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

    send_line(&tmp, &id, "sleep 60");
    sleep(Duration::from_millis(400));
    send_key(&tmp, &id, "ctrl+c");

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

#[test]
#[cfg(not(target_os = "windows"))]
fn e2e_special_keys_reach_pty_without_error() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_special_keys");
    let _daemon = start_daemon(&tmp);

    let id = start_session(&tmp, &["sh"]);

    wait_for_log(
        &tmp,
        &id,
        |log| !log.trim().is_empty(),
        Duration::from_secs(3),
    )
    .expect("sh produced no output within 3 s");

    for key in &["shift+tab", "up", "down", "left", "right", "home", "end"] {
        let key_chunk = format!("key:{key}");
        let output = oly_cmd(&tmp)
            .args(["send", &id, &key_chunk])
            .output()
            .expect("oly send failed to execute");
        assert!(
            output.status.success(),
            "`oly send key:{key}` returned non-zero.\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

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

#[test]
fn e2e_logs_available_after_session_exits() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_logs_after_exit");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "echo", "oly_e2e_exit_output_marker"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "echo oly_e2e_exit_output_marker"];

    let id = start_session(&tmp, cmd);

    const MARKER: &str = "oly_e2e_exit_output_marker";
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

#[test]
fn e2e_attach_completed_session_succeeds_with_piped_stdio_cross_platform() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_attach_completed_piped");
    let _daemon = start_daemon(&tmp);

    const MARKER: &str = "oly_e2e_attach_completed_marker";

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", &format!("echo {MARKER}")];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", &format!("echo {MARKER}")];

    let id = start_session(&tmp, cmd);

    let logged = wait_for_log(
        &tmp,
        &id,
        |log| log.contains(MARKER),
        Duration::from_secs(3),
    );
    assert!(
        logged.is_some(),
        "completed session marker '{MARKER}' not found in logs.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );

    let output = oly_cmd(&tmp)
        .args(["attach", &id])
        .output()
        .expect("`oly attach` failed to execute");

    assert!(
        output.status.success(),
        "`oly attach` exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(MARKER),
        "completed-session attach did not replay marker '{MARKER}'.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Session ") && stdout.contains(" has ended."),
        "completed-session attach did not report child termination.\nstdout:\n{stdout}"
    );
}

#[test]
fn e2e_logs_contain_no_escape_artifacts() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_no_escape_artifacts");
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

    send_line(&tmp, &id, "echo ARTIFACT_CHECK_1");
    send_line(&tmp, &id, "echo ARTIFACT_CHECK_2");

    wait_for_log(
        &tmp,
        &id,
        |log| log.contains("ARTIFACT_CHECK_2"),
        Duration::from_secs(3),
    )
    .expect("second marker not found in logs");

    let log = fetch_logs(&tmp, &id);
    let has_cpr_response =
        log.contains(";1R") || log.contains(";80R") || log.contains("\x1b[") && log.contains("R");
    let has_dsr_query = log.contains("[6n") || log.contains("[5n");

    assert!(
        !has_cpr_response || log.contains("ARTIFACT"),
        "logs contain CPR response artifacts.\nLogs:\n{log}"
    );
    assert!(
        !has_dsr_query,
        "logs contain DSR query artifacts.\nLogs:\n{log}"
    );
}

#[test]
fn e2e_multiple_concurrent_sessions_are_independent() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_multi_session");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let shell: &[&str] = &["cmd.exe"];
    #[cfg(not(target_os = "windows"))]
    let shell: &[&str] = &["sh"];

    let id1 = start_session(&tmp, shell);
    let id2 = start_session(&tmp, shell);

    for id in [&id1, &id2] {
        wait_for_log(
            &tmp,
            id,
            |log| !log.trim().is_empty(),
            Duration::from_secs(3),
        )
        .unwrap_or_else(|| panic!("session {id} produced no output within 3 s"));
    }

    const MARKER_1: &str = "oly_e2e_session_alpha";
    const MARKER_2: &str = "oly_e2e_session_beta";

    send_line(&tmp, &id1, &format!("echo {MARKER_1}"));
    send_line(&tmp, &id2, &format!("echo {MARKER_2}"));

    wait_for_log(
        &tmp,
        &id1,
        |log| log.contains(MARKER_1),
        Duration::from_secs(3),
    )
    .expect("marker 1 not found in session 1 logs");

    wait_for_log(
        &tmp,
        &id2,
        |log| log.contains(MARKER_2),
        Duration::from_secs(3),
    )
    .expect("marker 2 not found in session 2 logs");

    let log1 = fetch_logs(&tmp, &id1);
    let log2 = fetch_logs(&tmp, &id2);

    assert!(
        !log1.contains(MARKER_2),
        "session 1 logs contain session 2 marker — output is leaking.\nLog1:\n{log1}"
    );
    assert!(
        !log2.contains(MARKER_1),
        "session 2 logs contain session 1 marker — output is leaking.\nLog2:\n{log2}"
    );
}

#[test]
fn e2e_high_bandwidth_output_logs_intact() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_high_bandwidth");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "for /L %i in (1,1,500) do @echo LINE_%i"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &[
        "sh",
        "-c",
        "seq 1 500 | while read i; do echo LINE_$i; done",
    ];

    let id = start_session(&tmp, cmd);

    let result = wait_for_log(
        &tmp,
        &id,
        |log| log.contains("LINE_500"),
        Duration::from_secs(10),
    );
    assert!(
        result.is_some(),
        "LINE_500 not found — high-bandwidth output may have been lost.\nLogs (tail):\n{}",
        fetch_logs(&tmp, &id)
    );

    let full_log = oly_cmd(&tmp)
        .args(["logs", &id, "--tail", "600", "--no-truncate"])
        .output()
        .expect("`oly logs --tail 600` failed");
    let full_text = String::from_utf8_lossy(&full_log.stdout);
    assert!(
        full_text.contains("LINE_1"),
        "LINE_1 not found with --tail 600 — early output may have been dropped.\nLogs:\n{full_text}"
    );
}

#[test]
fn e2e_logs_keep_color_preserves_ansi_codes() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_keep_color");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &[
        "cmd.exe",
        "/c",
        "powershell -NoProfile -Command \"Write-Host 'COLOR_TEST' -ForegroundColor Red\"",
    ];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "printf '\\033[31mCOLOR_TEST\\033[0m\\n'"];

    let id = start_session(&tmp, cmd);

    wait_for_log(
        &tmp,
        &id,
        |log| log.contains("COLOR_TEST"),
        Duration::from_secs(5),
    )
    .expect("COLOR_TEST not found in logs");

    let plain = fetch_logs(&tmp, &id);
    assert!(
        plain.contains("COLOR_TEST"),
        "plain logs should contain text.\nLogs:\n{plain}"
    );

    let colored = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--tail",
            "200",
            "--no-truncate",
            "--keep-color",
        ])
        .output()
        .expect("`oly logs --keep-color` failed");
    let colored_text = String::from_utf8_lossy(&colored.stdout);
    assert!(
        colored_text.contains("COLOR_TEST"),
        "colored logs should contain text.\nLogs:\n{colored_text}"
    );
    assert!(
        colored_text.contains('\x1b'),
        "colored logs should contain ANSI escape sequences.\nLogs:\n{colored_text}"
    );
}

#[test]
fn e2e_logs_wait_for_prompt_returns_after_command_exits() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_wait_prompt");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "echo PROMPT_DONE"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "echo PROMPT_DONE"];

    let id = start_session(&tmp, cmd);
    sleep(Duration::from_millis(500));

    let output = oly_cmd(&tmp)
        .args([
            "logs",
            &id,
            "--wait-for-prompt",
            "--timeout",
            "5000",
            "--tail",
            "100",
            "--no-truncate",
        ])
        .output()
        .expect("`oly logs --wait-for-prompt` failed to execute");

    assert!(
        output.status.success(),
        "`oly logs --wait-for-prompt` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("PROMPT_DONE"),
        "expected PROMPT_DONE in wait-for-prompt output.\nstdout:\n{stdout}"
    );
}

#[test]
fn e2e_stopped_session_logs_persist_on_disk() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_persist_after_stop");

    fs::create_dir_all(tmp.join("oly")).expect("create state dir");
    fs::write(
        tmp.join("oly").join("config.json"),
        r#"{ "session_eviction_seconds": 1 }"#,
    )
    .expect("write config override");

    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "echo", "PERSIST_CHECK_MARKER"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "echo PERSIST_CHECK_MARKER"];

    let id = start_session(&tmp, cmd);

    wait_for_log(
        &tmp,
        &id,
        |log| log.contains("PERSIST_CHECK_MARKER"),
        Duration::from_secs(3),
    )
    .expect("marker not found before eviction");

    sleep(Duration::from_secs(3));

    let log = fetch_logs(&tmp, &id);
    assert!(
        log.contains("PERSIST_CHECK_MARKER"),
        "logs should persist on disk after session eviction.\nLogs:\n{log}"
    );
}

#[test]
fn e2e_foreground_start_streams_full_output() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_foreground_full");
    let _daemon = start_daemon(&tmp);

    #[cfg(target_os = "windows")]
    let cmd: &[&str] = &["cmd.exe", "/c", "echo LINE_A& echo LINE_B& echo LINE_C"];
    #[cfg(not(target_os = "windows"))]
    let cmd: &[&str] = &["sh", "-c", "echo LINE_A; echo LINE_B; echo LINE_C"];

    let mut args = vec!["start"];
    args.extend_from_slice(cmd);
    let output = oly_cmd(&tmp)
        .args(&args)
        .output()
        .expect("`oly start` failed to execute");

    assert!(
        output.status.success(),
        "`oly start` exited non-zero.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("LINE_A") && stdout.contains("LINE_B") && stdout.contains("LINE_C"),
        "foreground start should capture all output lines.\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("has ended"),
        "foreground start should report session end.\nstdout:\n{stdout}"
    );
}

#[test]
fn e2e_input_to_nonexistent_session_fails_gracefully() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_bad_session_input");
    let _daemon = start_daemon(&tmp);

    let output = oly_cmd(&tmp)
        .args(["send", "zzz9999", "hello"])
        .output()
        .expect("`oly send` failed to execute");

    assert!(
        !output.status.success(),
        "`oly send` should fail for non-existent session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("error") || stderr.contains("evicted"),
        "expected clear error for non-existent session.\nstderr:\n{stderr}"
    );
}
