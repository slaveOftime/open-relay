use std::convert::Infallible;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;

use tracing::{debug, warn};

use crate::protocol::ListQuery;

use super::AppState;

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Snapshot of all current sessions sent as the first event
    let initial = {
        let mut store = state.store.lock().await;
        let q = ListQuery {
            search: None,
            statuses: vec![],
            since: None,
            until: None,
            limit: 200,
            offset: 0,
            sort: None,
            order: None,
        };
        store.list_summaries(&q).await.unwrap_or_default()
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
