pub mod channel;
pub mod dispatcher;
pub mod event;
pub mod prompt;

use std::{sync::Arc, time::Instant};
use tracing::{debug, info, warn};

use crate::{
    config::AppConfig,
    db::Database,
    http,
    notification::{
        channel::{LocalOsNotificationChannel, NotificationChannel, WebPushChannel},
        dispatcher::Notifier,
        event::{NotificationEvent, NotificationTriggerRule},
        prompt::{compile_prompt_patterns, find_prompt_match, sanitize_body, strip_ansi_for_body},
    },
    session::{SessionStore, SilentCandidate},
};

/// Periodically checks all running sessions for silence and emits local OS
/// notifications once per output epoch. Silence alone is sufficient to
/// trigger a notification. Prompt patterns are used to pick a better body
/// line but do **not** gate delivery.
pub(super) async fn run_notification_monitor(
    session_store: Arc<SessionStore>,
    config: Arc<AppConfig>,
    db: Arc<Database>,
    event_tx: tokio::sync::broadcast::Sender<http::SessionEvent>,
    notification_tx: tokio::sync::broadcast::Sender<NotificationEvent>,
) {
    let silence = std::time::Duration::from_secs(config.silence_seconds);
    let suppression_window = std::time::Duration::from_secs(3);
    let min_notification_interval = std::time::Duration::from_secs(5);
    let patterns = compile_prompt_patterns(&config.prompt_patterns);
    let notifier = build_notifier(db, &config);

    info!(
        silence_seconds = config.silence_seconds,
        min_notification_interval_seconds = min_notification_interval.as_secs(),
        prompt_patterns = patterns.len(),
        "notification monitor started"
    );

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let candidates: Vec<SilentCandidate> =
            session_store.silent_candidates(suppression_window, min_notification_interval);

        if !candidates.is_empty() {
            debug!(count = candidates.len(), "notification candidates detected");
        }

        for candidate in candidates {
            let session_id = candidate.session_id;
            let excerpt = candidate.raw_excerpt;
            let output_epoch = candidate.output_epoch;
            debug!(
                session_id,
                excerpt = excerpt.as_str(),
                output_epoch = ?output_epoch,
                "evaluating notification triggers for candidate"
            );

            let clean = strip_ansi_for_body(&excerpt);

            let (trigger_rule, trigger_detail, body) =
                if let Some(pattern) = find_prompt_match(&excerpt, &patterns) {
                    info!(
                        session_id,
                        trigger_rule = NotificationTriggerRule::RegexPattern.as_str(),
                        pattern = pattern.as_str(),
                        "notification triggered"
                    );
                    let raw = clean
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .find(|line| {
                            patterns
                                .iter()
                                .any(|re| re.as_str() == pattern && re.is_match(line))
                        })
                        .unwrap_or("")
                        .trim();
                    (
                        NotificationTriggerRule::RegexPattern,
                        Some(pattern.clone()),
                        sanitize_body(raw),
                    )
                } else if let Some(llm_detail) = evaluate_llm_direct_trigger(&clean) {
                    info!(
                        session_id,
                        trigger_rule = NotificationTriggerRule::LlmCheck.as_str(),
                        "notification triggered"
                    );
                    let raw = clean
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .last()
                        .unwrap_or("")
                        .trim();
                    (
                        NotificationTriggerRule::LlmCheck,
                        Some(llm_detail),
                        sanitize_body(raw),
                    )
                } else if Instant::now().duration_since(output_epoch) >= silence {
                    info!(
                        session_id,
                        trigger_rule = NotificationTriggerRule::Silence.as_str(),
                        "notification triggered"
                    );
                    let raw = clean
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .last()
                        .unwrap_or("")
                        .trim();
                    (NotificationTriggerRule::Silence, None, sanitize_body(raw))
                } else {
                    continue;
                };

            let event = NotificationEvent::input_needed_with_trigger(
                session_id.clone(),
                body,
                trigger_rule,
                trigger_detail,
            );

            let dispatched = if candidate.notifications_enabled {
                notifier.dispatch(&event).await.any_delivered()
            } else {
                // This is useful for supervisor agent to take over when to send notifications
                true
            };

            if dispatched {
                session_store.mark_notified(&session_id, output_epoch, std::time::Instant::now());

                let _ = notification_tx.send(event.clone());
                let _ = event_tx.send(http::SessionEvent::SessionNotification {
                    kind: event.kind.as_str().to_string(),
                    summary: event.summary,
                    body: event.body,
                    session_ids: event.session_ids,
                    trigger_rule: event.trigger_rule.map(|rule| rule.as_str().to_string()),
                    trigger_detail: event.trigger_detail,
                });
            } else {
                warn!(session_id, "notification delivery failed on all channels");
            }
        }
    }
}

fn evaluate_llm_direct_trigger(_excerpt: &str) -> Option<String> {
    None
}

pub(super) fn build_notifier(db: Arc<Database>, config: &AppConfig) -> Notifier {
    let mut channels: Vec<Box<dyn NotificationChannel + Send + Sync>> =
        vec![Box::new(LocalOsNotificationChannel {
            hook: config.notification_hook.clone(),
        })];

    if let (Some(vapid_public_key), Some(vapid_private_key), Some(vapid_subject)) = (
        config.web_push_vapid_public_key.clone(),
        config.web_push_vapid_private_key.clone(),
        config.web_push_subject.clone(),
    ) {
        match WebPushChannel::new(&vapid_private_key, &vapid_public_key, &vapid_subject, db) {
            Ok(channel) => {
                info!("web push channel enabled");
                channels.push(Box::new(channel));
            }
            Err(err) => {
                warn!(%err, "web push channel init failed, continuing without it");
            }
        }
    }

    Notifier::with_channels(channels)
}
