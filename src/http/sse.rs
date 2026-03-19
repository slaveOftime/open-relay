use std::{
    collections::HashMap,
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
    protocol::{ListQuery, ListSortField, SortOrder},
    session::{SessionEvent, SessionLiveSummary, SessionStore},
};

use super::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionFingerprint {
    status: String,
    pid: Option<u32>,
    input_needed: bool,
    last_output_at: Option<Instant>,
}

impl From<&SessionLiveSummary> for SessionFingerprint {
    fn from(value: &SessionLiveSummary) -> Self {
        Self {
            status: value.summary.status.clone(),
            pid: value.summary.pid,
            input_needed: value.summary.input_needed,
            last_output_at: value.last_output_at,
        }
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

        let mut seen_ids = std::collections::HashSet::with_capacity(last_sent.len());

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
        state.store.list_summaries(&q).await.unwrap_or_default()
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
                let (event_name, data) = match &ev {
                    super::SessionEvent::SessionCreated(s) => (
                        "session_created",
                        serde_json::to_string(s).unwrap_or_default(),
                    ),
                    super::SessionEvent::SessionUpdated(s) => (
                        "session_updated",
                        serde_json::to_string(s).unwrap_or_default(),
                    ),
                    super::SessionEvent::SessionDeleted { id } => (
                        "session_deleted",
                        serde_json::to_string(&serde_json::json!({ "id": id })).unwrap_or_default(),
                    ),
                    super::SessionEvent::SessionNotification {
                        kind,
                        summary,
                        body,
                        session_ids,
                        trigger_rule,
                        trigger_detail,
                    } => (
                        "session_notification",
                        serde_json::to_string(&serde_json::json!({
                            "kind": kind,
                            "summary": summary,
                            "body": body,
                            "session_ids": session_ids,
                            "trigger_rule": trigger_rule,
                            "trigger_detail": trigger_detail,
                        }))
                        .unwrap_or_default(),
                    ),
                };
                Some(Ok(Event::default().event(event_name).data(data)))
            }
            Err(_) => {
                warn!("SSE receiver lagged, dropping event");
                None
            }
        }
    });

    Sse::new(initial_stream.chain(live_stream)).keep_alive(KeepAlive::default())
}
