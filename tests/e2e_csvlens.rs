mod e2e;

use e2e::*;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn csvlens_install_root() -> PathBuf {
    repo_root().join("target-tests").join("csvlens-toolchain")
}

fn csvlens_binary_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        csvlens_install_root().join("bin").join("csvlens.exe")
    }

    #[cfg(not(target_os = "windows"))]
    {
        csvlens_install_root().join("bin").join("csvlens")
    }
}

fn ensure_csvlens_installed() -> PathBuf {
    let binary = csvlens_binary_path();
    if binary.exists() {
        return binary;
    }

    let repo_root = repo_root();
    let cargo_target_dir = repo_root.join("target-tests").join("cargo-install-target");
    let cargo_tmp_dir = repo_root.join("target-tests").join("cargo-install-tmp");
    fs::create_dir_all(csvlens_install_root()).expect("create csvlens install root");
    fs::create_dir_all(&cargo_target_dir).expect("create cargo install target dir");
    fs::create_dir_all(&cargo_tmp_dir).expect("create cargo install temp dir");

    let output = Command::new("cargo")
        .args([
            "install",
            "csvlens",
            "--version",
            "0.15.1",
            "--root",
            csvlens_install_root()
                .to_str()
                .expect("csvlens install root is valid UTF-8"),
            "--locked",
        ])
        .env("CARGO_TARGET_DIR", &cargo_target_dir)
        .env("TEMP", &cargo_tmp_dir)
        .env("TMP", &cargo_tmp_dir)
        .stdin(Stdio::null())
        .output()
        .expect("`cargo install csvlens` failed to execute");

    assert!(
        output.status.success(),
        "`cargo install csvlens` exited non-zero.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(binary.exists(), "csvlens binary missing after install");
    binary
}

fn wait_for_session_status(tmp: &PathBuf, id: &str, expected: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let output = oly_cmd(tmp)
            .args(["ls"])
            .stdin(Stdio::null())
            .output()
            .expect("`oly ls` failed to execute");
        assert!(
            output.status.success(),
            "`oly ls` exited non-zero.\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout
            .lines()
            .any(|line| line.contains(id) && line.contains(expected))
        {
            return;
        }

        sleep(Duration::from_millis(250));
    }

    panic!(
        "session {id} did not reach status {expected:?} within {} s.\n`oly ls`:\n{}",
        timeout.as_secs(),
        String::from_utf8_lossy(
            &oly_cmd(tmp)
                .args(["ls"])
                .stdin(Stdio::null())
                .output()
                .expect("`oly ls` failed during panic context")
                .stdout
        )
    );
}

fn write_csv_fixture(path: &Path) {
    fs::write(
        path,
        "name,city,score\nalice,seattle,10\nbob,portland,20\ncara,austin,30\n",
    )
    .expect("write csvlens fixture");
}

#[test]
fn e2e_csvlens_live_logs_mark_message_and_ctrl_e_keeps_session_running() {
    let _lock = E2E_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tmp = make_tmp_dir("e2e_csvlens");
    let _daemon = start_daemon(&tmp);

    let csvlens = ensure_csvlens_installed();
    let csv = tmp.join("people.csv");
    write_csv_fixture(&csv);

    let csvlens_str = csvlens.display().to_string();
    let csv_str = csv.display().to_string();
    let id = start_session(&tmp, &[csvlens_str.as_str(), csv_str.as_str()]);

    let initial = wait_for_log(
        &tmp,
        &id,
        |log| {
            log.contains("name")
                && log.contains("alice")
                && log.contains("bob")
                && log.contains("cara")
        },
        Duration::from_secs(15),
    )
    .unwrap_or_else(|| {
        panic!(
            "csvlens did not render the table within 15 s.\nLogs:\n{}",
            fetch_logs(&tmp, &id)
        )
    });
    assert!(
        initial.contains("score"),
        "expected csvlens header row in logs.\nLogs:\n{initial}"
    );

    send_key(&tmp, &id, "down");
    sleep(Duration::from_millis(200));
    send_key(&tmp, &id, "down");
    sleep(Duration::from_millis(400));
    send_text_only(&tmp, &id, "m");

    let marked = wait_for_log(
        &tmp,
        &id,
        |log| log.contains("Marked line 3"),
        Duration::from_secs(5),
    )
    .unwrap_or_else(|| {
        panic!(
            "csvlens did not report the marked row.\nLogs:\n{}",
            fetch_logs(&tmp, &id)
        )
    });
    assert!(
        marked.contains("Marked line 3"),
        "expected marked-row status message.\nLogs:\n{marked}"
    );

    send_key(&tmp, &id, "ctrl+e");
    sleep(Duration::from_millis(700));
    wait_for_session_status(&tmp, &id, "running", Duration::from_secs(3));

    let after_ctrl_e = fetch_logs(&tmp, &id);
    assert!(
        after_ctrl_e.contains("Marked line 3"),
        "expected csvlens status line to remain available after Ctrl-E.\nLogs:\n{after_ctrl_e}"
    );

    send_text_only(&tmp, &id, "q");
    wait_for_session_status(&tmp, &id, "stopped", Duration::from_secs(5));

    let final_logs = fetch_logs(&tmp, &id);
    assert!(
        final_logs.trim().is_empty(),
        "expected logs to be empty after csvlens quits and clears the alternate screen.\nLogs:\n{final_logs}"
    );
}
