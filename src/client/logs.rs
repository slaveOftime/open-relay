//! `oly logs`: print a session's recorded output, optionally after waiting
//! for a condition, and hand back the cursor the read was taken at.
//!
//! Three surfaces, chosen by flag (never by the format of the output, so
//! what you asked for is what you get):
//!
//! - text history (`--tail N`) — the last N lines of the canonical stream.
//! - the current screen (`--screen`) — what an attaching viewer would see,
//!   including for a full-screen program that already exited.
//! - screen history (`--tail-frames N`) — the distinct screens a full-screen
//!   program painted, newest last. Line tailing cannot recover these; they
//!   are re-rendered from the journal (see
//!   [`crate::session::logs::collect_screen_history`]).
//!
//! Default: last 40 text lines. `--since` instead delivers exact bounded
//! canonical-byte pages; rendered views are never lossless transcripts.

use base64::Engine as _;
use std::io::{IsTerminal as _, Write as _};
use std::time::{Duration, Instant};

use regex::Regex;

use crate::cli::{LogsArgs, WaitCondition};
use crate::config::AppConfig;
use crate::error::{AppError, Result};
use crate::ipc;
use crate::protocol::{LogFrame, RpcRequest, RpcResponse, StreamPosition};

use super::log_cursor;

/// Default `--wait` timeout; `--timeout 0` waits forever.
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// Default `--idle-for`: long enough that a program pausing between two
/// writes is not called idle, short enough to feel instant to a human.
const DEFAULT_IDLE_MS: u64 = 800;
/// Cursor poll interval while gated.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Bytes pulled per `--wait match` page.
const MATCH_PAGE_BYTES: u32 = 256 * 1024;
/// Bytes of already-scanned output kept between match pages so a pattern
/// split across two pages is still found.
const MATCH_LINE_BYTES: usize = 1024 * 1024;
/// Lines printed when the caller asked for no surface and the session is a
/// plain text one.
const DEFAULT_TAIL_LINES: usize = 40;

/// `oly logs <id> [flags]`.
pub async fn run_logs(config: &AppConfig, id: &str, args: &LogsArgs) -> Result<()> {
    validate(args)?;
    let node = args.node.as_deref();
    let from = args
        .since
        .as_deref()
        .map(|token| log_cursor::decode(id, token))
        .transpose()?;

    if args.cursor {
        return print_cursor(config, id, node, args.json).await;
    }

    // Pin the whole operation, including the gap between a satisfied gate
    // and its read. A restart must not silently switch the selected session.
    let pinned = session_cursor(config, node, id).await?.incarnation;
    if let Some(from) = from {
        check_incarnation(from.incarnation, pinned)?;
    }
    if let Some(condition) = args.wait {
        wait(config, id, node, condition, args, from, pinned).await?;
    }
    if session_cursor(config, node, id).await?.incarnation != pinned {
        return Err(AppError::Protocol(
            "session restarted before log read".into(),
        ));
    }

    if let Some(from) = from {
        return print_stream_page(config, id, node, args, from).await;
    }

    let (mode, count) = surface(args);
    let response = rpc(
        config,
        node,
        RpcRequest::LogsRead {
            id: id.to_string(),
            mode: mode.to_string(),
            count,
            from,
            keep_color: keep_color(args),
            term_cols: term_cols(args),
        },
    )
    .await?;

    let RpcResponse::LogsRead {
        mode,
        output,
        frames,
        width,
        cursor,
        running,
        exit_code,
        status,
    } = response
    else {
        return Err(AppError::Protocol(format!(
            "unexpected response to logs_read: {response:?}"
        )));
    };

    if session_cursor(config, node, id).await?.incarnation != pinned
        || cursor.is_some_and(|cursor| Some(cursor.incarnation) != pinned)
    {
        return Err(AppError::Protocol(
            "session restarted during log read".into(),
        ));
    }
    if args.json {
        return print_json(LogsJson {
            session: id,
            mode: &mode,
            output: &output,
            frames: &frames,
            width,
            cursor,
            running,
            exit_code,
            status: status.as_deref(),
        });
    }

    if !running && let Some(line) = inactive_note(id, status.as_deref(), exit_code) {
        eprintln!("{line}");
    }

    match mode.as_str() {
        "frames" => print_frames(id, &frames, cursor),
        _ => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&output)?;
            stdout.flush()?;
            Ok(())
        }
    }
}

/// Read a pinned byte page. Revalidate around the RPC so restarts cannot
/// silently change the cursor's byte space, including on remote nodes.
async fn read_page(
    config: &AppConfig,
    id: &str,
    node: Option<&str>,
    position: StreamPosition,
    limit: usize,
) -> Result<(Vec<u8>, Cursor)> {
    let before = session_cursor(config, node, id).await?;
    check_incarnation(position.incarnation, before.incarnation)?;
    if position.offset > before.offset {
        return Err(AppError::Protocol("cursor is past the stream end".into()));
    }
    let limit = limit.min(before.offset.saturating_sub(position.offset) as usize);
    let data = if limit == 0 {
        Vec::new()
    } else {
        match rpc(
            config,
            node,
            RpcRequest::ObserveWindow {
                id: id.into(),
                from: position.offset,
                max_bytes: limit as u32,
            },
        )
        .await?
        {
            RpcResponse::ObserveWindow {
                data, incarnation, ..
            } => {
                check_incarnation(position.incarnation, incarnation)?;
                data
            }
            other => {
                return Err(AppError::Protocol(format!(
                    "unexpected stream page: {other:?}"
                )));
            }
        }
    };
    let after = session_cursor(config, node, id).await?;
    check_incarnation(position.incarnation, after.incarnation)?;
    Ok((data, before))
}

fn check_incarnation(expected: u64, actual: Option<u64>) -> Result<()> {
    if actual != Some(expected) {
        return Err(AppError::Protocol(
            "session restarted or cursor incarnation is stale".into(),
        ));
    }
    Ok(())
}

async fn print_stream_page(
    config: &AppConfig,
    id: &str,
    node: Option<&str>,
    args: &LogsArgs,
    from: StreamPosition,
) -> Result<()> {
    let (data, observed) = read_page(
        config,
        id,
        node,
        from,
        args.limit_bytes.unwrap_or(256 * 1024),
    )
    .await?;
    let next = StreamPosition {
        incarnation: from.incarnation,
        offset: from.offset + data.len() as u64,
    };
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "v": 1, "session": id, "mode": "stream", "encoding": "base64",
                "bytes": base64::prelude::BASE64_STANDARD.encode(&data),
                "start_cursor": log_cursor::encode(id, from), "cursor": log_cursor::encode(id, next),
                "has_more": next.offset < observed.offset, "running": observed.running,
                "exit_code": observed.exit_code,
            })
        );
    } else {
        let mut out = std::io::stdout().lock();
        out.write_all(&data)?;
        out.flush()?;
    }
    Ok(())
}

/// `oly export <id>`: the canonical output byte stream on stdout, for files
/// and pipes. Rendering interprets control sequences and can never reproduce
/// them; this path writes the recorded bytes untouched and warns when a
/// human points it at a terminal.
pub async fn run_export(
    config: &AppConfig,
    id: &str,
    json: bool,
    node: Option<&str>,
) -> Result<()> {
    let observed = session_cursor(config, node, id).await?;
    let incarnation = observed
        .incarnation
        .ok_or_else(|| AppError::Protocol("no recorded stream".into()))?;
    let end = observed.offset;
    let mut position = StreamPosition {
        incarnation,
        offset: 0,
    };
    let mut carry = Vec::new();
    let mut stalled_since = Instant::now();
    if json {
        print!(
            "{{\"v\":1,\"session\":{},\"bytes\":\"",
            serde_json::to_string(id)?
        );
    } else if std::io::stdout().is_terminal() {
        eprintln!(
            "warning: canonical recorded bytes contain terminal controls; redirect to a file or pipe"
        );
    }
    while position.offset < end {
        let limit = (end - position.offset).min(1024 * 1024) as usize;
        let (data, _) = read_page(config, id, node, position, limit).await?;
        if data.is_empty() {
            if stalled_since.elapsed() > Duration::from_secs(30) {
                return Err(AppError::Protocol(
                    "export stalled waiting for journal persistence".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        }
        stalled_since = Instant::now();
        position.offset += data.len() as u64;
        let mut out = std::io::stdout().lock();
        if json {
            carry.extend_from_slice(&data);
            let complete = carry.len() / 3 * 3;
            out.write_all(
                base64::prelude::BASE64_STANDARD
                    .encode(&carry[..complete])
                    .as_bytes(),
            )?;
            carry.drain(..complete);
        } else {
            out.write_all(&data)?;
        }
        out.flush()?;
    }
    if json {
        println!(
            "{}\",\"size\":{end}}}",
            base64::prelude::BASE64_STANDARD.encode(&carry)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Surface selection
// ---------------------------------------------------------------------------

/// The print surface and its count, as the RPC understands them.
///
/// `count` reports the requested history size for `tail` (rows) and
/// `frames` (snapshots); the daemon ignores it for `screen` since the
/// visible screen is exactly one snapshot.
fn surface(args: &LogsArgs) -> (&'static str, usize) {
    if let Some(count) = args.tail_frames {
        ("frames", count)
    } else if args.screen {
        ("screen", 0)
    } else if let Some(lines) = args.tail {
        ("tail", lines)
    } else {
        ("tail", DEFAULT_TAIL_LINES)
    }
}

/// Flag combinations that cannot mean anything coherent. Checked here
/// rather than in clap so the message names the condition they belong to.
fn validate(args: &LogsArgs) -> Result<()> {
    let error = |message: String| AppError::Protocol(message);
    if args.timeout.is_some() && args.wait.is_none() {
        return Err(error("--timeout requires --wait".into()));
    }
    if args.idle_for.is_some() && args.wait != Some(WaitCondition::Idle) {
        return Err(error("--idle-for requires --wait idle".into()));
    }
    if args.tail.is_some_and(|n| n > 65535) || args.tail_frames.is_some_and(|n| n > 1024) {
        return Err(error(
            "maximum --tail is 65535; maximum --tail-frames is 1024".into(),
        ));
    }
    if args
        .limit_bytes
        .is_some_and(|n| n == 0 || n > 8 * 1024 * 1024)
    {
        return Err(error("--limit-bytes must be 1..8388608".into()));
    }
    match (args.wait, args.match_.as_deref(), args.idle_for) {
        (None, Some(_), _) => Err(error(
            "--match needs --wait match (it only means something as a wait condition)".to_string(),
        )),
        (Some(WaitCondition::Match), None, _) => {
            Err(error("--wait match needs a --match <REGEX>".to_string()))
        }
        (Some(WaitCondition::Match), Some(pattern), _) => {
            Regex::new(pattern).map_err(|err| error(format!("invalid --match regex: {err}")))?;
            Ok(())
        }
        (Some(WaitCondition::Idle), _, Some(0)) => Err(error(
            "--idle-for 0 would report idle before anything could happen".to_string(),
        )),
        (Some(condition), _, Some(_)) if condition != WaitCondition::Idle => Err(error(format!(
            "--idle-for only applies to --wait idle (not --wait {})",
            kebab(condition)
        ))),
        (Some(condition), Some(_), _) if condition != WaitCondition::Match => Err(error(format!(
            "--match only applies to --wait match (not --wait {})",
            kebab(condition)
        ))),
        _ => Ok(()),
    }
}

/// How the condition reads in a status line: a verb phrase for "waiting",
/// the bare name for "timed out".
fn describe(condition: WaitCondition) -> &'static str {
    match condition {
        WaitCondition::Output => "produce new output",
        WaitCondition::Prompt => "need input",
        WaitCondition::Idle => "go quiet",
        WaitCondition::Exit => "exit",
        WaitCondition::Match => "match the pattern",
    }
}

fn kebab(condition: WaitCondition) -> &'static str {
    match condition {
        WaitCondition::Output => "output",
        WaitCondition::Prompt => "prompt",
        WaitCondition::Idle => "idle",
        WaitCondition::Exit => "exit",
        WaitCondition::Match => "match",
    }
}

/// Colorize when asked, or when the consumer is a terminal. A pipe gets
/// plain bytes either way: `--color always` would be a way to say "I know
/// what I'm doing", so it wins over the terminal check.
fn keep_color(args: &LogsArgs) -> bool {
    match args.color {
        crate::cli::ColorMode::Always => true,
        crate::cli::ColorMode::Never => false,
        crate::cli::ColorMode::Auto => std::io::stdout().is_terminal(),
    }
}

/// Render width: the caller's terminal, or 0 (no viewport) when stdout is
/// piped or JSON, so nothing is clipped at a grid the consumer does not have.
fn term_cols(args: &LogsArgs) -> u16 {
    if args.json || !std::io::stdout().is_terminal() {
        return 0;
    }
    crossterm::terminal::size()
        .map(|(width, _)| width)
        .unwrap_or(80)
}

// ---------------------------------------------------------------------------
// Waiting
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn wait(
    config: &AppConfig,
    id: &str,
    node: Option<&str>,
    condition: WaitCondition,
    args: &LogsArgs,
    from: Option<StreamPosition>,
    pinned: Option<u64>,
) -> Result<()> {
    let timeout_ms = args.timeout.unwrap_or(DEFAULT_TIMEOUT_MS);
    let idle_ms = args.idle_for.unwrap_or(DEFAULT_IDLE_MS);
    let pattern = args
        .match_
        .as_deref()
        .map(Regex::new)
        .transpose()
        .map_err(|err| AppError::Protocol(format!("invalid --match regex: {err}")))?;

    let start = session_cursor(config, node, id).await?;
    if start.incarnation != pinned {
        return Err(AppError::Protocol("session restarted before wait".into()));
    }
    // The start point is the cursor the caller named (`--since`) or, by
    // default, the stream end: waiting for output should mean "what comes
    // next", not "something is already there".
    if let (Some(given), Some(current)) = (from, start.incarnation)
        && given.incarnation != current
    {
        return Err(AppError::Protocol(format!(
            "cursor is stale: it points at incarnation {} but this session is now incarnation {}",
            given.incarnation, current
        )));
    }
    let start_offset = from.map(|position| position.offset).unwrap_or(start.offset);
    let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
    let mut last_change = Instant::now();
    let mut last_offset = start.offset;
    // Match retains the current logical line across pages and polls.
    let mut scan_pos = start_offset;
    let mut scan_tail: Vec<u8> = Vec::new();

    eprintln!("waiting for session {id} to {}…", describe(condition));
    loop {
        let cursor = session_cursor(config, node, id).await?;
        if cursor.incarnation != start.incarnation {
            return Err(AppError::Protocol("session restarted during wait".into()));
        }
        if cursor.offset != last_offset {
            last_offset = cursor.offset;
            last_change = Instant::now();
        }
        let satisfied = match condition {
            WaitCondition::Output => cursor.offset > start_offset || !cursor.running,
            WaitCondition::Prompt => cursor.input_needed || !cursor.running,
            WaitCondition::Idle => {
                // No more output will ever arrive: an ended session is idle
                // by definition, not by timeout.
                !cursor.running || last_change.elapsed() >= Duration::from_millis(idle_ms)
            }
            WaitCondition::Exit => !cursor.running,
            WaitCondition::Match => {
                scan_for_match(
                    config,
                    node,
                    id,
                    pattern.as_ref().expect("--wait match validated --match"),
                    &mut scan_pos,
                    cursor.offset,
                    &mut scan_tail,
                    start.incarnation,
                )
                .await?
            }
        };
        if satisfied {
            if condition == WaitCondition::Exit
                && let Some(code) = cursor.exit_code
                && code != 0
            {
                // The exit code is data the caller reads from the printed
                // output; surface it before printing so a non-zero child is
                // visible even when the output itself says nothing.
                eprintln!("session {id} exited with code {code}");
            }
            return Ok(());
        }
        if !cursor.running && condition == WaitCondition::Match {
            return Err(AppError::Protocol(
                "session ended without matching the pattern".into(),
            ));
        }
        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            eprintln!(
                "timed out after {}s waiting for session {id} to {}",
                timeout_ms / 1000,
                describe(condition)
            );
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({"v":1,"session":id,"outcome":"timeout","condition":kebab(condition),"cursor":null})
                );
            }
            std::process::exit(2);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Page whatever the session produced since the last scan and search it.
#[allow(clippy::too_many_arguments)]
async fn scan_for_match(
    config: &AppConfig,
    node: Option<&str>,
    id: &str,
    pattern: &Regex,
    scan_pos: &mut u64,
    available: u64,
    carry: &mut Vec<u8>,
    incarnation: Option<u64>,
) -> Result<bool> {
    while *scan_pos < available {
        let response = rpc(
            config,
            node,
            RpcRequest::ObserveWindow {
                id: id.to_string(),
                from: *scan_pos,
                max_bytes: MATCH_PAGE_BYTES,
            },
        )
        .await?;
        let RpcResponse::ObserveWindow {
            data,
            next_offset,
            incarnation: page_incarnation,
            ..
        } = response
        else {
            return Err(AppError::Protocol(format!(
                "unexpected response to observe_window: {response:?}"
            )));
        };
        if page_incarnation != incarnation
            || session_cursor(config, node, id).await?.incarnation != incarnation
        {
            return Err(AppError::Protocol(
                "session restarted during match scan".into(),
            ));
        }
        if data.is_empty() {
            break;
        }
        *scan_pos = next_offset;
        if match_lines(pattern, carry, &data)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Page-independent, bounded logical-line matching, including partial prompts.
fn match_lines(pattern: &Regex, line: &mut Vec<u8>, data: &[u8]) -> Result<bool> {
    for part in data.split_inclusive(|b| *b == b'\n') {
        let complete = part.last() == Some(&b'\n');
        let payload = if complete {
            &part[..part.len() - 1]
        } else {
            part
        };
        if line.len().saturating_add(payload.len()) > MATCH_LINE_BYTES {
            return Err(AppError::Protocol("match input line exceeds 1 MiB; use a shorter-line producer or another wait condition".into()));
        }
        line.extend_from_slice(payload);
        // Do not invent a replacement character for an unfinished codepoint
        // at a page/poll boundary. Truly invalid bytes and completed lines
        // still use replacement decoding.
        let observed = if complete {
            line.as_slice()
        } else {
            complete_utf8_prefix(line)
        };
        if pattern.is_match(&String::from_utf8_lossy(observed)) {
            return Ok(true);
        }
        if complete {
            line.clear();
        }
    }
    Ok(false)
}

fn complete_utf8_prefix(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while let Err(error) = std::str::from_utf8(&bytes[start..]) {
        start += error.valid_up_to();
        match error.error_len() {
            Some(len) => start += len,
            None => return &bytes[..start],
        }
    }
    bytes
}

/// Where the session's stream currently stands.
struct Cursor {
    running: bool,
    exit_code: Option<i32>,
    offset: u64,
    incarnation: Option<u64>,
    input_needed: bool,
}

async fn session_cursor(config: &AppConfig, node: Option<&str>, id: &str) -> Result<Cursor> {
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
            input_needed,
        } => Ok(Cursor {
            running,
            exit_code,
            offset,
            incarnation,
            input_needed,
        }),
        other => Err(AppError::Protocol(format!(
            "unexpected response to session_cursor: {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

/// `oly logs --cursor`: just the token, so `oly logs --since "$(oly logs
/// --cursor)"` composes; `--json` adds the liveness fields around it.
async fn print_cursor(config: &AppConfig, id: &str, node: Option<&str>, json: bool) -> Result<()> {
    let cursor = session_cursor(config, node, id).await?;
    let token = cursor
        .incarnation
        .map(|incarnation| {
            log_cursor::encode(
                id,
                StreamPosition {
                    incarnation,
                    offset: cursor.offset,
                },
            )
        })
        .ok_or_else(|| {
            AppError::Protocol(format!(
                "session {id} has recorded no output yet, so it has no cursor"
            ))
        })?;
    if json {
        let object = serde_json::json!({
            "v": 1,
            "session": id,
            "cursor": token,
            "running": cursor.running,
            "exit_code": cursor.exit_code,
        });
        println!("{object}");
    } else {
        println!("{token}");
    }
    Ok(())
}

struct LogsJson<'a> {
    session: &'a str,
    mode: &'a str,
    output: &'a [u8],
    frames: &'a [LogFrame],
    width: u16,
    cursor: Option<StreamPosition>,
    running: bool,
    exit_code: Option<i32>,
    status: Option<&'a str>,
}

fn print_json(payload: LogsJson<'_>) -> Result<()> {
    let LogsJson {
        session,
        mode,
        output,
        frames,
        width,
        cursor,
        running,
        exit_code,
        status,
    } = payload;
    let mut object = serde_json::json!({
        "v": 1,
        "session": session,
        "mode": mode,
        "width": width,
        "running": running,
        "exit_code": exit_code,
        "status": status,
        "cursor": cursor_token(session, cursor),
    });
    match mode {
        "frames" => {
            object["frames"] = frames
                .iter()
                .map(|frame| {
                    serde_json::json!({
                        "cursor": cursor_token(session, Some(frame.position)),
                        "rows": frame.rows,
                        "cols": frame.cols,
                        "content": String::from_utf8_lossy(&frame.content),
                    })
                })
                .collect();
        }
        _ => {
            object["content"] = String::from_utf8_lossy(output).into_owned().into();
        }
    }
    println!("{object}");
    Ok(())
}

fn cursor_token(session: &str, cursor: Option<StreamPosition>) -> Option<String> {
    cursor.map(|position| log_cursor::encode(session, position))
}

/// Screen history on a terminal: each snapshot in a labelled block, so a
/// human can tell the redraws apart without reading a spec.
///
/// Frame labels are human presentation, not stream continuation cursors.
/// Machine-readable observation positions are available in JSON.
fn print_frames(
    _session: &str,
    frames: &[LogFrame],
    _cursor: Option<StreamPosition>,
) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    if frames.is_empty() {
        return Ok(());
    }
    for (index, frame) in frames.iter().enumerate() {
        let label = format!("{}/{}", index + 1, frames.len());
        let separator = format!("--- frame {label} ---");
        writeln!(stdout, "{separator}")?;
        stdout.write_all(&frame.content)?;
    }
    stdout.flush()?;
    Ok(())
}

/// Status of a session that is no longer producing output, on stderr so
/// pipes receive only the recorded bytes.
fn inactive_note(id: &str, status: Option<&str>, exit_code: Option<i32>) -> Option<String> {
    let status = status?;
    if matches!(status, "created" | "running") {
        return None;
    }
    Some(match exit_code {
        Some(code) => format!("--- Session {id} is {status} (exit code {code}) ---"),
        None => format!("--- Session {id} is {status} ---"),
    })
}

async fn rpc(config: &AppConfig, node: Option<&str>, inner: RpcRequest) -> Result<RpcResponse> {
    let request = match node {
        None => inner,
        Some(name) => RpcRequest::NodeProxy {
            node: name.to_string(),
            inner: Box::new(inner),
        },
    };
    ipc::send_request_checked(config, request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    fn logs(argv: &[&str]) -> LogsArgs {
        let cli = Cli::try_parse_from(argv).expect("parses");
        match cli.command {
            crate::cli::Commands::Logs(args) => args,
            _ => panic!("expected logs"),
        }
    }

    #[test]
    fn partial_utf8_never_matches_an_invented_replacement_at_page_boundaries() {
        let replacement = Regex::new("�").unwrap();
        let mut carry = Vec::new();
        assert!(!match_lines(&replacement, &mut carry, &[0xe4]).unwrap());
        assert!(!match_lines(&replacement, &mut carry, &[0xbd]).unwrap());
        assert!(!match_lines(&replacement, &mut carry, &[0xa0]).unwrap());
        assert!(match_lines(&Regex::new("你").unwrap(), &mut carry, b" ").unwrap());
        carry.clear();
        assert!(match_lines(&replacement, &mut carry, &[0xff]).unwrap());
        carry.clear();
        assert!(!match_lines(&replacement, &mut carry, &[0xe4]).unwrap());
        assert!(match_lines(&replacement, &mut carry, b"\n").unwrap());
    }

    #[test]
    fn surface_defaults_to_tail_and_flags_select_the_rest() {
        assert_eq!(surface(&logs(&["oly", "logs", "s"])), ("tail", 40));
        assert_eq!(
            surface(&logs(&["oly", "logs", "s", "--tail", "5"])),
            ("tail", 5)
        );
        assert_eq!(
            surface(&logs(&["oly", "logs", "s", "--screen"])),
            ("screen", 0)
        );
        assert_eq!(
            surface(&logs(&["oly", "logs", "s", "--tail-frames", "3"])),
            ("frames", 3)
        );
    }

    #[test]
    fn print_surfaces_are_mutually_exclusive() {
        for argv in [
            &["oly", "logs", "s", "--tail", "5", "--screen"][..],
            &["oly", "logs", "s", "--tail", "5", "--tail-frames", "2"][..],
            &["oly", "logs", "s", "--screen", "--tail-frames", "2"][..],
        ] {
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "should reject: {argv:?}"
            );
        }
    }

    #[test]
    fn wait_conditions_parse() {
        let args = logs(&["oly", "logs", "s", "--wait", "exit"]);
        assert_eq!(args.wait, Some(WaitCondition::Exit));
        // `--output` reads better for the "wait for new output" intent.
        let args = logs(&["oly", "logs", "s", "--wait", "new"]);
        assert_eq!(args.wait, Some(WaitCondition::Output));
        assert!(Cli::try_parse_from(["oly", "logs", "s", "--wait", "sometimes"]).is_err());
    }

    #[test]
    fn wait_options_only_combine_with_their_condition() {
        assert!(validate(&logs(&["oly", "logs", "s", "--match", "x"])).is_err());
        assert!(validate(&logs(&["oly", "logs", "s", "--wait", "match"])).is_err());
        assert!(
            validate(&logs(&[
                "oly", "logs", "s", "--wait", "exit", "--match", "x"
            ]))
            .is_err()
        );
        assert!(
            validate(&logs(&[
                "oly",
                "logs",
                "s",
                "--wait",
                "exit",
                "--idle-for",
                "1s"
            ]))
            .is_err()
        );
        assert!(
            validate(&logs(&[
                "oly",
                "logs",
                "s",
                "--wait",
                "idle",
                "--idle-for",
                "0"
            ]))
            .is_err()
        );
        assert!(
            validate(&logs(&[
                "oly",
                "logs",
                "s",
                "--wait",
                "match",
                "--match",
                "error.*done"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn match_regex_is_validated_before_waiting() {
        let err = validate(&logs(&[
            "oly",
            "logs",
            "s",
            "--wait",
            "match",
            "--match",
            "(unclosed",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid --match regex"), "{err}");
    }

    #[test]
    fn cursor_query_is_standalone() {
        let args = logs(&["oly", "logs", "s", "--cursor"]);
        assert!(args.cursor);
        assert!(
            Cli::try_parse_from(["oly", "logs", "s", "--cursor", "--tail", "5"]).is_err(),
            "--cursor prints only the token; a print flag alongside it is a mistake"
        );
        assert!(Cli::try_parse_from(["oly", "logs", "s", "--cursor", "--since", "tok"]).is_err());
    }

    #[test]
    fn durations_accept_units() {
        let args = logs(&[
            "oly",
            "logs",
            "s",
            "--wait",
            "idle",
            "--idle-for",
            "1500ms",
            "--timeout",
            "2m",
        ]);
        assert_eq!(args.idle_for, Some(1500));
        assert_eq!(args.timeout, Some(120_000));
    }

    #[test]
    fn line_matching_is_page_independent_bounded_and_prompt_friendly() {
        let regex = Regex::new("BEGIN.*END").unwrap();
        let mut line = Vec::new();
        assert!(!match_lines(&regex, &mut line, b"BEGIN").unwrap());
        assert!(!match_lines(&regex, &mut line, &vec![b'x'; 8192]).unwrap());
        assert!(match_lines(&regex, &mut line, b"END").unwrap());
        line.clear();
        assert!(!match_lines(&regex, &mut line, b"BEGIN\nEND\n").unwrap());
        assert!(match_lines(&Regex::new("ready>").unwrap(), &mut line, b"ready>").unwrap());
        line.clear();
        assert!(match_lines(&regex, &mut line, &vec![b'x'; MATCH_LINE_BYTES + 1]).is_err());
    }

    #[test]
    fn ambiguous_combinations_and_limits_are_rejected() {
        for args in [
            vec!["oly", "logs", "s", "--timeout", "1s"],
            vec!["oly", "logs", "s", "--idle-for", "1s"],
            vec![
                "oly",
                "logs",
                "s",
                "--wait",
                "match",
                "--match",
                "x",
                "--idle-for",
                "1s",
            ],
            vec!["oly", "logs", "s", "--tail", "65536"],
            vec!["oly", "logs", "s", "--tail-frames", "1025"],
            vec!["oly", "logs", "s", "--since", "tok", "--limit-bytes", "0"],
        ] {
            assert!(validate(&logs(&args)).is_err(), "{args:?}");
        }
        for surface in ["--tail", "--tail-frames"] {
            assert!(
                Cli::try_parse_from(["oly", "logs", "s", "--since", "tok", surface, "1"]).is_err()
            );
        }
        assert!(Cli::try_parse_from(["oly", "logs", "s", "--since", "tok", "--screen"]).is_err());
        assert!(Cli::try_parse_from(["oly", "logs", "s", "--limit-bytes", "1"]).is_err());
    }

    #[test]
    fn inactive_note_carries_the_exit_code() {
        assert_eq!(
            inactive_note("abc", Some("exited"), Some(3)).as_deref(),
            Some("--- Session abc is exited (exit code 3) ---")
        );
        assert_eq!(inactive_note("abc", Some("running"), None), None);
        assert_eq!(inactive_note("abc", None, None), None);
    }
}
