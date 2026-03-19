use crossterm::terminal;
use std::io::Write;

use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
    session::logs::render_log_file,
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

    let output = render_log_file(&log_path, tail, keep_color, term_cols, None)?;
    write_rendered_output(&output, keep_color)
}

/// Write the rendered CLI log output to stdout.
///
/// When color output is enabled, emit a final ANSI reset so styles do not
/// leak into subsequent terminal output.
fn write_rendered_output(output: &[u8], keep_color: bool) -> Result<()> {
    let _reset_guard = crate::terminal_guards::ColorfulGuard::new(keep_color);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(output)?;

    Ok(())
}
