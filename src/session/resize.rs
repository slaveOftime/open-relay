// ---------------------------------------------------------------------------
// ResizeSubscriber — encapsulates resize broadcast reception with
// self-echo suppression so that each attach handler doesn't need to
// duplicate the same `tokio::select!` branch + tracking logic.
// ---------------------------------------------------------------------------

use tokio::sync::broadcast;
use tracing::debug;

/// Receives PTY resize broadcasts from `SessionRuntime::resize_tx` while
/// filtering out resizes that originated from this client.
///
/// Cancel-safe: the only await point is `broadcast::Receiver::recv()`,
/// which is itself cancel-safe.
pub struct ResizeSubscriber {
    rx: Option<broadcast::Receiver<(u16, u16)>>,
    /// The most recent resize that *this* client sent.  When a broadcast
    /// arrives matching this value we know it's our own echo and skip it.
    last_self_resize: Option<(u16, u16)>,
    session_id: String,
}

impl ResizeSubscriber {
    /// Create a subscriber from the result of `SessionStore::subscribe_resize`.
    /// If `result` is `None` (session not found / lock failed), the subscriber
    /// will pend forever without producing any values.
    pub fn new(
        result: Option<(broadcast::Receiver<(u16, u16)>, Option<(u16, u16)>)>,
        session_id: String,
    ) -> Self {
        let rx = result.map(|(rx, _)| rx);
        Self {
            rx,
            last_self_resize: None,
            session_id,
        }
    }

    /// Record that this client just sent a resize with the given dimensions.
    /// The next broadcast matching these dimensions will be treated as a
    /// self-echo and silently consumed.
    pub fn mark_sent(&mut self, rows: u16, cols: u16) {
        self.last_self_resize = Some((rows, cols));
    }

    /// Wait for a resize event that originated from *another* client.
    ///
    /// - Self-echoes are consumed internally (never returned).
    /// - `Lagged` errors are logged and retried.
    /// - Returns `None` when the channel is closed; subsequent calls pend
    ///   forever (the session is gone).
    pub async fn recv_foreign(&mut self) -> Option<(u16, u16)> {
        loop {
            let rx = self.rx.as_mut()?;
            match rx.recv().await {
                Ok((rows, cols)) => {
                    if self.last_self_resize == Some((rows, cols)) {
                        self.last_self_resize = None;
                        continue;
                    }
                    return Some((rows, cols));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!(
                        session_id = %self.session_id,
                        skipped = n,
                        "resize broadcast lagged"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!(
                        session_id = %self.session_id,
                        "resize broadcast channel closed"
                    );
                    self.rx = None;
                    return None;
                }
            }
        }
    }
}
