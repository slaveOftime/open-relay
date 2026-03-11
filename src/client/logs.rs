use crossterm::terminal;
use std::io::Write;

use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
    session::logs::{read_tail_bytes, render_rows},
};

pub async fn run_logs(
    config: &AppConfig,
    id: &str,
    tail: usize,
    keep_color: bool,
    no_truncate: bool,
    node: Option<String>,
    wait_for_prompt: bool,
    timeout_secs: u64,
) -> Result<()> {
    // ── --wait-for-prompt path ────────────────────────────────────────────────
    if wait_for_prompt {
        eprintln!("Waiting for session {id} to need input…");
        let inner = RpcRequest::LogsWait {
            id: id.to_string(),
            tail,
            timeout_secs,
        };
        let req = if let Some(ref node_name) = node {
            RpcRequest::NodeProxy {
                node: node_name.clone(),
                inner: Box::new(inner),
            }
        } else {
            inner
        };
        let _ = ipc::send_request(config, req).await;
    }

    if let Some(node_name) = node {
        // Remote logs via IPC NodeProxy.
        let inner = RpcRequest::LogsSnapshot {
            id: id.to_string(),
            tail,
        };
        let req = RpcRequest::NodeProxy {
            node: node_name,
            inner: Box::new(inner),
        };
        return match ipc::send_request(config, req).await? {
            RpcResponse::LogsSnapshot { lines, .. } => {
                for line in lines {
                    println!("{line}");
                }
                Ok(())
            }
            RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
            _ => Err(AppError::Protocol("unexpected response type".to_string())),
        };
    }

    run_logs_local(config, id, tail, keep_color, no_truncate).await
}

async fn run_logs_local(
    config: &AppConfig,
    id: &str,
    tail: usize,
    keep_color: bool,
    no_truncate: bool,
) -> Result<()> {
    let db = Database::open(&config.db_file, config.sessions_dir.clone()).await?;
    let session_dir = match db.get_session_dir(id).await? {
        Some(dir) => dir,
        None => return Err(AppError::Protocol(format!("session not found: {id}"))),
    };

    let log_path = session_dir.join("output.log");
    if !log_path.exists() {
        return Err(AppError::Protocol(format!(
            "log file not found: {}",
            log_path.display()
        )));
    }

    let term_cols = if no_truncate {
        u16::MAX
    } else {
        terminal::size().map(|(w, _)| w).unwrap_or(80)
    };

    // Step 1: seek to a position that gives `tail * 2` lines worth of bytes,
    // providing enough context for the vt100 parser even with heavy escape usage.
    let bytes = read_tail_bytes(&log_path, tail)?;

    // Step 2: feed bytes into a vt100 parser sized to (tail rows × 2000 cols),
    // then collect each visible row formatted and trimmed to the terminal width.
    let rows = render_rows(&bytes, tail, term_cols, keep_color);

    print_rows(&rows, keep_color)
}

/// Write all rows to stdout, skipping any leading blank rows.
///
/// When color output is enabled, emit a final ANSI reset so styles do not
/// leak into subsequent terminal output.
fn print_rows(rows: &[Vec<u8>], keep_color: bool) -> Result<()> {
    let _reset_guard = TerminalResetGuard::new(keep_color);

    // Find the first non-empty row so we don't print a sea of blank lines when
    // the log is shorter than `tail`.
    let first_content = rows
        .iter()
        .position(|r| !r.is_empty() && r.iter().any(|&b| !b.is_ascii_whitespace()))
        .unwrap_or(0);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for row in &rows[first_content..] {
        out.write_all(row)?;
        if keep_color {
            out.write_all(b"\x1b[0m")?;
        }
        writeln!(out)?;
    }

    if keep_color {
        out.write_all(b"\x1b[0m\x1b[39m\x1b[49m\x1b[?25h")?;
    }

    Ok(())
}

struct TerminalResetGuard {
    enabled: bool,
}

impl TerminalResetGuard {
    fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl Drop for TerminalResetGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[39m\x1b[49m\x1b[?25h");
            let _ = std::io::stdout().flush();
        }
    }
}
