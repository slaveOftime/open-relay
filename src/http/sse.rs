use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use tracing::{debug, info, warn};

use crate::{
    protocol::{ListQuery, ListSortField, SessionSummary, SortOrder},
    session::{SessionEvent, SessionLiveSummary, SessionStore},
};

use super::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionFingerprint {
    status: String,
    pid: Option<u32>,
    input_needed: bool,
    notifications_enabled: bool,
    last_output_at: Option<Instant>,
    last_total_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct EncodedSessionEvent {
    pub event_name: &'static str,
    pub data: String,
}

impl From<&SessionLiveSummary> for SessionFingerprint {
    fn from(value: &SessionLiveSummary) -> Self {
        Self {
            status: value.summary.status.clone(),
            pid: value.summary.pid,
            input_needed: value.summary.input_needed,
            notifications_enabled: value.summary.notifications_enabled,
            last_output_at: value.last_output_at,
            last_total_bytes: value.summary.total_bytes,
        }
    }
}

fn merge_node(existing: &Option<String>, node: Option<&str>) -> Option<String> {
    existing.clone().or_else(|| node.map(str::to_string))
}

pub(crate) fn session_summary_for_delivery(
    summary: &SessionSummary,
    node: Option<&str>,
) -> SessionSummary {
    let mut summary = summary.clone();
    summary.node = merge_node(&summary.node, node);
    summary
}

pub(crate) fn session_event_for_delivery(event: &SessionEvent, node: Option<&str>) -> SessionEvent {
    match event {
        SessionEvent::SessionCreated(summary) => {
            SessionEvent::SessionCreated(session_summary_for_delivery(summary, node))
        }
        SessionEvent::SessionUpdated(summary) => {
            SessionEvent::SessionUpdated(session_summary_for_delivery(summary, node))
        }
        SessionEvent::SessionDeleted { id, node: existing } => SessionEvent::SessionDeleted {
            id: id.clone(),
            node: merge_node(existing, node),
        },
        SessionEvent::SessionNotification {
            kind,
            title,
            description,
            body,
            navigation_url,
            session_ids,
            trigger_rule,
            trigger_detail,
            node: existing,
            last_total_bytes,
        } => SessionEvent::SessionNotification {
            kind: kind.clone(),
            title: title.clone(),
            description: description.clone(),
            body: body.clone(),
            navigation_url: navigation_url.clone(),
            session_ids: session_ids.clone(),
            trigger_rule: trigger_rule.clone(),
            trigger_detail: trigger_detail.clone(),
            node: merge_node(existing, node),
            last_total_bytes: last_total_bytes.clone(),
        },
    }
}

pub(crate) fn encode_session_event(event: &SessionEvent) -> EncodedSessionEvent {
    match event {
        SessionEvent::SessionCreated(summary) => EncodedSessionEvent {
            event_name: "session_created",
            data: serde_json::to_string(summary).unwrap_or_default(),
        },
        SessionEvent::SessionUpdated(summary) => EncodedSessionEvent {
            event_name: "session_updated",
            data: serde_json::to_string(summary).unwrap_or_default(),
        },
        SessionEvent::SessionDeleted { id, node } => EncodedSessionEvent {
            event_name: "session_deleted",
            data: serde_json::to_string(&serde_json::json!({
                "id": id,
                "node": node,
            }))
            .unwrap_or_default(),
        },
        SessionEvent::SessionNotification {
            kind,
            title,
            description,
            body,
            navigation_url,
            session_ids,
            trigger_rule,
            trigger_detail,
            node,
            last_total_bytes,
        } => EncodedSessionEvent {
            event_name: "session_notification",
            data: serde_json::to_string(&serde_json::json!({
                "kind": kind,
                "title": title,
                "description": description,
                "body": body,
                "navigation_url": navigation_url,
                "session_ids": session_ids,
                "trigger_rule": trigger_rule,
                "trigger_detail": trigger_detail,
                "node": node,
                "last_total_bytes": last_total_bytes,
            }))
            .unwrap_or_default(),
        },
    }
}

// ── Session poller ────────────────────────────────────────────────────────────

/// Background task that polls live in-memory sessions every 500 ms and emits
/// `SessionEvent::SessionUpdated` only when a session's fingerprint changes.
/// This avoids a database round-trip on every tick.
pub(super) async fn run_session_poller(
    store: Arc<SessionStore>,
    event_tx: broadcast::Sender<SessionEvent>,
) {
    info!("session poller started");
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    let mut last_sent: HashMap<String, SessionFingerprint> = HashMap::new();
    let mut initialized = false;

    loop {
        interval.tick().await;
        let sessions = store.list_live_summaries();

        let mut seen_ids = HashSet::with_capacity(last_sent.len());

        // First poll establishes a baseline without emitting any events.
        if !initialized {
            for s in sessions {
                seen_ids.insert(s.summary.id.clone());
                last_sent.insert(s.summary.id.clone(), SessionFingerprint::from(&s));
            }
            initialized = true;
            continue;
        }

        for s in sessions {
            seen_ids.insert(s.summary.id.clone());
            let fp = SessionFingerprint::from(&s);

            let changed = match last_sent.get(&s.summary.id) {
                Some(prev) => prev != &fp,
                None => true,
            };

            if changed {
                debug!(
                    id = %s.summary.id,
                    status = %s.summary.status,
                    "session state changed, broadcasting update"
                );
                last_sent.insert(s.summary.id.clone(), fp);
                let _ = event_tx.send(SessionEvent::SessionUpdated(s.summary));
            }
        }

        // Drop fingerprints for sessions no longer in memory.
        last_sent.retain(|id, _| seen_ids.contains(id));
    }
}

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Snapshot of all current sessions sent as the first event
    let initial = {
        let q = ListQuery {
            search: None,
            statuses: vec![],
            since: None,
            until: None,
            limit: 200,
            offset: 0,
            sort: ListSortField::CreatedAt,
            order: SortOrder::Desc,
        };
        state
            .store
            .list_summaries(&q)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|summary| session_summary_for_delivery(&summary, None))
            .collect::<Vec<_>>()
    };

    debug!(snapshot_count = initial.len(), "SSE client connected");

    let snapshot_event = Event::default()
        .event("snapshot")
        .data(serde_json::to_string(&initial).unwrap_or_default());

    let initial_stream =
        futures_util::stream::once(async move { Ok::<Event, Infallible>(snapshot_event) });

    let rx = state.event_tx.subscribe();
    let live_stream = BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(ev) => {
                let encoded = encode_session_event(&session_event_for_delivery(&ev, None));
                Some(Ok(Event::default()
                    .event(encoded.event_name)
                    .data(encoded.data)))
            }
            Err(_) => {
                warn!("SSE receiver lagged, dropping event");
                None
            }
        }
    });

    Sse::new(initial_stream.chain(live_stream)).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::{encode_session_event, session_event_for_delivery};
    use crate::{protocol::SessionSummary, session::SessionEvent};
    use chrono::{TimeZone, Utc};

    fn sample_summary() -> SessionSummary {
        SessionSummary {
            id: "sess-123".to_string(),
            title: Some("demo".to_string()),
            tags: vec!["prod".to_string()],
            command: "cargo".to_string(),
            args: vec!["test".to_string()],
            pid: Some(42),
            status: "running".to_string(),
            age: "5m".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 3, 21, 10, 11, 12).unwrap(),
            cwd: Some("C:\\work".to_string()),
            input_needed: true,
            notifications_enabled: false,
            node: None,
            total_bytes: 0,
        }
    }

    #[test]
    fn delivery_helper_applies_node_to_summary_events() {
        let event = SessionEvent::SessionUpdated(sample_summary());
        let delivered = session_event_for_delivery(&event, Some("worker-a"));

        let SessionEvent::SessionUpdated(summary) = delivered else {
            panic!("expected session_updated");
        };
        assert_eq!(summary.node.as_deref(), Some("worker-a"));
    }

    #[test]
    fn delivery_helper_applies_node_to_notifications() {
        let event = SessionEvent::SessionNotification {
            kind: "input_needed".to_string(),
            title: "Input required".to_string(),
            description: "Waiting".to_string(),
            body: "Password:".to_string(),
            navigation_url: Some("/session/sess-123?mode=attach".to_string()),
            session_ids: vec!["sess-123".to_string()],
            trigger_rule: Some("regex_pattern".to_string()),
            trigger_detail: None,
            node: None,
            last_total_bytes: 0,
        };

        let delivered = session_event_for_delivery(&event, Some("worker-a"));
        let encoded = encode_session_event(&delivered);

        assert_eq!(encoded.event_name, "session_notification");
        assert!(encoded.data.contains("\"node\":\"worker-a\""));
    }
}
