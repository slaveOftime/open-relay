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
    let results = futures_util::future::join_all(requests).await;
    Ok(collect_session_results(results, query.limit))
}

fn collect_session_results(
    results: impl IntoIterator<Item = (Option<String>, Result<Vec<SessionSummary>>)>,
    limit: usize,
) -> SessionRefresh {
    let mut sessions = Vec::new();
    let mut failed_nodes = HashSet::new();
    let mut failures = Vec::new();
    for (node, result) in results {
        match result {
            Ok(target_sessions) => sessions.extend(target_sessions),
            Err(error) => {
                failures.push(format!("{}: {error}", node.as_deref().unwrap_or("local")));
                failed_nodes.insert(node);
            }
        }
    }
    sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at));
    sessions.truncate(limit);
    SessionRefresh {
        sessions,
        failed_nodes,
        failures,
    }
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

pub fn remove_sessions(config: &AppConfig, app: &mut App, targets: Vec<SessionTarget>) {
    if targets.is_empty() {
        return;
    }
    let requests = targets.iter().map(remove_request).collect::<Vec<_>>();
    let config = config.clone();
    tokio::spawn(async move {
        let _ = futures_util::future::join_all(
            requests
                .into_iter()
                .map(|request| ipc::send_request_checked(&config, request)),
        )
        .await;
    });
    for target in &targets {
        app.remove_session_payload(&target.id, target.node.as_deref());
    }
    let message = if targets.len() == 1 {
        format!("removing session {}", targets[0].id)
    } else {
        format!("removing {} sessions", targets.len())
    };
    app.set_action_message(Some(message));
}
/// Apply the force-remove result only after the duplicate was created. A
/// refusal leaves the original in the list and makes the partial success
/// visible instead of silently pretending the original was removed.
pub(super) fn apply_clone_remove_response(
    app: &mut App,
    new_id: &str,
    source: &SessionTarget,
    response: Result<RpcResponse>,
) -> (String, bool) {
    let started = format!("started new session {new_id}");
    match response {
        Ok(RpcResponse::Remove { removed: true }) => {
            app.remove_session_payload(&source.id, source.node.as_deref());
            (format!("{started} · removed original {}", source.id), false)
        }
        Ok(RpcResponse::Remove { removed: false }) => (
            format!(
                "{started} · original {} was not removed (not found)",
                source.id
            ),
            true,
        ),
        Ok(_) => (
            format!(
                "{started} · original {} was not removed (unexpected response)",
                source.id
            ),
            true,
        ),
        Err(error) => (
            format!(
                "{started} · original {} was not removed: {error}",
                source.id
            ),
            true,
        ),
    }
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
            app.resume_dialog = None;
            let (feedback, removal_failed) = if let Some(source) = &launch.remove_source {
                // Never force-remove the newly created session even if a
                // buggy daemon were to return the original ID on the same node.
                if source.id == session_id && source.node.as_deref() == launch.node.as_deref() {
                    (
                        format!(
                            "started new session {session_id} · original was not removed (same ID)"
                        ),
                        true,
                    )
                } else {
                    let response = ipc::send_request_checked(config, remove_request(source)).await;
                    apply_clone_remove_response(app, &session_id, source, response)
                }
            } else {
                (format!("started new session {session_id}"), false)
            };
            app.set_action_message(Some(feedback.clone()));
            if launch.attach_after_start {
                open_session_inline(terminal, app, &session_id, launch.node.as_deref(), true)?;
                if removal_failed {
                    let attach_feedback = app.message.as_deref().unwrap_or_default();
                    app.set_action_message(Some(format!("{feedback} · {attach_feedback}")));
                }
            }
        }
        Ok(_) if app.resume_dialog.is_some() => {
            set_resume_error(app, "unexpected response type".to_string());
        }
        Ok(_) => set_clone_error(app, "unexpected response type".to_string()),
        Err(error) if app.resume_dialog.is_some() => {
            set_resume_error(app, format!("resume failed: {error}"));
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

pub fn set_resume_error(app: &mut App, error: String) {
    if let Some(dialog) = app.resume_dialog.as_mut() {
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

#[cfg(test)]
mod tests {
    use super::collect_session_results;
    use crate::error::AppError;
    use std::collections::HashSet;

    #[test]
    fn all_failed_node_refreshes_are_retryable_results() {
        let refresh = collect_session_results(
            vec![(
                Some("worker-a".to_string()),
                Err(AppError::NodeNotConnected("worker-a".to_string())),
            )],
            100,
        );

        assert!(refresh.sessions.is_empty());
        assert_eq!(
            refresh.failed_nodes,
            HashSet::from([Some("worker-a".to_string())])
        );
        assert_eq!(
            refresh.warning().as_deref(),
            Some("sync lost: worker-a: node not connected: worker-a")
        );
    }

    #[test]
    fn empty_target_refresh_is_a_valid_empty_snapshot() {
        let refresh = collect_session_results(Vec::new(), 100);
        assert!(refresh.sessions.is_empty());
        assert!(refresh.failed_nodes.is_empty());
        assert!(refresh.failures.is_empty());
    }
}
