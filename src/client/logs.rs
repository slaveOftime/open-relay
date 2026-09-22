use crossterm::terminal;
use std::io::{IsTerminal, Write};

use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
    session::logs::render_log_session,
};

/// Long-lived plumbing function: the CLI surface (`LogsArgs`) maps 1:1
/// to these orthogonal parameters; a parameter-object refactor would add
/// indirection at the only call site for no win.
#[allow(clippy::too_many_arguments)]
pub async fn run_logs(
    config: &AppConfig,
    id: &str,
    tail: Option<usize>,
    keep_color: bool,
    from_file: bool,
    no_truncate: bool,
    raw: bool,
    cols: Option<u32>,
    node: Option<String>,
    wait_for_prompt: bool,
    timeout_ms: u64,
) -> Result<()> {
    // ── --raw export path ─────────────────────────────────────────────────────
    if raw {
        return run_logs_raw(config, id, node);
    }

    // ── --wait-for-prompt path ────────────────────────────────────────────────
    if wait_for_prompt {
        eprintln!("Waiting for session {id} to need input…");
        let inner = RpcRequest::LogsWait {
            id: id.to_string(),
            timeout_ms,
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

    let tail = tail.unwrap_or_else(|| {
        terminal::size()
            .map(|(_, h)| (h - 1) as usize)
            .unwrap_or(40)
    });

    let term_cols = if no_truncate {
        u16::MAX
    } else if let Some(cols) = cols {
        // Explicit --cols render width (0 renders nothing).
        cols.min(u32::from(u16::MAX)) as u16
    } else {
        terminal::size().map(|(w, _)| w).unwrap_or(80)
    };

    if let Some(node_name) = node {
        // Remote logs via IPC NodeProxy.
        let inner = RpcRequest::LogsTail {
            id: id.to_string(),
            tail,
            keep_color,
            term_cols,
            from_file,
        };
        let req = RpcRequest::NodeProxy {
            node: node_name,
            inner: Box::new(inner),
        };
        return match ipc::send_request_checked(config, req).await? {
            RpcResponse::LogsTail { output, status, .. } => {
                print_log_output(output, keep_color, id, status.as_deref())
            }
            _ => Err(AppError::Protocol("unexpected response type".to_string())),
        };
    }

    run_logs_local(config, id, tail, keep_color, term_cols, from_file).await
}

/// M5-6 safe export: the original output byte stream, exported only on
/// explicit request. Rendered `oly logs` (plain or `--keep-color`) is
/// sanitized by the engine round-trip — control sequences are interpreted,
/// never re-emitted, and hyperlink escape codes are dropped — while `--raw`
/// returns the exact child bytes and therefore warns on a TTY. Local disk
/// read: the journal is the source of truth and is written before output is
/// broadcast (ADR-0002/0006), so a visible byte is always exportable.
fn run_logs_raw(config: &AppConfig, id: &str, node: Option<String>) -> Result<()> {
    if node.is_some() {
        return Err(AppError::Protocol(
            "raw export is only available for local sessions".to_string(),
        ));
    }

    let session_dir = config.sessions_dir.join(id);
    let journal_dir = session_dir.join(crate::session::journal::JOURNAL_DIR_NAME);
    if !journal_dir.is_dir() {
        return Err(AppError::Protocol(format!(
            "session {id} uses the pre-0.5 log format (output.log); export it \
             with a 0.x build first, see MIGRATION.md"
        )));
    }
    let bytes = crate::session::replay::filtered_stream_from(&session_dir, 0)
        .map_err(|err| AppError::Protocol(format!("raw export failed for {id}: {err}")))?
        .0;

    let mut stdout = std::io::stdout();
    if stdout.is_terminal() {
        eprintln!(
            "warning: `oly logs --raw` writes unfiltered child output, which may \
             contain terminal control sequences; redirect to a file or pipe instead"
        );
    }
    stdout.lock().write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

async fn run_logs_local(
    config: &AppConfig,
    id: &str,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
    from_file: bool,
) -> Result<()> {
    if !from_file {
        match ipc::send_request_checked(
            config,
            RpcRequest::LogsTail {
                id: id.to_string(),
                tail,
                keep_color,
                term_cols,
                from_file: false,
            },
        )
        .await
        {
            Ok(RpcResponse::LogsTail { output, status, .. }) => {
                return print_log_output(output, keep_color, id, status.as_deref());
            }
            Ok(_) => return Err(AppError::Protocol("unexpected response type".to_string())),
            Err(AppError::DaemonUnavailable(_)) => {}
            Err(err) => return Err(err),
        }
    }

    let db = Database::open(&config.db_file, config.sessions_dir.clone()).await?;
    let session = match db.get_session(id).await? {
        Some(session) => session,
        None => return Err(AppError::Protocol(format!("session not found: {id}"))),
    };
    let session_dir = config.sessions_dir.join(id);

    if !session_dir
        .join(crate::session::journal::JOURNAL_DIR_NAME)
        .is_dir()
    {
        return Err(AppError::Protocol(format!(
            "session {id} has no journal (pre-0.5 log format); see MIGRATION.md"
        )));
    }

    let (output, _resizes) = render_log_session(&session_dir, tail, keep_color, term_cols, None)?;

    print_log_output(output, keep_color, id, Some(session.status.as_str()))
}

fn print_log_output(
    output: Vec<u8>,
    keep_color: bool,
    id: &str,
    status: Option<&str>,
) -> Result<()> {
    let _reset_guard = crate::terminal_guards::ColorfulGuard::new(keep_color);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(&output)?;

    if let Some(line) = inactive_session_helper(id, status) {
        if !output.is_empty() && !output.ends_with(b"\n") {
            writeln!(out)?;
        }
        writeln!(out, "{line}")?;
    }

    Ok(())
}

fn inactive_session_helper(id: &str, status: Option<&str>) -> Option<String> {
    match status {
        Some("created" | "running") | None => None,
        Some(status) => Some(format!("--- Session {id} is {status} ---")),
    }
}

#[cfg(test)]
mod tests {
    use super::inactive_session_helper;

    #[test]
    fn helper_identifies_inactive_sessions() {
        assert_eq!(
            inactive_session_helper("abc123", Some("failed")).as_deref(),
            Some("--- Session abc123 is failed ---")
        );
        assert!(inactive_session_helper("abc123", Some("running")).is_none());
        assert!(inactive_session_helper("abc123", Some("created")).is_none());
        assert!(inactive_session_helper("abc123", None).is_none());
    }
}
