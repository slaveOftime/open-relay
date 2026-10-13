use chrono::{DateTime, Local, SecondsFormat, Utc};
use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};

use crate::{
    cli::{ColorMode, InfoArgs},
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse, SessionInfo},
};

pub async fn run_info(config: &AppConfig, args: InfoArgs) -> Result<()> {
    let id = crate::resolve_session_id(config, args.id, args.node.as_ref()).await?;
    let request = crate::node_wrap(args.node.clone(), RpcRequest::SessionInfo { id });
    let RpcResponse::SessionInfo { mut info } = ipc::send_request_checked(config, request).await?
    else {
        return Err(AppError::Protocol(
            "unexpected response to session info".into(),
        ));
    };
    info.summary.node = args.node;

    let output = if args.json {
        serde_json::to_string_pretty(&info)? + "\n"
    } else {
        let color = use_color(
            args.color,
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()),
            std::env::var_os("TERM").is_some_and(|value| value == "dumb"),
        );
        render_info(&info, color, Utc::now())
    };
    std::io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}

fn use_color(mode: ColorMode, terminal: bool, no_color: bool, dumb: bool) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => terminal && !no_color && !dumb,
    }
}

// Escape controls from child/user-provided metadata instead of letting a title,
// command, or OSC color spec inject terminal sequences or fake detail rows.
fn display_value(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

struct Details {
    output: String,
    color: bool,
}

impl Details {
    fn styled(&self, value: &str, style: &str) -> String {
        let value = display_value(value);
        if self.color {
            format!("\x1b[{style}m{value}\x1b[0m")
        } else {
            value
        }
    }

    fn section(&mut self, title: &str) {
        let title = self.styled(title, "1;36");
        writeln!(self.output, "\n{title}").unwrap();
    }

    fn row(&mut self, label: &str, value: impl AsRef<str>) {
        self.row_styled(label, value.as_ref(), "0");
    }

    fn row_styled(&mut self, label: &str, value: &str, style: &str) {
        let label = self.styled(&format!("{label:<21}"), "2");
        let value = self.styled(value, if value == "—" { "2" } else { style });
        writeln!(self.output, "  {label} {value}").unwrap();
    }
}

fn optional<T: ToString>(value: Option<T>) -> String {
    value.map(|v| v.to_string()).unwrap_or_else(|| "—".into())
}

fn timestamp(value: Option<DateTime<Utc>>) -> String {
    optional(value.map(|v| {
        v.with_timezone(&Local)
            .to_rfc3339_opts(SecondsFormat::Secs, false)
    }))
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {} ({value} bytes)", UNITS[unit])
    }
}

fn duration(start: Option<DateTime<Utc>>, end: Option<DateTime<Utc>>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return "—".into();
    };
    let seconds = (end - start).num_seconds().max(0);
    let mut parts = Vec::new();
    for (value, unit) in [
        (seconds / 86400, "d"),
        (seconds / 3600 % 24, "h"),
        (seconds / 60 % 60, "m"),
        (seconds % 60, "s"),
    ] {
        if value > 0 {
            parts.push(format!("{value}{unit}"));
        }
    }
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

fn render_info(info: &SessionInfo, color: bool, now: DateTime<Utc>) -> String {
    let s = &info.summary;
    let mut d = Details {
        output: String::new(),
        color,
    };
    let heading = d.styled(&format!("Session {}", s.id), "1;36");
    writeln!(d.output, "{heading}").unwrap();
    d.row("Title", optional(s.title.as_deref()));
    d.row(
        "Tags",
        if s.tags.is_empty() {
            "—".into()
        } else {
            s.tags.join(", ")
        },
    );
    d.row("Node", s.node.as_deref().unwrap_or("local"));
    let status_style = match s.status.as_str() {
        "running" => "32",
        "created" | "stopping" => "33",
        "failed" | "killed" => "31",
        "stopped" => "2",
        _ => "35",
    };
    d.row_styled("Status", &s.status, status_style);
    d.row_styled(
        "Input needed",
        if s.input_needed { "yes" } else { "no" },
        if s.input_needed { "1;33" } else { "2" },
    );
    d.row_styled(
        "Notifications",
        if s.notifications_enabled {
            "enabled"
        } else {
            "disabled"
        },
        if s.notifications_enabled { "32" } else { "2" },
    );

    d.section("Process");
    d.row("Command", &s.command);
    // JSON quoting preserves empty args, whitespace, and argument boundaries.
    d.row(
        "Arguments",
        serde_json::to_string(&s.args).expect("string arguments serialize"),
    );
    d.row("Working directory", optional(s.cwd.as_deref()));
    d.row("PID", optional(s.pid));
    d.row_styled(
        "Exit code",
        &optional(info.exit_code),
        if info.exit_code.is_some_and(|code| code != 0) {
            "31"
        } else {
            "0"
        },
    );
    d.row("Resume command", optional(s.resume_command.as_deref()));

    d.section("Timing & activity");
    d.row("Created", timestamp(Some(s.created_at)));
    d.row("Started", timestamp(s.started_at));
    d.row("Ended", timestamp(s.ended_at));
    let end = s
        .ended_at
        .or_else(|| matches!(s.status.as_str(), "running" | "stopping").then_some(now));
    d.row("Runtime", duration(s.started_at, end));
    d.row("Last output", timestamp(s.last_output_epoch));
    d.row("Filtered output", bytes(s.last_total_bytes));

    d.section("Terminal");
    d.row("Rows", optional(s.rows));
    d.row("Columns", optional(s.cols));
    d.row("Attached clients", s.attach_count.to_string());
    d.row("Foreground color", optional(s.foreground_color.as_deref()));
    d.row("Background color", optional(s.background_color.as_deref()));

    d.section("Storage & journal");
    d.row("Session directory", &info.session_dir);
    d.row("Journal incarnation", optional(info.journal_incarnation));
    d.row(
        "Retained sealed bytes",
        optional(s.journal_bytes_retained.map(bytes)),
    );
    d.row(
        "Journal byte cap",
        optional(s.journal_byte_cap.map(|cap| {
            if cap == 0 {
                "unlimited".into()
            } else {
                bytes(cap)
            }
        })),
    );
    d.row("Retention sweeps", optional(s.journal_retention_sweeps));
    d.row(
        "Incarnations dropped",
        optional(s.journal_incarnations_dropped),
    );
    d.output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::meta_to_summary,
        session::{SessionMeta, SessionStatus},
    };
    use chrono::TimeZone;

    fn fixture() -> SessionInfo {
        let created = Utc.with_ymd_and_hms(2026, 3, 21, 10, 11, 12).unwrap();
        let meta = SessionMeta {
            id: "info123".into(),
            title: Some("a very long session title without truncation".into()),
            tags: vec!["prod".into(), "release".into()],
            command: "agent".into(),
            args: vec!["--prompt".into(), "two words".into(), "".into()],
            cwd: Some("/work/demo".into()),
            created_at: created,
            started_at: Some(created),
            ended_at: None,
            resume_command: Some("agent resume example".into()),
            status: SessionStatus::Running,
            pid: Some(42),
            exit_code: None,
            notifications_enabled: true,
            foreground_color: Some("#ffffff".into()),
            background_color: Some("#000000".into()),
        };
        let mut summary = meta_to_summary(&meta, true, 4096);
        summary.node = Some("worker-a".into());
        summary.rows = Some(24);
        summary.cols = Some(80);
        summary.attach_count = 2;
        summary.journal_byte_cap = Some(0);
        summary.journal_bytes_retained = Some(2048);
        summary.journal_retention_sweeps = Some(3);
        summary.journal_incarnations_dropped = Some(1);
        SessionInfo {
            summary,
            exit_code: None,
            session_dir: "/state/info123".into(),
            journal_incarnation: Some(8),
        }
    }

    #[test]
    fn info_plain_details_are_complete_and_not_truncated() {
        let info = fixture();
        let out = render_info(
            &info,
            false,
            info.summary.created_at + chrono::Duration::seconds(3661),
        );
        assert!(!out.contains('\x1b'));
        for value in [
            "Session info123",
            "a very long session title without truncation",
            "prod, release",
            "worker-a",
            "running",
            "[\"--prompt\",\"two words\",\"\"]",
            "/work/demo",
            "agent resume example",
            "1h 1m 1s",
            "4.0 KiB (4096 bytes)",
            "#ffffff",
            "#000000",
            "/state/info123",
            "unlimited",
            "2.0 KiB (2048 bytes)",
        ] {
            assert!(out.contains(value), "missing {value:?}: {out}");
        }
    }

    #[test]
    fn info_styles_status_and_input_and_escapes_metadata_controls() {
        let mut info = fixture();
        info.summary.title = Some("bad\x1b[31m\nrow\r\t".into());
        let out = render_info(&info, true, info.summary.created_at);
        assert!(out.contains("\x1b[32mrunning\x1b[0m"));
        assert!(out.contains("\x1b[1;33myes\x1b[0m"));
        assert!(out.contains("bad\\u{1b}[31m\\nrow\\r\\t"));
        let plain = render_info(&info, false, info.summary.created_at);
        assert!(!plain.contains('\x1b'));
        info.summary.status = "failed".into();
        info.exit_code = Some(7);
        let out = render_info(&info, true, info.summary.created_at);
        assert!(out.contains("\x1b[31mfailed\x1b[0m"));
        assert!(out.contains("\x1b[31m7\x1b[0m"));
    }

    #[test]
    fn info_missing_values_and_finished_duration_are_explicit() {
        let mut info = fixture();
        info.summary.title = None;
        info.summary.status = "stopped".into();
        info.summary.ended_at = Some(info.summary.created_at + chrono::Duration::seconds(65));
        let out = render_info(
            &info,
            false,
            info.summary.created_at + chrono::Duration::days(1),
        );
        assert!(out.contains("Title                 —"));
        assert!(out.contains("Runtime               1m 5s"));
        assert_eq!(duration(None, Some(Utc::now())), "—");
        assert_eq!(bytes(0), "0 B");
    }

    #[test]
    fn info_color_policy_honors_terminal_and_environment_unless_forced() {
        assert!(use_color(ColorMode::Auto, true, false, false));
        assert!(!use_color(ColorMode::Auto, false, false, false));
        assert!(!use_color(ColorMode::Auto, true, true, false));
        assert!(!use_color(ColorMode::Auto, true, false, true));
        assert!(use_color(ColorMode::Always, false, true, true));
        assert!(!use_color(ColorMode::Never, true, false, false));
    }

    #[test]
    fn info_json_is_flat_complete_and_round_trips() {
        let info = fixture();
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["id"], "info123");
        assert_eq!(value["node"], "worker-a");
        assert_eq!(value["args"][2], "");
        assert_eq!(value["journal_incarnation"], 8);
        assert_eq!(value["session_dir"], "/state/info123");
        assert!(value["exit_code"].is_null());
        let decoded: SessionInfo = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        let response = RpcResponse::SessionInfo { info };
        let encoded = serde_json::to_string(&response).unwrap();
        assert!(matches!(
            serde_json::from_str::<RpcResponse>(&encoded).unwrap(),
            RpcResponse::SessionInfo { .. }
        ));
    }
}
