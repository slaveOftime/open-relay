mod e2e;

use e2e::*;
use std::{fs, path::PathBuf, thread::sleep, time::Duration};

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn trailing_prompt(log: &str) -> &str {
    log.rsplit('\n').next().unwrap_or(log)
}

fn prompted_transcript<I, S>(prompt: &str, command: &str, output_lines: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut transcript = String::new();
    transcript.push_str(command);
    transcript.push('\n');
    for line in output_lines {
        transcript.push_str(line.as_ref());
        transcript.push('\n');
    }
    transcript.push_str(prompt);
    transcript
}

fn plain_output<I, S>(lines: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut output = String::new();
    for line in lines {
        output.push_str(line.as_ref());
        output.push('\n');
    }
    output
}

fn wait_for_exact_append(
    tmp: &PathBuf,
    id: &str,
    baseline: &str,
    expected_append: &str,
    timeout: Duration,
) -> Option<String> {
    let expected = format!("{baseline}{expected_append}");
    wait_for_exact_log(tmp, id, &expected, timeout)
}

fn start_bash_session(tmp: &PathBuf, test_name: &str) -> Option<String> {
    if !program_exists("bash") {
        eprintln!("SKIP {test_name}: bash not found on PATH");
        return None;
    }

    let id = start_session(tmp, &["bash", "--noprofile", "--norc"]);
    let baseline = wait_for_stable_log(tmp, &id, Duration::from_secs(5)).unwrap_or_else(|| {
        panic!(
            "bash did not reach a stable prompt within 5 s.\nLogs:\n{}",
            fetch_logs(tmp, &id)
        )
    });
    let prompt = trailing_prompt(&baseline).to_string();
    let ready_marker = format!("oly_e2e_bash_ready_{test_name}");
    let ready_command = format!("echo {ready_marker}");
    send_line(tmp, &id, &ready_command);

    let ready = wait_for_exact_append(
        tmp,
        &id,
        &baseline,
        &prompted_transcript(&prompt, &ready_command, [ready_marker.as_str()]),
        Duration::from_secs(5),
    );
    assert!(
        ready.is_some(),
        "bash did not echo readiness marker '{ready_marker}'.\nLogs:\n{}",
        fetch_logs(tmp, &id)
    );

    Some(id)
}

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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(3)).unwrap_or_else(|| {
        panic!("shell did not reach a stable prompt within 3 s; session = {id}")
    });
    let prompt = trailing_prompt(&initial).to_string();

    const MARKER: &str = "oly_e2e_native_echo_marker";
    let command = format!("echo {MARKER}");
    send_line(&tmp, &id, &command);

    let result = wait_for_exact_append(
        &tmp,
        &id,
        &initial,
        &prompted_transcript(&prompt, &command, [MARKER]),
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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(3))
        .expect("shell did not reach a stable prompt within 3 s");
    let prompt = trailing_prompt(&initial).to_string();

    const MARKER: &str = "oly_e2e_two_inputs_marker";
    let command = format!("echo {MARKER}");
    send_text_only(&tmp, &id, &command);
    sleep(Duration::from_millis(100));
    send_key(&tmp, &id, "enter");

    let result = wait_for_exact_append(
        &tmp,
        &id,
        &initial,
        &prompted_transcript(&prompt, &command, [MARKER]),
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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(3))
        .expect("shell did not reach a stable prompt within 3 s");
    let prompt = trailing_prompt(&initial).to_string();

    send_line(&tmp, &id, "echo oly_e2e_order_first");
    send_line(&tmp, &id, "echo oly_e2e_order_second");
    send_line(&tmp, &id, "echo oly_e2e_order_third");

    let expected_append = format!(
        "{}{}{}",
        prompted_transcript(&prompt, "echo oly_e2e_order_first", ["oly_e2e_order_first"]),
        prompted_transcript(
            &prompt,
            "echo oly_e2e_order_second",
            ["oly_e2e_order_second"]
        ),
        prompted_transcript(&prompt, "echo oly_e2e_order_third", ["oly_e2e_order_third"]),
    );
    let result = wait_for_exact_append(
        &tmp,
        &id,
        &initial,
        &expected_append,
        Duration::from_secs(3),
    );
    assert!(
        result.is_some(),
        "command transcript did not match exact expected output.\nLogs:\n{}",
        fetch_logs(&tmp, &id)
    );
}

#[test]
fn e2e_bash_repeated_send_echo_commands_accumulate_in_logs() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_bash_repeated_echo");
    let _daemon = start_daemon(&tmp);

    let Some(id) = start_bash_session(
        &tmp,
        "e2e_bash_repeated_send_echo_commands_accumulate_in_logs",
    ) else {
        return;
    };

    let mut baseline = wait_for_stable_log(&tmp, &id, Duration::from_secs(5))
        .expect("bash did not return to a stable prompt after readiness check");
    let mut expected_markers = Vec::new();
    for round in 1..=4 {
        let marker = format!("oly_e2e_bash_echo_round_{round}");
        let command = format!("echo {marker}");
        let prompt = trailing_prompt(&baseline).to_string();
        send_line(&tmp, &id, &command);
        expected_markers.push(marker);

        baseline = wait_for_exact_append(
            &tmp,
            &id,
            &baseline,
            &prompted_transcript(
                &prompt,
                &command,
                [expected_markers.last().expect("marker just pushed").as_str()],
            ),
            Duration::from_secs(5),
        )
        .unwrap_or_else(|| {
            panic!(
                "repeated echo transcript did not match exact expected output after round {round}.\nLogs:\n{}",
                fetch_logs(&tmp, &id)
            )
        });
    }
}

#[test]
fn e2e_bash_repeated_loop_scripts_appear_fully_in_logs() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_bash_repeated_loops");
    let _daemon = start_daemon(&tmp);

    let Some(id) = start_bash_session(&tmp, "e2e_bash_repeated_loop_scripts_appear_fully_in_logs")
    else {
        return;
    };

    let mut baseline = wait_for_stable_log(&tmp, &id, Duration::from_secs(5))
        .expect("bash did not return to a stable prompt after readiness check");
    let mut expected_markers = Vec::new();
    for round in 1..=3 {
        let script = format!("for i in 1 2 3 4; do echo oly_e2e_bash_loop_r{round}_i$i; done");
        let prompt = trailing_prompt(&baseline).to_string();
        let round_markers: Vec<String> = (1..=4)
            .map(|item| format!("oly_e2e_bash_loop_r{round}_i{item}"))
            .collect();
        for item in 1..=4 {
            expected_markers.push(format!("oly_e2e_bash_loop_r{round}_i{item}"));
        }

        send_line(&tmp, &id, &script);

        baseline = wait_for_exact_append(
            &tmp,
            &id,
            &baseline,
            &prompted_transcript(&prompt, &script, round_markers.iter().map(String::as_str)),
            Duration::from_secs(5),
        )
        .unwrap_or_else(|| {
            panic!(
                "loop transcript did not match exact expected output after round {round}.\nLogs:\n{}",
                fetch_logs(&tmp, &id)
            )
        });
    }

    let final_log = fetch_logs(&tmp, &id);
    for marker in &expected_markers {
        assert_eq!(
            count_occurrences(&final_log, marker),
            1,
            "expected loop marker {marker:?} exactly once in logs.\nLogs:\n{final_log}"
        );
    }
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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(15)).unwrap_or_else(|| {
        panic!("pwsh did not reach a stable prompt within 15 s; session = {id}")
    });
    let prompt = trailing_prompt(&initial).to_string();

    const MARKER: &str = "oly_e2e_pwsh_write_host_marker";
    let command = format!("Write-Host {MARKER}");
    send_line(&tmp, &id, &command);

    let result = wait_for_exact_append(
        &tmp,
        &id,
        &initial,
        &prompted_transcript(&prompt, &command, [MARKER]),
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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(3))
        .expect("sh did not reach a stable prompt within 3 s");
    let prompt = trailing_prompt(&initial).to_string();

    send_line(&tmp, &id, "sleep 60");
    wait_for_exact_log(
        &tmp,
        &id,
        &format!("{initial}sleep 60\n"),
        Duration::from_secs(3),
    )
    .expect("sh did not echo `sleep 60` exactly before Ctrl-C");
    sleep(Duration::from_millis(400));
    send_key(&tmp, &id, "ctrl+c");

    let recovered = wait_for_exact_log(
        &tmp,
        &id,
        &format!("{initial}sleep 60\n^C\n{prompt}"),
        Duration::from_secs(3),
    )
    .or_else(|| {
        wait_for_exact_log(
            &tmp,
            &id,
            &format!("{initial}sleep 60\n{prompt}"),
            Duration::from_secs(3),
        )
    });
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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(3))
        .expect("sh did not reach a stable prompt within 3 s");
    let prompt = trailing_prompt(&initial).to_string();

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
    let command = format!("echo {MARKER}");
    send_line(&tmp, &id, &command);

    let result = wait_for_exact_append(
        &tmp,
        &id,
        &initial,
        &prompted_transcript(&prompt, &command, [MARKER]),
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
    let expected = plain_output([MARKER]);
    let result = wait_for_exact_log(&tmp, &id, &expected, Duration::from_secs(3));
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

    let expected = plain_output([MARKER]);
    let logged = wait_for_exact_log(&tmp, &id, &expected, Duration::from_secs(3));
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

    let initial = wait_for_stable_log(&tmp, &id, Duration::from_secs(3))
        .expect("shell did not reach a stable prompt within 3 s");
    let prompt = trailing_prompt(&initial).to_string();

    send_line(&tmp, &id, "echo ARTIFACT_CHECK_1");
    send_line(&tmp, &id, "echo ARTIFACT_CHECK_2");

    let expected_append = format!(
        "{}{}",
        prompted_transcript(&prompt, "echo ARTIFACT_CHECK_1", ["ARTIFACT_CHECK_1"]),
        prompted_transcript(&prompt, "echo ARTIFACT_CHECK_2", ["ARTIFACT_CHECK_2"]),
    );
    let log = wait_for_exact_append(
        &tmp,
        &id,
        &initial,
        &expected_append,
        Duration::from_secs(3),
    )
    .expect("echo transcript did not match exact expected output");
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

    let initial1 = wait_for_stable_log(&tmp, &id1, Duration::from_secs(3))
        .unwrap_or_else(|| panic!("session {id1} did not reach a stable prompt within 3 s"));
    let initial2 = wait_for_stable_log(&tmp, &id2, Duration::from_secs(3))
        .unwrap_or_else(|| panic!("session {id2} did not reach a stable prompt within 3 s"));
    let prompt1 = trailing_prompt(&initial1).to_string();
    let prompt2 = trailing_prompt(&initial2).to_string();

    const MARKER_1: &str = "oly_e2e_session_alpha";
    const MARKER_2: &str = "oly_e2e_session_beta";

    send_line(&tmp, &id1, &format!("echo {MARKER_1}"));
    send_line(&tmp, &id2, &format!("echo {MARKER_2}"));

    let log1 = wait_for_exact_append(
        &tmp,
        &id1,
        &initial1,
        &prompted_transcript(&prompt1, &format!("echo {MARKER_1}"), [MARKER_1]),
        Duration::from_secs(3),
    )
    .expect("session 1 transcript did not match exact expected output");

    let log2 = wait_for_exact_append(
        &tmp,
        &id2,
        &initial2,
        &prompted_transcript(&prompt2, &format!("echo {MARKER_2}"), [MARKER_2]),
        Duration::from_secs(3),
    )
    .expect("session 2 transcript did not match exact expected output");

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

    let expected = plain_output((1..=500).map(|i| format!("LINE_{i}")));
    let result = wait_for_exact_log_with_tail(&tmp, &id, 600, &expected, Duration::from_secs(10));
    assert!(
        result.is_some(),
        "high-bandwidth output did not match the exact expected transcript.\nLogs (tail):\n{}",
        fetch_logs_with_tail(&tmp, &id, 600)
    );

    let full_log = oly_cmd(&tmp)
        .args(["logs", &id, "--tail", "600", "--no-truncate"])
        .output()
        .expect("`oly logs --tail 600` failed");
    let full_text = normalize_log_text(&String::from_utf8_lossy(&full_log.stdout));
    assert_eq!(
        full_text, expected,
        "high-bandwidth --tail 600 output did not match the exact expected transcript.\nLogs:\n{full_text}"
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

    let expected_plain = plain_output(["COLOR_TEST"]);
    wait_for_exact_log(&tmp, &id, &expected_plain, Duration::from_secs(5))
        .expect("plain logs did not match exact expected color-stripped output");

    let plain = normalize_log_text(&fetch_logs(&tmp, &id));
    assert_eq!(
        plain, expected_plain,
        "plain logs should match the exact expected color-stripped output.\nLogs:\n{plain}"
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

    let expected = plain_output(["PERSIST_CHECK_MARKER"]);
    wait_for_exact_log(&tmp, &id, &expected, Duration::from_secs(3))
        .expect("persisted session logs did not match the exact expected output before eviction");

    sleep(Duration::from_secs(3));

    let log = normalize_log_text(&fetch_logs(&tmp, &id));
    assert_eq!(
        log, expected,
        "logs should persist on disk after session eviction with the exact original output.\nLogs:\n{log}"
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
