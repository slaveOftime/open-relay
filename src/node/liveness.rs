use std::time::Duration;

use tokio::{sync::watch, time::Instant};

/// Both endpoints must abandon half-open links. The watchdog runs outside the
/// relay future so a blocked socket/channel/local operation cannot suppress it.
pub(crate) const NODE_LIVENESS_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const NODE_SEND_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) async fn wait_for_inbound_timeout(mut inbound: watch::Receiver<Instant>) {
    loop {
        let deadline = *inbound.borrow_and_update() + NODE_LIVENESS_TIMEOUT;
        tokio::select! {
            biased;
            changed = inbound.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            _ = tokio::time::sleep_until(deadline) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn authenticated_inbound_resets_the_deadline() {
        let (tx, rx) = watch::channel(Instant::now());
        let watchdog = tokio::spawn(wait_for_inbound_timeout(rx));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(40)).await;
        tx.send_replace(Instant::now());
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(40)).await;
        assert!(!watchdog.is_finished());
        tokio::time::advance(Duration::from_secs(5)).await;
        watchdog.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_interrupts_a_stalled_relay() {
        let (_tx, rx) = watch::channel(Instant::now());
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = std::future::pending::<()>() => panic!("stalled relay returned"),
                _ = wait_for_inbound_timeout(rx) => {},
            }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(NODE_LIVENESS_TIMEOUT).await;
        task.await.unwrap();
    }
}
