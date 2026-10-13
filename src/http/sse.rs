use std::convert::Infallible;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures_util::{Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::broadcast;

use tracing::{debug, warn};

use crate::session::SessionEvent;

use super::AppState;

/// Bounded broadcast capacity for the **encoded** event channel. The frontend
/// EventSource MUST close and reconnect when it lags past this number of
/// messages -- missing events silently break the rendered table because
/// activity and structural updates become inconsistent.
pub(crate) const SSE_BROADCAST_CAPACITY: usize = 256;

/// Event names the frontend sees. ``&'static str`` keeps the per-connection
/// payload allocation-free; only the JSON ``data`` is constructed at runtime.
const EVT_READY: &str = "stream_ready";
const EVT_NODES: &str = "node_state";
const EVT_RESYNC: &str = "resync_required";
const EVT_CREATED: &str = "session_created";
const EVT_UPDATED: &str = "session_updated";
const EVT_DELETED: &str = "session_deleted";
const EVT_NOTIFICATION: &str = "session_notification";
const EVT_ACTIVITY: &str = "session_activity";

#[derive(Debug, Clone)]
pub struct EncodedSessionEvent {
    pub event_name: &'static str,
    pub data: String,
}

pub(crate) fn encode_session_event(event: &SessionEvent) -> EncodedSessionEvent {
    match event {
        SessionEvent::SessionCreated(summary) => EncodedSessionEvent {
            event_name: EVT_CREATED,
            data: serde_json::to_string(&summary.for_delivery(None)).unwrap_or_default(),
        },
        SessionEvent::SessionUpdated(summary) => EncodedSessionEvent {
            event_name: EVT_UPDATED,
            data: serde_json::to_string(&summary.for_delivery(None)).unwrap_or_default(),
        },
        SessionEvent::SessionDeleted { id, node } => EncodedSessionEvent {
            event_name: EVT_DELETED,
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
            enabled_for_channels: _,
        } => EncodedSessionEvent {
            event_name: EVT_NOTIFICATION,
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
        SessionEvent::SessionActivity(batch) => EncodedSessionEvent {
            event_name: EVT_ACTIVITY,
            data: serde_json::to_string(&batch).unwrap_or_default(),
        },
        SessionEvent::ResyncRequired { node, reason } => EncodedSessionEvent {
            event_name: EVT_RESYNC,
            data: serde_json::to_string(&serde_json::json!({
                "node": node,
                "reason": reason,
            }))
            .unwrap_or_default(),
        },
        SessionEvent::NodeState {
            node,
            connected,
            last_seen,
        } => EncodedSessionEvent {
            event_name: EVT_NODES,
            data: serde_json::to_string(&serde_json::json!({
                "node": node,
                "connected": connected,
                "last_seen": last_seen,
            }))
            .unwrap_or_default(),
        },
    }
}

/// Single-source-of-truth encoder fan-out. Subscribed to the raw
/// `event_tx: broadcast::Sender<SessionEvent>` and re-broadcasts the
/// already-encoded bytes onto `event_bytes_tx`, so each SSE handler that
/// subscribes does **not** re-serialize events for every reader.
///
/// On `Lagged`, the encoder publishes a `resync_required` event so
/// subscribers learn the stream diverged. The relay-side uses the same
/// `ResyncRequired` to surface relay losses to its peers.
pub fn run_event_encoder(
    event_tx: broadcast::Sender<SessionEvent>,
    event_bytes_tx: broadcast::Sender<EncodedSessionEvent>,
) {
    let mut rx = event_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let delivered = event.for_delivery(None);
                    let encoded = encode_session_event(&delivered);
                    // No browsers connected is normal. Keep draining raw events
                    // so future subscribers still receive live activity.
                    let _ = event_bytes_tx.send(encoded);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "session event broadcast lagged; emitting resync");
                    let payload = EncodedSessionEvent {
                        event_name: EVT_RESYNC,
                        data: serde_json::to_string(&serde_json::json!({
                            "node": Option::<String>::None,
                            "reason": "sender_lagged",
                        }))
                        .unwrap_or_default(),
                    };
                    let _ = event_bytes_tx.send(payload);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// A lagged client receives an explicit gap marker, then EOF so it reconnects.
fn live_event_stream(
    rx: broadcast::Receiver<EncodedSessionEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::unfold((rx, false), |(mut rx, ended)| async move {
        if ended {
            return None;
        }
        match rx.recv().await {
            Ok(encoded) => Some((
                Ok(Event::default()
                    .event(encoded.event_name)
                    .data(encoded.data)),
                (rx, false),
            )),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "SSE stream lagged; emitting resync and closing");
                let data = serde_json::json!({
                    "node": null,
                    "reason": "receiver_lagged",
                });
                Some((
                    Ok(Event::default().event(EVT_RESYNC).data(data.to_string())),
                    (rx, true),
                ))
            }
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
}

/// Build the ``stream_ready`` payload. ``version`` lets the frontend
/// negotiate the wire format; ``nodes`` carries the currently connected
/// secondary list so the browser knows which scopes to expect.
async fn build_stream_ready(state: &AppState) -> serde_json::Value {
    let names = state.node_registry.connected_names().await;
    let mut names = names;
    names.sort();
    serde_json::json!({
        "version": SSE_VERSION,
        "nodes": names,
    })
}

/// Constants tests reference; re-exports prevent churn in the
/// ``events_handler`` ABI while we shape the protocol.
pub const SSE_VERSION: u32 = 2;

/// SSE handler shape:
///
///   1. Subscribe to `event_bytes_tx` (already-encoded frames) BEFORE
///      constructing ``stream_ready`` so events that fire during readiness
///      are not lost.
///   2. Emit a single ``stream_ready`` per connect (version + connected
///      nodes). Per CR-3, NO inventory ``snapshot``: the frontend fetches
///      the list over REST after seeing ``stream_ready`` so a sluggish SSE
///      connection cannot leave a stale table on every page reload.
///   3. Forward every pre-encoded broadcast frame verbatim.
///   4. On bounded-ring lag (owning or relay path), the safest default is
///      to **close** the stream: the EventSource auto-reconnects, receives a
///      fresh ``stream_ready``, and the browser pulls REST again. Emit
///      `resync_required` before closing so the client marks the gap.
pub async fn events_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.event_bytes_tx.subscribe();

    let stream_ready = build_stream_ready(&state).await;
    debug!(
        nodes = ?stream_ready.get("nodes"),
        "SSE client connected"
    );

    let ready_event = Event::default()
        .event(EVT_READY)
        .data(stream_ready.to_string());

    let initial_stream =
        futures_util::stream::once(async move { Ok::<Event, Infallible>(ready_event) });

    let live_stream = live_event_stream(rx);

    let mut sse_stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(initial_stream.chain(live_stream));

    // ADR-0007: poll revocations as part of the stream itself. Dropping the
    // response drops this future too; no watcher task outlives the browser.
    if let (Some(auth), Some(token)) = (
        state.auth.clone(),
        crate::http::auth::extract_request_token_parts(&headers, query.as_deref()),
    ) {
        let mut revocations = auth.revocation_watch();
        let stop_signal = async move {
            loop {
                if !auth.validate_token(&token).await {
                    debug!("SSE stream closed: session token revoked");
                    return;
                }
                if revocations.changed().await.is_err() {
                    return;
                }
            }
        };
        sse_stream = Box::pin(sse_stream.take_until(stop_signal));
    }

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::{encode_session_event, live_event_stream, run_event_encoder};
    use crate::{
        protocol::SessionSummary,
        session::{SessionActivityBatch, SessionActivitySample, SessionEvent},
    };
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
            created_at: Utc.with_ymd_and_hms(2026, 3, 21, 10, 11, 12).unwrap(),
            started_at: None,
            ended_at: None,
            resume_command: None,
            cwd: Some("C:\\work".to_string()),
            input_needed: true,
            notifications_enabled: false,
            node: None,
            last_total_bytes: 0,
            last_output_epoch: None,
            rows: None,
            cols: None,
            attach_count: 0,
            foreground_color: None,
            background_color: None,
            journal_bytes_retained: None,
            journal_retention_sweeps: None,
            journal_incarnations_dropped: None,
            journal_byte_cap: None,
        }
    }

    #[tokio::test]
    async fn lagged_sse_stream_emits_resync_then_closes() {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        let event = encode_session_event(&SessionEvent::SessionUpdated(sample_summary()));
        tx.send(event.clone()).unwrap();
        tx.send(event).unwrap();
        let stream = live_event_stream(rx);
        futures_util::pin_mut!(stream);
        use futures_util::StreamExt;
        let marker = stream.next().await.unwrap().unwrap();
        assert!(format!("{marker:?}").contains("resync_required"));
        assert!(
            stream.next().await.is_none(),
            "lagged stream must close, not skip events"
        );
    }

    #[tokio::test]
    async fn encoder_survives_periods_without_sse_clients() {
        let (event_tx, _) = tokio::sync::broadcast::channel(16);
        let (encoded_tx, _) = tokio::sync::broadcast::channel(16);
        run_event_encoder(event_tx.clone(), encoded_tx.clone());

        // Output before the first browser connects must not kill the encoder.
        event_tx
            .send(SessionEvent::SessionUpdated(sample_summary()))
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            event_tx.receiver_count(),
            1,
            "encoder exited before a browser connected"
        );

        let mut browser = encoded_tx.subscribe();
        event_tx
            .send(SessionEvent::SessionActivity(SessionActivityBatch {
                node: None,
                samples: vec![SessionActivitySample {
                    id: "sess-123".to_string(),
                    last_total_bytes: 4096,
                    last_output_at: None,
                }],
            }))
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), browser.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event_name, "session_activity");

        // The last browser disconnecting is also a normal idle period.
        drop(browser);
        event_tx
            .send(SessionEvent::SessionUpdated(sample_summary()))
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            event_tx.receiver_count(),
            1,
            "encoder exited after the last browser disconnected"
        );
        let mut browser = encoded_tx.subscribe();
        event_tx
            .send(SessionEvent::SessionUpdated(sample_summary()))
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), browser.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.event_name, "session_updated");
    }

    #[test]
    fn delivery_helper_applies_node_to_summary_events() {
        let event = SessionEvent::SessionUpdated(sample_summary());
        let delivered = event.for_delivery(Some("worker-a"));

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
            enabled_for_channels: false,
        };

        let delivered = event.for_delivery(Some("worker-a"));
        let encoded = encode_session_event(&delivered);

        assert_eq!(encoded.event_name, "session_notification");
        assert!(encoded.data.contains("\"node\":\"worker-a\""));
    }

    #[test]
    fn activity_event_round_trips_with_samples() {
        let event = SessionEvent::SessionActivity(SessionActivityBatch {
            node: Some("worker-a".to_string()),
            samples: vec![SessionActivitySample {
                id: "sess-123".to_string(),
                last_total_bytes: 4096,
                last_output_at: Some(Utc.with_ymd_and_hms(2026, 3, 21, 10, 11, 12).unwrap()),
            }],
        });
        let encoded = encode_session_event(&event);
        assert_eq!(encoded.event_name, "session_activity");
        assert!(encoded.data.contains("\"node\":\"worker-a\""));
        assert!(encoded.data.contains("\"last_total_bytes\":4096"));
    }

    #[test]
    fn resync_required_round_trips_through_encoder() {
        let event = SessionEvent::ResyncRequired {
            node: Some("worker-a".to_string()),
            reason: Some("receiver_lagged".to_string()),
        };
        let encoded = encode_session_event(&event);
        assert_eq!(encoded.event_name, "resync_required");
        assert!(encoded.data.contains("\"node\":\"worker-a\""));
        assert!(encoded.data.contains("\"reason\":\"receiver_lagged\""));
    }

    #[test]
    fn node_state_round_trips_through_encoder() {
        let event = SessionEvent::NodeState {
            node: "worker-a".to_string(),
            connected: false,
            last_seen: None,
        };
        let encoded = encode_session_event(&event);
        assert_eq!(encoded.event_name, "node_state");
        assert!(encoded.data.contains("\"node\":\"worker-a\""));
        assert!(encoded.data.contains("\"connected\":false"));
    }
}
