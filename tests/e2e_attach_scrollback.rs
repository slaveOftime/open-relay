mod e2e;

use e2e::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

struct ChildGuard(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct OuterTerminal {
    child: ChildGuard,
    _master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    screen: vt100::Parser,
}

impl OuterTerminal {
    fn attach(tmp: &std::path::Path, id: &str) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(oly_bin());
        command.args(["attach", id]);
        command.env("OLY_STATE_DIR", tmp.join("oly"));
        command.env("OLY_SOCKET_NAME", socket_name_for_tmp(tmp));
        let child = ChildGuard(pair.slave.spawn_command(command).unwrap());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        #[cfg(windows)]
        writer.write_all(b"\x1b[1;1R").unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            _master: pair.master,
            writer,
            rx,
            screen: vt100::Parser::new(24, 80, 1000),
        }
    }

    fn wait_for(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.screen.screen().contents().contains(marker) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {marker}; screen: {}",
                self.screen.screen().contents()
            );
            if let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.screen.process(&bytes);
            }
        }
    }

    fn history(&mut self) -> String {
        self.screen.screen_mut().set_scrollback(1000);
        let history = self.screen.screen().contents();
        self.screen.screen_mut().set_scrollback(0);
        history
    }

    fn detach(&mut self) {
        self.writer.write_all(b"\x04").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "attach did not detach");
            if let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.screen.process(&bytes);
            }
        }
    }
}

#[test]
fn e2e_attach_picker_to_chat_preserves_live_and_reattached_scrollback() {
    assert!(
        program_exists("node"),
        "node required for benign terminal probe"
    );
    let tmp = make_tmp_dir("attach_scrollback");
    let _daemon = start_daemon(&tmp);
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/scrollback_probe.cjs");
    let id = start_session(
        &tmp,
        &[
            "--disable-notifications",
            "--tag",
            "temp",
            "--tag",
            "scrollback-test",
            "node",
            fixture.to_str().unwrap(),
        ],
    );
    assert!(
        wait_for_log(
            &tmp,
            &id,
            |log| log.contains("PICKER_READY"),
            Duration::from_secs(10)
        )
        .is_some()
    );
    let mut outer = OuterTerminal::attach(&tmp, &id);
    outer.wait_for("PICKER_READY");
    // Sending via the independent daemon control path avoids a startup-drain
    // race; this test asserts the output behavior, not keyboard delivery.
    send_text_only(&tmp, &id, "!");
    outer.wait_for("CHAT_READY");
    assert!(
        !outer.screen.screen().alternate_screen(),
        "picker must leave the outer alternate screen"
    );
    assert!(
        outer.history().contains("TRANSCRIPT_001"),
        "old transcript lines must be scrollable while attached"
    );
    outer.detach();

    let mut reattached = OuterTerminal::attach(&tmp, &id);
    reattached.wait_for("CHAT_READY");
    assert!(
        reattached.history().contains("TRANSCRIPT_001"),
        "reattach must restore history, not only the latest screen"
    );
    send_text_only(&tmp, &id, "@");
    reattached.wait_for("FULLSCREEN_READY");
    assert!(
        reattached.screen.screen().alternate_screen(),
        "fullscreen app must own the alternate buffer"
    );
    send_text_only(&tmp, &id, "#");
    reattached.wait_for("CHAT_READY");
    assert!(!reattached.screen.screen().alternate_screen());
    assert!(
        reattached.history().contains("TRANSCRIPT_001"),
        "fullscreen roundtrip lost main-buffer history"
    );
    reattached.detach();
}
