mod e2e;

use e2e::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    fs,
    io::{Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

// Own only this attach child; a failing assertion must not leave it running
// (or keep the Windows test executable locked during the next build).
struct AttachChild(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for AttachChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn e2e_attach_multiline_terminal_paste_does_not_submit() {
    assert!(
        program_exists("node"),
        "node is required for the paste probe"
    );
    let tmp = make_tmp_dir("attach_multiline_paste");
    let _daemon = start_daemon(&tmp);
    let report = tmp.join("paste-report.json");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/paste_probe.cjs");
    let id = start_session(
        &tmp,
        &[
            "--disable-notifications",
            "--tag",
            "temp",
            "--tag",
            "paste-test",
            "node",
            fixture.to_str().unwrap(),
            report.to_str().unwrap(),
        ],
    );
    assert!(
        wait_for_log(
            &tmp,
            &id,
            |log| log.contains("PASTE_READY"),
            Duration::from_secs(10)
        )
        .is_some(),
        "probe did not become ready"
    );

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open outer PTY");
    let mut cmd = CommandBuilder::new(oly_bin());
    cmd.args(["attach", &id]);
    cmd.env("OLY_STATE_DIR", tmp.join("oly"));
    cmd.env("OLY_SOCKET_NAME", socket_name_for_tmp(&tmp));
    let mut child = AttachChild(pair.slave.spawn_command(cmd).expect("spawn attach"));
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("clone outer reader");
    let mut writer = pair.master.take_writer().expect("take outer writer");
    #[cfg(windows)]
    writer
        .write_all(b"\x1b[1;1R")
        .expect("answer ConPTY inherited-cursor query");
    let (tx, rx) = mpsc::channel();
    let pump = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains("PASTE_READY") {
        assert!(
            Instant::now() < deadline,
            "attach did not replay ready marker; output: {:?}",
            String::from_utf8_lossy(&output)
        );
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }
    // Synchronize on a round trip, not a sleep: input is deliberately drained
    // during startup. An ordinary printable key proves the reader is ready.
    writer.write_all(b"!").unwrap();
    // The probe's first key is echoed as an explicit readiness acknowledgement.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&output).contains("INPUT_READY") {
        assert!(
            Instant::now() < deadline,
            "attach input reader did not become ready"
        );
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            output.extend(chunk);
        }
    }

    // Replay the exact terminal paste protocol, including CRLF line endings.
    // Write complete VT sequences: ConPTY itself can consume partial CSI
    // writes before they reach the child (decoder fragmentation is unit-tested).
    for bytes in [
        "\x1b[200~line one\r\nline two\nline three 工作\x04\x16\x1b[201~".as_bytes(),
        b"#",
    ] {
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !report.exists() {
        assert!(
            Instant::now() < deadline,
            "probe report missing; logs: {}",
            fetch_logs(&tmp, &id)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let result: serde_json::Value = serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        result["submits"], 0,
        "paste newlines submitted the prompt: {result}"
    );
    assert_eq!(result["pastes"], 1, "paste boundaries were lost: {result}");
    assert_eq!(
        result["text"],
        "line one\r\nline two\nline three 工作\x04\x16"
    );

    // Ctrl-D must still detach outside paste, even with VT input active.
    writer.write_all(b"\x04").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.0.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "Ctrl-D did not detach attach");
        std::thread::sleep(Duration::from_millis(50));
    }
    while let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
        output.extend(chunk);
    }
    assert!(String::from_utf8_lossy(&output).contains(&format!("Detached from session {id}")));
    drop(writer);
    drop(pair.master);
    drop(rx);
    pump.join().unwrap();
}
