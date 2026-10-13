mod e2e;

use e2e::*;
use std::{
    io::Write,
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};

fn screen(tmp: &Path, id: &str) -> String {
    let output = oly_cmd(tmp)
        .args(["logs", id, "--screen"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait_screen(tmp: &Path, id: &str, marker: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rendered = screen(tmp, id);
        // Hex text can wrap at the viewport width; remove display whitespace.
        if rendered
            .split_whitespace()
            .collect::<String>()
            .contains(marker)
        {
            return rendered;
        }
        assert!(Instant::now() < deadline, "missing {marker}: {rendered}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn send(tmp: &Path, id: &str, chunks: &[&str]) {
    let mut args = vec!["send", id];
    args.extend_from_slice(chunks);
    let output = oly_cmd(tmp).args(&args).output().unwrap();
    assert!(
        output.status.success(),
        "send {chunks:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn reject(tmp: &Path, id: &str, chunks: &[&str], marker: &str) {
    let mut args = vec!["send", id];
    args.extend_from_slice(chunks);
    let output = oly_cmd(tmp).args(&args).output().unwrap();
    assert!(
        !output.status.success(),
        "{chunks:?} unexpectedly succeeded"
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains(marker), "{chunks:?}: {err}");
}

fn hex(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn e2e_send_mouse_drives_application_view_in_both_encodings() {
    assert!(
        program_exists("node"),
        "node required for benign terminal probe"
    );
    let tmp = make_tmp_dir("send_mouse");
    let _daemon = start_daemon(&tmp);
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mouse_scroll_probe.cjs");

    for mode in ["sgr", "legacy"] {
        let id = start_session(
            &tmp,
            &[
                "--disable-notifications",
                "--tag",
                "temp",
                "--tag",
                "mouse-test",
                "node",
                fixture.to_str().unwrap(),
                mode,
            ],
        );
        wait_screen(&tmp, &id, "FIRST_HISTORY_ROW_040");
        let listing = oly_cmd(&tmp).args(["ls", "--json"]).output().unwrap();
        assert!(listing.status.success());
        let listing: serde_json::Value = serde_json::from_slice(&listing.stdout).unwrap();
        let session = listing["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap();
        let cols = session["cols"].as_u64().unwrap();
        let rows = session["rows"].as_u64().unwrap();
        let center = format!("LAST_POSITION_{}_{}", cols / 2 + 1, rows / 2 + 1);

        send(&tmp, &id, &["mouse:wheel+3"]);
        let rendered = wait_screen(&tmp, &id, "FIRST_HISTORY_ROW_037");
        assert!(
            rendered.contains("WHEEL_EVENTS_00003") && rendered.contains(&center),
            "{rendered}"
        );
        send(&tmp, &id, &["mouse:wheel+3"]);
        wait_screen(&tmp, &id, "FIRST_HISTORY_ROW_034");
        send(&tmp, &id, &["mouse:wheel-3@2,3"]);
        let rendered = wait_screen(&tmp, &id, "FIRST_HISTORY_ROW_037");
        assert!(
            rendered.contains("WHEEL_EVENTS_00009") && rendered.contains("LAST_POSITION_2_3"),
            "{rendered}"
        );

        // Missing click positions fail even with mouse reporting enabled,
        // and neither inject events nor fall back to the viewport center.
        let before = screen(&tmp, &id);
        for spec in [
            "mouse:left",
            "mouse:middle",
            "mouse:right",
            "mouse:left+2",
            "mouse:middle+2",
            "mouse:right+2",
        ] {
            reject(&tmp, &id, &[spec], "requires @col,row");
            assert_eq!(screen(&tmp, &id), before, "{spec} injected input");
        }
        send(&tmp, &id, &["mouse:left@2,3"]);
        let rendered = wait_screen(&tmp, &id, "RELEASE_EVENTS_00001");
        assert!(
            rendered.contains("CLICK_EVENTS_00001") && rendered.contains("LAST_POSITION_2_3"),
            "{rendered}"
        );
        send(&tmp, &id, &["mouse:right+2@4,5"]);
        let rendered = wait_screen(&tmp, &id, "RELEASE_EVENTS_00003");
        assert!(
            rendered.contains("CLICK_EVENTS_00003") && rendered.contains("LAST_POSITION_4_5"),
            "{rendered}"
        );
        send(&tmp, &id, &["mouse:middle@6,7"]);
        let rendered = wait_screen(&tmp, &id, "RELEASE_EVENTS_00004");
        assert!(
            rendered.contains("CLICK_EVENTS_00004") && rendered.contains("LAST_POSITION_6_7"),
            "{rendered}"
        );
        assert!(rendered.contains("ORDER_UUUUUUDDDLrRrRrMr"), "{rendered}");

        // Counts and input order must be observable, not just acknowledged.
        send(&tmp, &id, &["mouse:wheel-2", "x", "mouse:wheel"]);
        let rendered = wait_screen(&tmp, &id, "FIRST_HISTORY_ROW_038");
        assert!(
            rendered.contains("WHEEL_EVENTS_00012") && rendered.contains("DDxU"),
            "{rendered}"
        );

        // Removed alias is now ordinary text, not wheel input.
        send(&tmp, &id, &["scroll:3"]);
        let rendered = wait_screen(&tmp, &id, &format!("TEXT_HEX_{}", hex("xscroll:3")));
        assert!(rendered.contains("WHEEL_EVENTS_00012"), "{rendered}");

        // --node must proxy, not accidentally send to this local session.
        let before = screen(&tmp, &id);
        reject(
            &tmp,
            &id,
            &["--node", "missing-mouse-test-node", "mouse:left@2,3"],
            "node",
        );
        assert_eq!(screen(&tmp, &id), before);

        // The owning daemon uses modes from the application's next redraw.
        let switch = if mode == "sgr" { "#" } else { "^" };
        let next_mode = if mode == "sgr" { "legacy" } else { "sgr" };
        send(&tmp, &id, &[switch]);
        wait_screen(&tmp, &id, &format!("MODE_{next_mode}"));
        send(&tmp, &id, &["mouse:wheel-1"]);
        let rendered = wait_screen(&tmp, &id, "FIRST_HISTORY_ROW_039");
        assert!(rendered.contains("WHEEL_EVENTS_00013"), "{rendered}");

        let before = screen(&tmp, &id);
        reject(
            &tmp,
            &id,
            &["mouse:left@65535,65535"],
            "outside the live viewport",
        );
        assert_eq!(screen(&tmp, &id), before);
        send(&tmp, &id, &["~"]);
        let before = wait_screen(&tmp, &id, "MODE_off");
        for spec in ["mouse:wheel+3", "mouse:left@2,3"] {
            reject(&tmp, &id, &[spec], "mouse reporting is disabled");
            assert_eq!(screen(&tmp, &id), before);
        }
        for spec in [
            "mouse:",
            "mouse:wheel+0",
            "mouse:wheel+1001",
            "mouse:wheel-1001",
            "mouse:wheel+2junk",
            "mouse:left-2",
            "mouse:left+shift",
            "mouse:left-drag",
            "mouse:move",
            "mouse:left*2",
            "mouse:left(2,3)",
            "mouse:left @2,3",
            "mouse:left@0,1",
            "mouse:left@1,2,3",
            "mouse:left@1,2junk",
        ] {
            reject(&tmp, &id, &[spec], "use mouse:left");
            assert_eq!(screen(&tmp, &id), before, "{spec} injected input");
        }

        send(&tmp, &id, &["--", "mouse:wheel+9"]);
        let expected = format!("TEXT_HEX_{}", hex("xscroll:3mouse:wheel+9"));
        let rendered = wait_screen(&tmp, &id, &expected);
        assert!(rendered.contains("WHEEL_EVENTS_00013"), "{rendered}");
        let mut child = oly_cmd(&tmp)
            .args(["send", &id])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"mouse:left@5,5")
            .unwrap();
        assert!(child.wait().unwrap().success());
        let expected = format!("TEXT_HEX_{}", hex("xscroll:3mouse:wheel+9mouse:left@5,5"));
        let rendered = wait_screen(&tmp, &id, &expected);
        assert!(
            rendered.contains("CLICK_EVENTS_00004")
                && rendered.contains("RELEASE_EVENTS_00004")
                && rendered.contains("WHEEL_EVENTS_00013")
                && rendered.contains("INVALID_EVENTS_00000"),
            "{rendered}"
        );

        let stopped = oly_cmd(&tmp)
            .args(["stop", &id, "--grace", "0"])
            .output()
            .unwrap();
        assert!(stopped.status.success());
        reject(&tmp, &id, &["mouse:left@2,3"], "session not running");
    }
}
