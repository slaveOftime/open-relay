use std::{any::Any, collections::HashSet, io, time::Duration};

use chrono::Utc;
use crossterm::event::{self, Event};

use super::super::list::ListTarget;
use super::app::App;
use super::app::open_session_inline;
use super::constants::{REFRESH_TIMEOUT, STOP_GRACE_SECONDS};
use super::proto::{CloneLaunch, SessionTarget, SessionUpdate, wrap_node};
use super::terminal::TuiTerminal;
use crate::{
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse, SessionSummary},
};

pub struct SessionRefresh {
    pub sessions: Vec<SessionSummary>,
    pub failed_nodes: HashSet<Option<String>>,
    pub failures: Vec<String>,
}

impl SessionRefresh {
    pub fn warning(&self) -> Option<String> {
        (!self.failures.is_empty()).then(|| format!("sync lost: {}", self.failures.join(" · ")))
    }
}

pub fn apply_refresh(
    app: &mut App,
    query: &crate::protocol::ListQuery,
    result: Result<SessionRefresh>,
) {
    match result {
        Ok(mut refresh) => {
            refresh.sessions.extend(
                app.sessions
                    .iter()
                    .filter(|session| refresh.failed_nodes.contains(&session.node))
                    .cloned(),
            );
            refresh
                .sessions
                .sort_by_key(|session| std::cmp::Reverse(session.created_at));
            refresh.sessions.truncate(query.limit);
            let warning = refresh.warning();
            app.replace_sessions(refresh.sessions);
            app.set_refresh_message(warning);
        }
        Err(error) => app.set_refresh_message(Some(format!("sync lost: {error}"))),
    }
}

pub fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

pub fn read_terminal_event(timeout: Duration) -> io::Result<Option<Event>> {
    read_terminal_event_with(timeout, event::poll, event::read)
}

/// Drain input events that are already queued, without waiting. Typing or
/// pasting produces bursts; handling the whole burst before the next draw
/// renders it as a single frame instead of one frame per keystroke.
pub fn drain_pending_events(events: &mut Vec<Event>) -> io::Result<()> {
    drain_pending_events_with(events, event::poll, event::read)
}

pub fn drain_pending_events_with<P, R>(
    events: &mut Vec<Event>,
    mut poll: P,
    mut read: R,
) -> io::Result<()>
where
    P: FnMut(Duration) -> io::Result<bool>,
    R: FnMut() -> io::Result<Event>,
{
    while let Some(event) = read_terminal_event_with(Duration::ZERO, &mut poll, &mut read)? {
        events.push(event);
    }
    Ok(())
}

pub fn read_terminal_event_with(
    timeout: Duration,
    poll: impl FnOnce(Duration) -> io::Result<bool>,
    read: impl FnOnce() -> io::Result<Event>,
) -> io::Result<Option<Event>> {
    match poll(timeout) {
        Ok(false) => Ok(None),
        Ok(true) => match read() {
            Ok(event) => Ok(Some(event)),
            Err(error) if is_transient_terminal_error(&error) => Ok(None),
            Err(error) => Err(error),
        },
        Err(error) if is_transient_terminal_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn is_transient_terminal_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

pub async fn fetch_sessions(
    config: &AppConfig,
    query: crate::protocol::ListQuery,
    targets: &[ListTarget],
) -> Result<SessionRefresh> {
    let requests = targets.iter().map(|target| {
        let query = query.clone();
        async move {
            let result = async {
                let inner = RpcRequest::List { query };
                let request = match target.node.as_ref() {
                    Some(node) => RpcRequest::NodeProxy {
                        node: node.clone(),
                        inner: Box::new(inner),
                    },
                    None => inner,
                };
                let response = tokio::time::timeout(
                    REFRESH_TIMEOUT,
                    ipc::send_request_checked(config, request),
                )
                .await
                .map_err(|_| AppError::Protocol("session refresh timed out".to_string()))??;
                match response {
                    RpcResponse::List { mut sessions, .. } => {
                        if let Some(node) = target.node.as_ref() {
                            for session in &mut sessions {
                                session.node = Some(node.clone());
                            }
                        }
                        Ok(sessions)
                    }
                    _ => Err(AppError::Protocol("unexpected response type".to_string())),
                }
            }
            .await;
            (target.node.clone(), result)
        }
    });
    let mut sessions = Vec::new();
    let mut failed_nodes = HashSet::new();
    let mut failures = Vec::new();
    let mut successful_targets = 0;
    for (node, result) in futures_util::future::join_all(requests).await {
        match result {
            Ok(target_sessions) => {
                successful_targets += 1;
                sessions.extend(target_sessions);
            }
            Err(error) => {
                failures.push(format!("{}: {error}", node.as_deref().unwrap_or("local")));
                failed_nodes.insert(node);
            }
        }
    }
    if successful_targets == 0 {
        return Err(AppError::Protocol(failures.join(" · ")));
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at));
    sessions.truncate(query.limit);
    Ok(SessionRefresh {
        sessions,
        failed_nodes,
        failures,
    })
}

/// Fire-and-forget `RpcRequest::Remove { force: true }` to the daemon,
/// matches what `oly rm -f <id>` does from the CLI. The row is also
/// dropped locally so the user sees the dismissal immediately; if the
/// daemon refuses, the next refresh tick surfaces the failure via
/// `apply_refresh` warning.
pub fn remove_request(target: &SessionTarget) -> RpcRequest {
    wrap_node(
        target.node.as_deref(),
        RpcRequest::Remove {
            id: target.id.clone(),
            force: true,
        },
    )
}

pub fn remove_session(config: &AppConfig, app: &mut App, target: SessionTarget) {
    let request = remove_request(&target);
    let config = config.clone();
    tokio::spawn(async move {
        let _ = ipc::send_request_checked(&config, request).await;
    });
    app.remove_session_payload(&target.id, target.node.as_deref());
    app.set_action_message(Some(format!("removing session {}", target.id)));
}

pub async fn start_clone(
    config: &AppConfig,
    terminal: &mut TuiTerminal,
    app: &mut App,
    launch: CloneLaunch,
) -> Result<()> {
    match ipc::send_request_checked(config, launch.request()).await {
        Ok(RpcResponse::Start { session_id }) => {
            app.clone_dialog = None;
            app.set_action_message(Some(format!("started new session {session_id}")));
            if launch.attach_after_start {
                open_session_inline(terminal, app, &session_id, launch.node.as_deref(), true)?;
            }
        }
        Ok(_) => {
            set_clone_error(app, "unexpected response type".to_string());
        }
        Err(error) => set_clone_error(app, format!("start failed: {error}")),
    }
    Ok(())
}

pub async fn update_session(config: &AppConfig, app: &mut App, update: SessionUpdate) {
    let target_id = update.id.clone();
    let target_node = update.node.clone();
    let response = ipc::send_request_checked(config, update.request()).await;
    apply_update_response(app, &target_id, target_node.as_deref(), response);
}

pub fn apply_update_response(
    app: &mut App,
    target_id: &str,
    target_node: Option<&str>,
    response: Result<RpcResponse>,
) {
    match response {
        Ok(RpcResponse::Session { mut summary }) => {
            summary.node = target_node.map(str::to_string);
            app.update_dialog = None;
            app.apply_updated_summary(summary);
            app.set_action_message(Some(format!("updated session {target_id}")));
        }
        Ok(_) => set_update_error(app, "unexpected response type".to_string()),
        Err(error) => set_update_error(app, format!("update failed: {error}")),
    }
}

pub fn stop_session(config: &AppConfig, app: &mut App, target: SessionTarget) {
    let request = wrap_node(
        target.node.as_deref(),
        RpcRequest::Stop {
            id: target.id.clone(),
            grace_seconds: STOP_GRACE_SECONDS,
        },
    );
    let config = config.clone();
    tokio::spawn(async move {
        let _ = ipc::send_request_checked(&config, request).await;
    });

    // Optimistically update the row so the user sees immediate feedback;
    // the daemon will confirm on the next refresh cycle.
    if let Some(session) = app
        .sessions
        .iter_mut()
        .find(|s| s.id == target.id && s.node == target.node)
    {
        session.status = "stopped".to_string();
        session.ended_at = Some(Utc::now());
    }

    app.set_action_message(Some(format!("stop signal sent to {}", target.id)));
}

pub fn set_clone_error(app: &mut App, error: String) {
    if let Some(dialog) = app.clone_dialog.as_mut() {
        dialog.error = Some(error);
    } else {
        app.set_action_message(Some(error));
    }
}

pub fn set_update_error(app: &mut App, error: String) {
    if let Some(dialog) = app.update_dialog.as_mut() {
        dialog.error = Some(error);
    } else {
        app.set_action_message(Some(error));
    }
}
