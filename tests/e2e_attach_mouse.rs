#![cfg(windows)]
mod e2e;
use e2e::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    fs,
    io::{self, Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

struct AttachChild(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for AttachChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_output(rx: &mpsc::Receiver<Vec<u8>>, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains(marker) {
        assert!(
            Instant::now() < deadline,
            "missing {marker}: {:?}",
            String::from_utf8_lossy(&output)
        );
        if let Ok(bytes) = rx.recv_timeout(Duration::from_millis(100)) {
            output.extend(bytes);
        }
    }
}
use windows_sys::Win32::System::Console::*;

// Run inside the session's real ConPTY, using the same native console-record
// input policy as crossterm-based Windows apps (including Codex). No model,
// clipboard, credentials or network. The parent reads only this report.
#[test]
#[ignore = "helper process for the mouse integration test"]
fn native_mouse_probe() {
    let cwd = std::env::current_dir().unwrap();
    // `cargo test -- --ignored` must not launch a blocking console helper.
    if !cwd.join("native-mouse-probe.enabled").is_file() {
        return;
    }
    let path = cwd.join("mouse-report.json");
    // SAFETY: This helper owns its console; valid initialized locals receive
    // records, and union members are read only for their matching EventType.
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        assert_ne!(GetConsoleMode(handle, &mut mode), 0);
        mode &= !(ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_PROCESSED_INPUT
            | ENABLE_VIRTUAL_TERMINAL_INPUT
            | ENABLE_QUICK_EDIT_MODE);
        mode |= ENABLE_EXTENDED_FLAGS | ENABLE_MOUSE_INPUT | ENABLE_WINDOW_INPUT;
        assert_ne!(SetConsoleMode(handle, mode), 0);
        print!("\x1b[?9001h\x1b[?1049h\x1b[?1003;1006hMOUSE_READY\r\n");
        io::stdout().flush().unwrap();
        let mut text = String::new();
        let mut mouse = Vec::new();
        loop {
            let mut record: INPUT_RECORD = std::mem::zeroed();
            let mut count = 0;
            assert_ne!(ReadConsoleInputW(handle, &mut record, 1, &mut count), 0);
            match record.EventType as u32 {
                MOUSE_EVENT => {
                    let event = record.Event.MouseEvent;
                    mouse.push(serde_json::json!({
                        "x": event.dwMousePosition.X,
                        "y": event.dwMousePosition.Y,
                        "flags": event.dwEventFlags,
                        "buttons": event.dwButtonState,
                    }));
                }
                KEY_EVENT => {
                    let key = record.Event.KeyEvent;
                    if key.bKeyDown == 0 {
                        continue;
                    }
                    let unit = key.uChar.UnicodeChar;
                    match unit {
                        33 => {
                            print!("INPUT_READY\r\n");
                            io::stdout().flush().unwrap();
                        }
                        35 => {
                            fs::write(
                                &path,
                                serde_json::to_vec(&serde_json::json!({"mouse":mouse,"text":text}))
                                    .unwrap(),
                            )
                            .unwrap();
                            print!("REPORT_READY\r\n");
                            io::stdout().flush().unwrap();
                        }
                        _ => {
                            if let Some(ch) = char::from_u32(unit.into())
                                && ch != '\0'
                            {
                                text.push(ch);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

#[test]
fn e2e_sgr_mouse_input_does_not_become_native_keyboard_text() {
    let tmp = make_tmp_dir("native_mouse");
    let _daemon = start_daemon(&tmp);
    fs::write(tmp.join("native-mouse-probe.enabled"), b"").unwrap();
    let report = tmp.join("mouse-report.json");
    let exe = std::env::current_exe().unwrap();
    // Scope the report to this isolated helper's working directory.
    let id = start_session(
        &tmp,
        &[
            "--disable-notifications",
            "--tag",
            "temp",
            "--tag",
            "mouse-test",
            "--cwd",
            tmp.to_str().unwrap(),
            exe.to_str().unwrap(),
            "--exact",
            "native_mouse_probe",
            "--ignored",
            "--nocapture",
        ],
    );
    assert!(
        wait_for_log(
            &tmp,
            &id,
            |s| s.contains("MOUSE_READY"),
            Duration::from_secs(10)
        )
        .is_some()
    );
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 36,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(oly_bin());
    command.args(["attach", &id]);
    command.env("OLY_STATE_DIR", tmp.join("oly"));
    command.env("OLY_SOCKET_NAME", socket_name_for_tmp(&tmp));
    let mut child = AttachChild(pair.slave.spawn_command(command).unwrap());
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    writer.write_all(b"\x1b[1;1R").unwrap();
    let (tx, rx) = mpsc::channel();
    let pump = std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    wait_for_output(&rx, "MOUSE_READY");
    // Synchronize through attach's actual input reader, past startup draining.
    writer.write_all(b"!").unwrap();
    wait_for_output(&rx, "INPUT_READY");
    // Complete outer VT writes, just like Windows Terminal mouse motion. The
    // attach reader sees individual console records and must retain boundaries.
    writer.write_all(b"\x1b[<35;54;26M").unwrap();
    writer.write_all(b"\x1b[<35;55;25M").unwrap();
    writer.write_all(b"#").unwrap();
    writer.flush().unwrap();
    wait_for_output(&rx, "REPORT_READY");
    let result: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    assert_eq!(
        result["text"], "",
        "mouse report leaked into keyboard text: {result}"
    );
    assert_eq!(
        result["mouse"],
        serde_json::json!([
            {"x":53,"y":25,"flags":MOUSE_MOVED,"buttons":0},
            {"x":54,"y":24,"flags":MOUSE_MOVED,"buttons":0},
        ]),
        "mouse report was not delivered as the expected native event: {result}"
    );
    writer.write_all(b"\x04").unwrap();
    // ConPTY may lose a synthetic key-down immediately after mouse output.
    // Match the Windows paste/scrollback harness: retry EOT at a bounded pace,
    // never substitute stopping the session for a successful local detach.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut retry_at = Instant::now() + Duration::from_millis(500);
    let mut detach_output = Vec::new();
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        while let Ok(bytes) = rx.try_recv() {
            detach_output.extend(bytes);
        }
        assert!(
            Instant::now() < deadline,
            "attach did not detach; output: {:?}",
            String::from_utf8_lossy(&detach_output)
        );
        if Instant::now() >= retry_at {
            writer.write_all(b"\x04").unwrap();
            writer.flush().unwrap();
            retry_at = Instant::now() + Duration::from_millis(500);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "attach exited unsuccessfully: {status:?}");
    drop(writer);
    drop(pair.master);
    drop(rx);
    pump.join().unwrap();
}
