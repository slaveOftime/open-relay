//! M4 agent surfaces: machine-readable cursor/history/wait/control commands
//! over the cursor-checked IPC protocol. Text output goes to stdout as data
//! only; diagnostics stay on stderr (PLAN §9.3).

use std::io::Write as _;
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::config::AppConfig;
use crate::error::{AppError, Result};
use crate::ipc;
use crate::protocol::{RpcRequest, RpcResponse};

async fn rpc(config: &AppConfig, node: Option<&str>, inner: RpcRequest) -> Result<RpcResponse> {
    let req = match node {
        None => inner,
        Some(n) => RpcRequest::NodeProxy {
            node: n.to_string(),
            inner: Box::new(inner),
        },
    };
    ipc::send_request_checked(config, req).await
}

struct CursorState {
    running: bool,
    exit_code: Option<i32>,
    offset: u64,
    incarnation: Option<u64>,
}

async fn session_cursor(config: &AppConfig, node: Option<&str>, id: &str) -> Result<CursorState> {
    match rpc(
        config,
        node,
        RpcRequest::SessionCursor { id: id.to_string() },
    )
    .await?
    {
        RpcResponse::SessionCursor {
            running,
            exit_code,
            offset,
            incarnation,
        } => Ok(CursorState {
            running,
            exit_code,
            offset,
            incarnation,
        }),
        other => Err(AppError::Protocol(format!(
            "unexpected response to session_cursor: {other:?}"
        ))),
    }
}

/// `oly observe <id> [--json]`: the session's canonical stream cursor.
pub async fn run_observe(
    config: &AppConfig,
    id: &str,
    node: Option<String>,
    json: bool,
) -> Result<()> {
    let cursor = session_cursor(config, node.as_deref(), id).await?;
    let status = if cursor.running { "running" } else { "exited" };
    if json {
        println!(
            "{{\"v\":1,\"session\":\"{id}\",\"status\":\"{status}\",\"offset\":{},\"exit_code\":{},\"incarnation\":{}}}",
            cursor.offset,
            json_opt_i32(cursor.exit_code),
            json_opt_u64(cursor.incarnation),
        );
    } else {
        println!(
            "{id}\t{status}\toffset={}\tincarnation={}",
            cursor.offset,
            cursor
                .incarnation
                .map(|inc| inc.to_string())
                .unwrap_or_else(|| "none".into()),
        );
    }
    Ok(())
}

/// `oly history <id> --from <offset> --limit <bytes> [--json]`: one bounded
/// window of the filtered output stream.
pub async fn run_history(
    config: &AppConfig,
    id: &str,
    from: u64,
    limit: u32,
    json: bool,
    node: Option<String>,
) -> Result<()> {
    let response = rpc(
        config,
        node.as_deref(),
        RpcRequest::ObserveWindow {
            id: id.to_string(),
            from,
            max_bytes: limit,
        },
    )
    .await?;
    let RpcResponse::ObserveWindow {
        data,
        next_offset,
        running,
        exit_code,
        incarnation,
    } = response
    else {
        return Err(AppError::Protocol(format!(
            "unexpected response to observe_window: {response:?}"
        )));
    };
    if json {
        println!(
            "{{\"v\":1,\"session\":\"{id}\",\"from\":{from},\"next\":{next_offset},\"running\":{running},\"exit_code\":{},\"incarnation\":{},\"bytes\":{},\"data_b64\":\"{}\"}}",
            json_opt_i32(exit_code),
            json_opt_u64(incarnation),
            data.len(),
            base64::engine::general_purpose::STANDARD.encode(&data),
        );
    } else {
        std::io::stdout().write_all(&data)?;
        std::io::stdout().flush()?;
    }
    Ok(())
}

/// `oly screen <id> [--cols N]`: the visible terminal screen (plain text).
pub async fn run_screen(
    config: &AppConfig,
    id: &str,
    cols: Option<u32>,
    node: Option<String>,
) -> Result<()> {
    // Match `oly logs`: render at the local terminal width (fallback 80);
    // a 0 width renders nothing.
    let term_cols = cols
        .map(|c| c as u16)
        .unwrap_or_else(|| crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80));
    let response = rpc(
        config,
        node.as_deref(),
        RpcRequest::LogsTail {
            id: id.to_string(),
            // render_engine_screen truncates to the last `tail` rows of the
            // visible screen; a large tail = the whole screen.
            tail: usize::MAX,
            keep_color: false,
            term_cols,
            from_file: false,
        },
    )
    .await?;
    match response {
        RpcResponse::LogsTail { output, .. } => {
            print!("{}", String::from_utf8_lossy(&output));
            Ok(())
        }
        other => Err(AppError::Protocol(format!(
            "unexpected response to screen: {other:?}"
        ))),
    }
}

/// Wait conditions for [`run_wait`]; `pattern` is matched against output
/// produced after `after` (bounded search buffer).
#[derive(Debug, Default)]
pub struct WaitCondition {
    pub after: u64,
    pub exit: bool,
    pub idle_ms: Option<u64>,
    pub pattern: Option<String>,
    pub timeout_secs: u64,
}

/// `oly wait <id> [--after N] [--exit|--idle-ms M|--pattern RE] [--timeout S]`.
///
/// Exit status: 0 when a condition matched, 2 on timeout, 1 on error.
pub async fn run_wait(
    config: &AppConfig,
    id: &str,
    condition: WaitCondition,
    node: Option<String>,
) -> Result<()> {
    let WaitCondition {
        after,
        exit,
        idle_ms,
        pattern,
        timeout_secs,
    } = condition;
    let pattern = match &pattern {
        Some(p) => Some(
            regex::Regex::new(p)
                .map_err(|err| AppError::Protocol(format!("invalid --pattern regex: {err}")))?,
        ),
        None => None,
    };
    // No explicit condition: any output after `--after` satisfies the wait.
    let want_output = !exit && idle_ms.is_none() && pattern.is_none();
    let deadline = (timeout_secs > 0).then(|| Instant::now() + Duration::from_secs(timeout_secs));
    let mut last_change = Instant::now();
    let mut last_offset = after;
    // Pattern search runs over output produced after `after`, bounded.
    let mut search_from = after;
    let mut search_buf: Vec<u8> = Vec::new();

    loop {
        let cursor = session_cursor(config, node.as_deref(), id).await?;
        if cursor.offset != last_offset {
            last_change = Instant::now();
        }
        if exit && !cursor.running {
            println!(
                "exit\t{}\toffset={}",
                cursor.exit_code.unwrap_or(-1),
                cursor.offset
            );
            return Ok(());
        }
        if want_output && cursor.offset > after {
            println!("output\toffset={}", cursor.offset);
            return Ok(());
        }
        if let Some(idle) = idle_ms {
            let quiet = cursor.offset == last_offset
                && last_change.elapsed() >= Duration::from_millis(idle);
            if quiet {
                println!("idle\t{}ms\toffset={}", idle, cursor.offset);
                return Ok(());
            }
        }
        if let Some(re) = &pattern
            && cursor.offset > search_from
        {
            let window = rpc(
                config,
                node.as_deref(),
                RpcRequest::ObserveWindow {
                    id: id.to_string(),
                    from: search_from,
                    max_bytes: 256 * 1024,
                },
            )
            .await?;
            if let RpcResponse::ObserveWindow {
                data, next_offset, ..
            } = window
            {
                search_buf.extend_from_slice(&data);
                search_from = next_offset;
                // Bound the search buffer (I7): keep the tail.
                if search_buf.len() > 1024 * 1024 {
                    let keep = search_buf.split_off(search_buf.len() - 256 * 1024);
                    search_buf = keep;
                }
                if let Some(m) = re.find(&String::from_utf8_lossy(&search_buf)) {
                    println!("pattern\t{}\toffset={}", m.as_str(), search_from);
                    return Ok(());
                }
            }
        }
        last_offset = cursor.offset;
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            eprintln!("wait timed out after {timeout_secs}s");
            std::process::exit(2);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `oly doctor [id]`: verify sealed-part journal manifests. Exit 1 when any
/// session reports an integrity issue.
pub async fn run_doctor(
    config: &AppConfig,
    id: Option<String>,
    node: Option<String>,
) -> Result<()> {
    let response = rpc(config, node.as_deref(), RpcRequest::Doctor { id }).await?;
    let RpcResponse::Doctor { results } = response else {
        return Err(AppError::Protocol(format!(
            "unexpected response to doctor: {response:?}"
        )));
    };
    let mut problems = 0usize;
    for report in &results {
        if report.issues.is_empty() {
            println!("{}	ok	{} sealed part(s)", report.id, report.sealed_parts);
        } else {
            problems += report.issues.len();
            for issue in &report.issues {
                println!("{}	ISSUE	{issue}", report.id);
            }
        }
    }
    if problems > 0 {
        return Err(AppError::Protocol(format!(
            "journal verification found {problems} issue(s)"
        )));
    }
    Ok(())
}

fn json_opt_i32(value: Option<i32>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}

fn json_opt_u64(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".into())
}
