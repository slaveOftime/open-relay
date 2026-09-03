use super::{
    channel::{LocalOsNotificationChannel, NotificationChannel},
    event::NotificationEvent,
};

/// Shareable, hot-swappable notifier handle.
///
/// The daemon's config hot-reload rebuilds the channels (notification hook,
/// VAPID keys, push proxy) and swaps in a new [`Notifier`] without a
/// restart; dispatchers load the current one per event.
pub type SharedNotifier = std::sync::Arc<arc_swap::ArcSwap<Notifier>>;

#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub attempted: usize,
    pub delivered: usize,
    pub failed_channels: Vec<String>,
}

impl DispatchOutcome {
    pub fn any_delivered(&self) -> bool {
        self.delivered > 0
    }
}

pub struct Notifier {
    channels: Vec<Box<dyn NotificationChannel + Send + Sync>>,
}

impl Notifier {
    #[allow(dead_code)]
    pub fn default_local(hook: Option<String>) -> Self {
        Self {
            channels: vec![Box::new(LocalOsNotificationChannel { hook })],
        }
    }

    pub fn with_channels(channels: Vec<Box<dyn NotificationChannel + Send + Sync>>) -> Self {
        Self { channels }
    }

    pub async fn dispatch(&self, event: &NotificationEvent) -> DispatchOutcome {
        let mut delivered = 0usize;
        let mut failed_channels = Vec::new();

        for channel in &self.channels {
            match channel.send(event).await {
                Ok(()) => delivered += 1,
                Err(err) => {
                    tracing::warn!(
                        channel = channel.name(),
                        kind = event.kind.as_str(),
                        error = %err,
                        "notification channel delivery failed"
                    );
                    failed_channels.push(channel.name().to_string());
                }
            }
        }

        DispatchOutcome {
            attempted: self.channels.len(),
            delivered,
            failed_channels,
        }
    }
}
