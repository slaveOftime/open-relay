//! `oly logs` read surface: resolve what to print (text tail, visible
//! screen, screen-snapshot history) and the stream cursor it was taken at.
//!
//! Kept out of `rpc.rs`'s per-variant dispatch because serving one `logs`
//! read fans out across several session-subsystem entry points (live
//! render, journal replay render, screen-history replay) that all need the
//! same status and cursor bookkeeping.

use std::path::Path;
use std::sync::Arc;

use crate::db::Database;
use crate::protocol::{LogFrame, RpcResponse, StreamPosition};
use crate::session::logs::{self, ScreenSnapshot, collect_screen_history};

use super::SessionStoreHandle;

/// Render an explicit view and return its observation position. Views are
/// deliberately lossy; exact byte delivery uses bounded ObserveWindow pages.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_logs_read(
    id: String,
    mode: String,
    count: usize,
    from: Option<StreamPosition>,
    keep_color: bool,
    term_cols: u16,
    session_store: &SessionStoreHandle,
    db: &Arc<Database>,
) -> RpcResponse {
    let session_dir = match db.get_session_dir(&id).await {
        Ok(Some(dir)) => dir,
        Ok(None) => {
            return RpcResponse::Error {
                message: format!("session not found: {id}"),
            };
        }
        Err(err) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
    };

    let (running, _closed, exit_code) = session_store
        .attach_stream_status(&id)
        .await
        .unwrap_or((false, true, None));
    let status = match session_store.get_summary(&id) {
        Some(summary) => Some(summary.status),
        None => db
            .get_session(&id)
            .await
            .ok()
            .flatten()
            .map(|meta| meta.status.as_str().to_string()),
    };
    let incarnation = session_store.journal_incarnation(&id);
    // `journal_incarnation` only answers for live sessions; a finished one
    // still has exactly one incarnation on disk, which is what the cursor
    // token needs.
    let incarnation = match incarnation {
        Some(value) => Some(value),
        None => disk_incarnation(session_dir.clone()).await,
    };
    // A cursor names a position in one incarnation's byte space. After a
    // restart the numbering refers to a different stream, so a stale cursor
    // is an error rather than a silently wrong window.
    if let Some(from) = from
        && let Some(current) = incarnation
        && from.incarnation != current
    {
        return RpcResponse::Error {
            message: format!(
                "cursor is stale: it points at incarnation {} but this session is now incarnation {}",
                from.incarnation, current
            ),
        };
    }
    let from_offset = from.map(|position| position.offset).unwrap_or(0);

    // A caller with no viewport (agent, JSON consumer) asks with 0 columns:
    // render at the parser width so nothing wraps at a grid edge.
    let term_cols = if term_cols == 0 {
        logs::PARSER_COLS
    } else {
        term_cols
    };

    if from.is_some() {
        return RpcResponse::Error {
            message: "rendered views do not accept byte continuation cursors; use ObserveWindow"
                .into(),
        };
    }
    let resolved = match mode.as_str() {
        "tail" if count <= 65535 => Mode::Tail,
        "screen" => Mode::Screen,
        "frames" if count <= 1024 => Mode::Frames,
        _ => {
            return RpcResponse::Error {
                message: "invalid logs mode or count".into(),
            };
        }
    };

    // Live fast path: a running session with no cursor and an explicit
    // `--screen` reads straight from the engine — the engine already has
    // the live state, no journal replay needed, cheaper than walking every
    // byte. Other modes (text tail, frames) use the journal path so the
    // returned cursor is an exact filtered offset the next `--since` can
    // chain onto without skipping mid-render output.
    if running && matches!(resolved, Mode::Screen) && from_offset == 0 {
        // Atomic snapshot: capture the cursor under the SAME read lock that
        // produced the engine rows, so future-byte observation starts at
        // the exact state represented by this view.
        let snapshot = match session_store
            .snapshot_live_render(&id, usize::MAX, keep_color, term_cols)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(crate::session::SessionError::NotRunning) => {
                // Completion raced the initial status read; the retained
                // runtime still has the last screen, so retry via journal below.
                return stopped_screen(
                    session_dir,
                    incarnation,
                    keep_color,
                    session_store,
                    &id,
                    status,
                )
                .await;
            }
            Err(err) => {
                return RpcResponse::Error {
                    message: format!("{err:?}"),
                };
            }
        };
        let cursor = consumed_position(snapshot.captured_offset, snapshot.incarnation);
        if snapshot.incarnation != session_store.journal_incarnation(&id) {
            return RpcResponse::Error {
                message: "session restarted during snapshot".into(),
            };
        }
        return RpcResponse::LogsRead {
            mode: "screen".to_string(),
            output: snapshot.output,
            frames: Vec::new(),
            width: term_cols,
            cursor,
            running,
            exit_code,
            status,
        };
    }

    // Cold terminal reconstruction can scan the whole recording. Per-key
    // caches make warm reads incremental; all work stays off async workers.
    let dir_for_task = session_dir.clone();
    let served = tokio::task::spawn_blocking(move || match resolved {
        Mode::Tail if count == 0 => {
            // Zero means no rendering or observation advance.
            Ok(Served::Text(Vec::new(), from_offset))
        }
        Mode::Tail => {
            // Reuse the cached terminal/scanner replay that screen history
            // maintains — one engine read feeds both the bytes and the
            // observation cursor.
            let (output, consumed) =
                logs::render_log_session_tail(&dir_for_task, count, keep_color)
                    .map_err(|err| err.to_string())?;
            Ok(Served::Text(output, consumed))
        }
        Mode::Screen => logs::current_recorded_screen(&dir_for_task, keep_color)
            .map(|history| {
                Served::Text(
                    history
                        .snapshots
                        .into_iter()
                        .next()
                        .map(|s| frame_payload(s, keep_color).content)
                        .unwrap_or_default(),
                    history.end_position.offset,
                )
            })
            .map_err(|err| err.to_string()),
        Mode::Frames => collect_screen_history(&dir_for_task, from_offset, count, keep_color)
            .map(|history| {
                Served::Frames(history.snapshots, history.text_width, history.end_position)
            })
            .map_err(|err| err.to_string()),
    })
    .await;

    let served = match served {
        Ok(Ok(served)) => served,
        Ok(Err(message)) => return RpcResponse::Error { message },
        Err(join_err) => {
            return RpcResponse::Error {
                message: format!("log render worker join failed: {join_err}"),
            };
        }
    };

    let current = match session_store.journal_incarnation(&id) {
        Some(value) => Some(value),
        None => disk_incarnation(session_dir.clone()).await,
    };
    if current != incarnation {
        return RpcResponse::Error {
            message: "session restarted during log replay".into(),
        };
    }
    let (output, frames, width, cursor) = match served {
        Served::Text(output, consumed) => (
            output,
            Vec::new(),
            term_cols,
            consumed_position(consumed, incarnation),
        ),
        Served::Frames(snapshots, width, end_position) => (
            Vec::new(),
            snapshots
                .into_iter()
                .map(|snapshot| frame_payload(snapshot, keep_color))
                .collect(),
            width,
            Some(end_position),
        ),
    };
    let mode = match resolved {
        Mode::Tail => "tail",
        Mode::Screen => "screen",
        Mode::Frames => "frames",
    };

    RpcResponse::LogsRead {
        mode: mode.to_string(),
        output,
        frames,
        width,
        cursor,
        running,
        exit_code,
        status,
    }
}

async fn stopped_screen(
    dir: std::path::PathBuf,
    incarnation: Option<u64>,
    keep_color: bool,
    store: &SessionStoreHandle,
    id: &str,
    status: Option<String>,
) -> RpcResponse {
    let replay_dir = dir.clone();
    let history =
        tokio::task::spawn_blocking(move || logs::current_recorded_screen(&replay_dir, keep_color))
            .await;
    let history = match history {
        Ok(Ok(history)) => history,
        Ok(Err(err)) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
        Err(err) => {
            return RpcResponse::Error {
                message: err.to_string(),
            };
        }
    };
    let current = match store.journal_incarnation(id) {
        Some(value) => Some(value),
        None => disk_incarnation(dir).await,
    };
    if Some(history.end_position.incarnation) != incarnation || current != incarnation {
        return RpcResponse::Error {
            message: "session restarted during screen replay".into(),
        };
    }
    let (_, _, exit_code) = store
        .attach_stream_status(id)
        .await
        .unwrap_or((false, true, None));
    RpcResponse::LogsRead {
        mode: "screen".into(),
        output: history
            .snapshots
            .into_iter()
            .next()
            .map(|s| frame_payload(s, keep_color).content)
            .unwrap_or_default(),
        frames: vec![],
        width: history.text_width,
        cursor: Some(history.end_position),
        running: false,
        exit_code,
        status,
    }
}

enum Served {
    Text(Vec<u8>, u64),
    Frames(Vec<ScreenSnapshot>, u16, StreamPosition),
}

#[derive(Clone, Copy)]
enum Mode {
    Tail,
    Screen,
    Frames,
}

/// Render one captured screen into a frame payload; the caller labels and
/// joins frames, so each payload carries exactly its screen's rows.
fn frame_payload(snapshot: ScreenSnapshot, _keep_color: bool) -> LogFrame {
    // Rows were captured at the recorded viewport width, so no column
    // trimming is applied; blank rows around the content are dropped and
    // rows joined exactly like every other rendered-log surface.
    let content = logs::finish_render(snapshot.rows_bytes, usize::MAX, _keep_color);
    LogFrame {
        position: snapshot.position,
        rows: snapshot.rows,
        cols: snapshot.cols,
        content,
    }
}

/// Build a cursor from a known-consumed filtered offset.
fn consumed_position(consumed_offset: u64, incarnation: Option<u64>) -> Option<StreamPosition> {
    incarnation.map(|incarnation| StreamPosition {
        incarnation,
        offset: consumed_offset,
    })
}

async fn disk_incarnation(dir: std::path::PathBuf) -> Option<u64> {
    tokio::task::spawn_blocking(move || latest_journal_incarnation(&dir))
        .await
        .ok()
        .flatten()
}

/// Newest incarnation recorded on disk (for sessions with no live runtime).
fn latest_journal_incarnation(session_dir: &Path) -> Option<u64> {
    let journal_dir = session_dir.join(crate::session::journal::JOURNAL_DIR_NAME);
    crate::session::journal::list_incarnations(&journal_dir)
        .ok()?
        .last()
        .copied()
}
