use crossterm::terminal;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::{
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
};

/// Generous per-line byte budget that accounts for ANSI escape sequences.
const BYTES_PER_LINE_ESTIMATE: u64 = 256;

/// Wide parser column count — prevents any line wrapping inside the vt100 grid.
const PARSER_COLS: u16 = 2000;

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

/// Seek near the end of the log file and read enough bytes to cover `tail * 2`
/// lines (using a generous per-line estimate), returning the raw bytes.
fn read_tail_bytes(log_path: &Path, tail: usize) -> Result<Vec<u8>> {
    let mut file = File::open(log_path)?;
    let file_size = file.seek(SeekFrom::End(0))?;

    let margin = (tail as u64) * 2 * BYTES_PER_LINE_ESTIMATE;
    let start = file_size.saturating_sub(margin);

    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((file_size - start) as usize);
    file.read_to_end(&mut buf)?;

    // If we didn't start at byte 0, we likely started in the middle of a line.
    // Drop the first partial line to avoid feeding truncated ANSI escape
    // sequences into the parser, which can corrupt subsequent color state.
    if start > 0 {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        }
    }

    Ok(buf)
}

/// Parse `bytes` through a vt100 terminal of `tail` rows and collect the last
/// `tail` visible ANSI-formatted row byte vectors, each trimmed to `term_cols`.
///
/// The cursor row is used as the upper bound of real content so that trailing
/// blank rows (present when the log has fewer lines than `tail`) are excluded
/// before taking the final `tail` rows.
fn render_rows(bytes: &[u8], tail: usize, term_cols: u16, keep_color: bool) -> Vec<Vec<u8>> {
    let mut parser = vt100::Parser::new(tail as u16, PARSER_COLS, 0);
    parser.process(bytes);

    let screen = parser.screen();

    // cursor_position() returns (row, col); the cursor row is the last row
    // that received output.  Rows beyond it are blank and should not count.
    let cursor_row = screen.cursor_position().0 as usize;

    // rows_formatted(start_col, width) emits ANSI-formatted bytes for every
    // visible row restricted to the column range [0, term_cols).
    let content_rows: Vec<Vec<u8>> = if keep_color {
        screen
            .rows_formatted(0, term_cols)
            .take(cursor_row + 1)
            .collect()
    } else {
        // If not keeping color, filter out ANSI escape codes from each row.
        screen
            .rows(0, term_cols)
            .take(cursor_row + 1)
            .map(|s| s.into_bytes())
            .collect()
    };

    // Take the last `tail` rows from the content region.
    let skip = content_rows.len().saturating_sub(tail);
    content_rows.into_iter().skip(skip).collect()
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
