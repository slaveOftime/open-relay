//! Silence detection driving push notifications.
//!
//! `silent_candidates` is polled on a timer over every live session, so it
//! takes a read lock per session and never writes: all suppression state is
//! derived from the activity epochs the runtime already maintains.

use std::time::{Duration, Instant};

use tracing::{debug, trace};

use crate::session::SessionEvent;

use super::{SessionStore, SilentCandidate, USER_ACTIVITY_WINDOW};

impl SessionStore {
    /// Returns silent candidates with their output and latest activity epochs.
    pub fn silent_candidates(
        &self,
        attach_suppression_window: Duration,
        min_notification_interval: Duration,
    ) -> Vec<SilentCandidate> {
        let now = Instant::now();
        let sessions = self.sessions.load();
        sessions
            .values()
            .filter_map(|handle| {
                let rt = handle.read();
                if rt.is_completed() {
                    return None;
                }

                // Sessions that have never produced any meaningful output (e.g. a
                // process that starts up and then silently waits for a password or
                // other input before printing anything) must still be considered:
                // fall back to the spawn time so they are not permanently invisible
                // to silence detection. `rt.spawned_at` never changes, so once such
                // a session is notified it will not be re-notified unless real
                // output eventually arrives and advances the epoch.
                let last_output = rt.effective_output_epoch();

                // The newest user-driven activity of any kind: text input,
                // mouse clicks/hover (delivered as input bytes), resizes,
                // attach heartbeats and attaches themselves.
                let user_activity = rt.user_activity_epoch();

                // Silence is measured from the newest activity of any kind, so
                // a user who is still interacting never looks "silent" and an
                // old output epoch is never treated as an already-expired
                // timer. This also covers recent attaches: someone who just
                // opened the session has seen its current state, so there is
                // nothing to notify about until the suppression window elapses.
                let silence_epoch =
                    user_activity.map_or(last_output, |activity| activity.max(last_output));

                if now.duration_since(silence_epoch) < attach_suppression_window {
                    trace!("silent because of recent user or output activity");
                    return None;
                }

                // Output that lands within `USER_ACTIVITY_WINDOW` of user
                // activity is almost certainly a reaction to it — keystroke
                // echo, a redraw after a resize, hover/click feedback — rather
                // than the program asking for attention. The same is true when
                // nothing came back at all (echo is off, e.g. a password
                // prompt): the user is mid-interaction either way.
                //
                // Once the gap grows beyond the window the program clearly
                // produced something on its own and then went quiet, which is
                // exactly the "waiting for you" state worth notifying about.
                let activity_driven = match user_activity {
                    Some(activity) if last_output <= activity => true,
                    Some(activity) => last_output.duration_since(activity) <= USER_ACTIVITY_WINDOW,
                    None => false,
                };
                let should_notify = !activity_driven;

                // Suppress rapid repeat notifications. `last_notified_at` is
                // normally later than `last_output`, so compare it with `now`;
                // subtracting it from the output epoch can underflow and kill
                // the notification monitor task.
                if should_notify
                    && let Some(last_notified_at) = rt.last_notified_at
                    && now.duration_since(last_notified_at) < min_notification_interval
                {
                    trace!("silent because notification was sent recently");
                    return None;
                }

                if rt.notified_output_epoch == Some(last_output) {
                    trace!("silent becase no changed since last nofification");
                    return None;
                }

                debug!(
                    session_id = rt.meta.id.as_str(),
                    user_activity_at = ?user_activity,
                    last_output_epoch = ?rt.last_output_epoch,
                    last_notified_at = ?rt.last_notified_at,
                    activity_driven,
                    "silent candidate ready"
                );

                // As this is most for matching some pattern from coding agent cli, most of them have input box under the bottom.
                // And most of them are using alt screen, it is more accurate to just use the live tail logs.
                // Silent can still be a fallback, just need to wait a little bit longer for the notification.
                let excerpt = rt.render_logs(15, false, u16::MAX);
                Some(SilentCandidate {
                    session_id: rt.meta.id.clone(),
                    session_title: rt.meta.title.clone(),
                    excerpt: String::from_utf8_lossy(&excerpt).into_owned(),
                    output_epoch: last_output,
                    silence_epoch,
                    should_notify,
                    enabled_for_channels: rt.notifications_enabled,
                    last_total_bytes: rt.last_total_bytes,
                })
            })
            .collect()
    }

    /// Records a successful notification for `session_id` at `output_epoch`.
    /// Re-notification is suppressed until output advances to a new epoch.
    pub fn mark_notified(&self, session_id: &str, output_epoch: Instant, notified_at: Instant) {
        let sessions = self.sessions.load();
        if let Some(handle) = sessions.get(session_id) {
            let mut rt = handle.write();
            rt.notified_output_epoch = Some(output_epoch);
            rt.last_notified_at = Some(notified_at);
        }
    }

    /// Marks a session as awaiting input without recording a delivered notification.
    pub fn mark_input_required(&self, session_id: &str, output_epoch: Instant) {
        let sessions = self.sessions.load();
        if let Some(handle) = sessions.get(session_id) {
            let summary = {
                let mut rt = handle.write();
                rt.notified_output_epoch = Some(output_epoch);
                rt.to_summary()
            };
            let _ = self.event_tx.send(SessionEvent::SessionUpdated(summary));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport::*;
    use super::*;
    use crate::session::{SessionError, SessionStatus};

    use std::time::{Duration, Instant};
    #[tokio::test]
    async fn test_silent_candidates_returns_running_past_silence() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        // last output was 10s ago → past silence
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "password: ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "abc1234");
        assert!(candidates[0].should_notify);
    }

    #[tokio::test]
    async fn test_silent_candidates_paused_input_uses_input_as_silence_anchor() {
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        let input_at = Instant::now() - Duration::from_secs(8);
        rt.write().last_input_at = Some(input_at);
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert_eq!(candidates.len(), 1);
        assert!(!candidates[0].should_notify);
        assert_eq!(candidates[0].silence_epoch, input_at);
    }

    #[tokio::test]
    async fn test_silent_candidates_echoed_input_does_not_notify() {
        // The PTY echoed the keystroke back almost immediately, so the output
        // only shows the user what they typed. Even once the silence window
        // elapses this must not push a notification — the user is simply
        // pausing while composing input.
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ls",
            Some(Duration::from_millis(29_600)),
        );
        let input_at = Instant::now() - Duration::from_secs(30);
        rt.write().last_input_at = Some(input_at);
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert_eq!(candidates.len(), 1);
        assert!(
            !candidates[0].should_notify,
            "output within the echo window of user input must not notify"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_output_long_after_input_notifies() {
        // The program kept printing well past the echo window and then went
        // quiet, which means it produced something on its own and is now
        // waiting for the user — worth a notification.
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "Do you want to proceed? (y/n)",
            Some(Duration::from_secs(10)),
        );
        rt.write().last_input_at = Some(Instant::now() - Duration::from_secs(60));
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0].should_notify,
            "output produced well after the input echo window should notify"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_notify_after_echo_suppressed_cycle() {
        // Echo suppression must not blackhole later notifications: once the
        // program itself prints something well after the user's keystroke, the
        // session becomes notify-worthy again even though the previous cycle
        // only flagged "input required".
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ls",
            Some(Duration::from_millis(29_600)),
        );
        rt.write().last_input_at = Some(Instant::now() - Duration::from_secs(30));
        let store = store_with(vec![rt.clone()], make_test_db().await);

        let first = store.silent_candidates(suppression_window, min_interval);
        assert_eq!(first.len(), 1);
        assert!(!first[0].should_notify);
        store.mark_input_required("abc1234", first[0].output_epoch);

        // Same epoch → nothing new to report.
        assert!(
            store
                .silent_candidates(suppression_window, min_interval)
                .is_empty()
        );

        // The program prints on its own and then goes quiet.
        rt.write().last_output_epoch = Some(Instant::now() - Duration::from_secs(6));

        let second = store.silent_candidates(suppression_window, min_interval);
        assert_eq!(second.len(), 1);
        assert!(
            second[0].should_notify,
            "program output after the echo window must re-enable notifications"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_uses_latest_activity_as_silence_anchor() {
        // Output arrived after the input, so silence must be measured from the
        // output rather than the older input timestamp.
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(10)),
        );
        let output_at = rt.read().effective_output_epoch();
        rt.write().last_input_at = Some(Instant::now() - Duration::from_secs(60));
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].silence_epoch, output_at);
    }

    #[tokio::test]
    async fn test_silent_candidates_recent_echo_output_suppressed_by_window() {
        // Fresh echo output keeps the session out of the candidate list while
        // the user is actively typing.
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> l",
            Some(Duration::from_millis(200)),
        );
        rt.write().last_input_at = Some(Instant::now() - Duration::from_millis(300));
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_recent_paused_input_waits_from_input_time() {
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        rt.write().last_input_at = Some(Instant::now() - Duration::from_secs(2));
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_mark_input_required_does_not_record_notification_delivery() {
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        let output_epoch = rt.read().effective_output_epoch();
        let store = store_with(vec![rt.clone()], make_test_db().await);
        let mut events = store.event_tx().subscribe();

        store.mark_input_required("abc1234", output_epoch);

        let locked = rt.read();
        assert!(locked.input_needed());
        assert!(locked.last_notified_at.is_none());
        drop(locked);
        let event = events.try_recv().expect("session update should be emitted");
        assert!(matches!(
            event,
            SessionEvent::SessionUpdated(summary) if summary.input_needed
        ));
    }

    #[tokio::test]
    async fn test_silent_candidates_suppresses_recent_output_within_attach_window() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_millis(500)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_respects_min_notification_interval() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        {
            let sessions = store.sessions.load();
            let handle = sessions.get("abc1234").unwrap();
            let mut rt = handle.write();
            rt.last_notified_at = Some(Instant::now() - Duration::from_secs(3));
            rt.notified_output_epoch = None;
        }

        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_ignores_non_running_session() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Stopped,
            "prompt> ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_suppresses_no_output_within_grace_period() {
        // A session that has produced no output yet is still within the
        // attach/output suppression window right after spawn, so it must not
        // be flagged yet — this is the short grace period, not a permanent
        // exemption from silence detection (see the regression test below).
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime("abc1234", SessionStatus::Running, "prompt> ", None);
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_flags_never_output_session_past_silence() {
        // Regression test for a genuinely running session that has never
        // emitted a single byte (e.g. it is blocked waiting for input before
        // printing its first prompt). Such a session must still surface as a
        // silent candidate once the spawn-relative silence window elapses,
        // instead of being silently invisible forever because
        // `last_output_epoch` is `None`.
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime("abc1234", SessionStatus::Running, "", None);
        {
            let mut locked = rt.write();
            locked.spawned_at = Instant::now() - Duration::from_secs(10);
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "abc1234");
    }

    #[tokio::test]
    async fn test_silent_candidates_ignores_never_output_non_running_session() {
        // A stopped/completed session that never produced output must not be
        // treated as a silent "awaiting input" candidate — `is_completed()`
        // gating must take precedence over the spawn-time fallback.
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime("abc1234", SessionStatus::Stopped, "", None);
        {
            let mut locked = rt.write();
            locked.spawned_at = Instant::now() - Duration::from_secs(10);
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(candidates.is_empty());
    }

    #[tokio::test]
    async fn test_silent_candidates_respects_min_notification_interval_after_output() {
        // A notification timestamp normally follows the output timestamp.
        // This must suppress the candidate instead of panicking on an
        // attempted `last_output - last_notified_at` subtraction.
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "waiting for approval",
            Some(Duration::from_secs(5)),
        );
        {
            let mut locked = rt.write();
            locked.last_notified_at = Some(Instant::now() - Duration::from_secs(3));
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should suppress re-notification within min_notification_interval"
        );
    }

    #[tokio::test]
    async fn test_input_needed_true_for_never_output_session_after_notify() {
        // Distinguishing "genuinely running and silent" from "awaiting
        // input": once the monitor notifies for a never-output session,
        // `input_needed()` must report true so status inference surfaces it
        // correctly, even though `last_output_epoch` is still `None`.
        let rt = make_runtime("abc1234", SessionStatus::Running, "", None);
        {
            let mut locked = rt.write();
            locked.spawned_at = Instant::now() - Duration::from_secs(10);
        }
        let store = store_with(vec![rt.clone()], make_test_db().await);
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);

        let candidates = store.silent_candidates(silence, min_interval);
        assert_eq!(
            candidates.len(),
            1,
            "never-output session past silence should be a candidate"
        );

        store.mark_notified(
            "abc1234",
            candidates[0].output_epoch,
            std::time::Instant::now(),
        );

        let locked = rt.read();
        assert!(
            locked.input_needed(),
            "session should be reported as awaiting input after notification, even with no real output"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_includes_screen_excerpt() {
        let silence = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(10)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(silence, min_interval);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "abc1234");
        assert!(
            candidates[0].excerpt.contains("prompt>"),
            "excerpt should contain rendered screen content"
        );
    }

    #[tokio::test]
    async fn test_mark_notified_suppresses_until_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        // First call returns a candidate with an output epoch.
        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;

        // Mark as notified at this output epoch.
        store.mark_notified(id, epoch, Instant::now());

        // Second call: same output epoch → suppressed.
        let second = store.silent_candidates(silence, min_interval);
        assert!(
            second.is_empty(),
            "should suppress re-notification at same output epoch"
        );
    }

    #[tokio::test]
    async fn test_mark_notified_allows_after_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;
        store.mark_notified(id, epoch, Instant::now());

        // Simulate new output by advancing last_output_at on the runtime.
        {
            let sessions = store.sessions.load();
            let handle = sessions.get("abc1234").unwrap();
            let mut rt = handle.write();
            // A new epoch strictly later than the notified one, but old enough
            // to be outside the attach suppression window.
            rt.last_output_epoch = Some(Instant::now() - Duration::from_secs(2));
            // Move notification timestamp into the past so cooldown no longer blocks.
            rt.last_notified_at = Some(Instant::now() - Duration::from_secs(30));
        }

        // New output epoch + expired notification cooldown should re-qualify.
        let after_output = store.silent_candidates(silence, min_interval);
        assert_eq!(after_output.len(), 1);
    }

    #[tokio::test]
    async fn test_mark_notified_stays_suppressed_without_new_output() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![rt], make_test_db().await);

        let first = store.silent_candidates(silence, min_interval);
        assert_eq!(first.len(), 1);
        let id = &first[0].session_id;
        let epoch = first[0].output_epoch;
        store.mark_notified(id, epoch, Instant::now());

        // Same output epoch -> suppressed.
        let suppressed = store.silent_candidates(silence, min_interval);
        assert!(suppressed.is_empty());

        // Simulate time passing without any new output.
        {
            let sessions = store.sessions.load();
            let handle = sessions.get("abc1234").unwrap();
            let mut rt = handle.write();
            rt.last_notified_at = Some(Instant::now() - Duration::from_secs(31));
        }

        let still_suppressed = store.silent_candidates(silence, min_interval);
        assert!(
            still_suppressed.is_empty(),
            "should remain suppressed until new output advances epoch"
        );
    }

    #[tokio::test]
    async fn test_mark_notified_on_unknown_id_is_noop() {
        let store = SessionStore::new(900, make_test_db().await);
        // Should not panic.
        let now = Instant::now();
        store.mark_notified("does_not_exist", now, now);
    }

    #[tokio::test]
    async fn test_set_notifications_enabled_updates_runtime_and_snapshot() {
        let runtime = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![runtime], make_test_db().await);

        store
            .set_notifications_enabled("abc1234", false)
            .await
            .expect("disable notifications");

        let sessions = store.sessions.load();
        let handle = sessions.get("abc1234").expect("runtime should exist");
        let rt = handle.read();
        assert!(!rt.to_summary().notifications_enabled);
        assert!(!rt.notifications_enabled);
    }

    #[tokio::test]
    async fn test_set_notifications_enabled_unknown_id_returns_error() {
        let store = SessionStore::new(900, make_test_db().await);
        let result = store.set_notifications_enabled("missing", false).await;
        assert!(matches!(result, Err(SessionError::NotRunning)));
    }

    #[tokio::test]
    async fn test_set_notifications_enabled_rejects_completed_session() {
        let runtime = make_runtime(
            "done123",
            SessionStatus::Stopped,
            "",
            Some(Duration::from_secs(5)),
        );
        let store = store_with(vec![runtime], make_test_db().await);

        let result = store.set_notifications_enabled("done123", false).await;
        assert!(matches!(result, Err(SessionError::NotRunning)));
    }

    #[tokio::test]
    async fn render_live_logs_preserves_input_needed() {
        let runtime = make_runtime(
            "prompt123",
            SessionStatus::Running,
            "Allow directory access?",
            Some(Duration::from_secs(5)),
        );
        let output_epoch = runtime.read().effective_output_epoch();
        let store = store_with(vec![runtime], make_test_db().await);
        store.mark_notified("prompt123", output_epoch, Instant::now());
        assert!(store.is_input_needed("prompt123"));

        store
            .render_live_logs("prompt123", 10, false, 80)
            .await
            .expect("render live logs");

        assert!(
            store.is_input_needed("prompt123"),
            "reading logs must not acknowledge an input-required prompt"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_suppressed_during_recent_attach_activity() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)), // output 5s ago (past silence)
        );
        // Recent attach activity should suppress notifications without mutating runtime state.
        {
            let mut locked = rt.write();
            locked.last_attach_activity_at = Some(Instant::now());
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should suppress notification while attach activity is inside suppression window"
        );

        let sessions = store.sessions.load();
        let handle = sessions.get("abc1234").unwrap();
        let locked = handle.read();
        assert!(
            locked.last_output_epoch.is_some(),
            "suppression path should not mutate output epoch — it must remain intact"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_attach_activity_within_window_suppresses_notify() {
        // Attach-class activity (resize, mouse click/hover, attach heartbeat)
        // shortly before the last output means the output is a reaction to the
        // user, not the program asking for attention — even though no text
        // input was ever sent.
        let suppression_window = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(10)), // output 10s ago
        );
        {
            let mut locked = rt.write();
            locked.last_attach_activity_at = Some(Instant::now() - Duration::from_secs(12));
        }
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert_eq!(candidates.len(), 1);
        assert!(
            !candidates[0].should_notify,
            "output within the user-activity window of attach activity must not notify"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_attach_activity_well_before_output_notifies() {
        // The program kept producing output long after the last user
        // interaction and then went quiet: that is self-driven output worth
        // a notification.
        let suppression_window = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "Do you want to proceed? (y/n)",
            Some(Duration::from_secs(10)), // output 10s ago
        );
        {
            let mut locked = rt.write();
            locked.last_attach_activity_at = Some(Instant::now() - Duration::from_secs(30));
        }
        let store = store_with(vec![rt], make_test_db().await);

        let candidates = store.silent_candidates(suppression_window, min_interval);

        assert_eq!(candidates.len(), 1);
        assert!(
            candidates[0].should_notify,
            "output produced well after the user-activity window should notify"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_attach_itself_counts_as_activity() {
        // Opening the session is user activity: the attaching user just saw
        // the current state, so nothing should notify right after.
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        let store = store_with(vec![rt.clone()], make_test_db().await);

        store.register_attach_client("abc1234").await;

        assert!(
            store
                .silent_candidates(suppression_window, min_interval)
                .is_empty(),
            "a fresh attach should suppress candidacy inside the suppression window"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_stay_suppressed_after_detach() {
        // Detaching must not hand the session straight back to the notifier:
        // the user just read the screen, so the activity timestamp survives
        // the disconnect.
        let suppression_window = Duration::from_secs(5);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(30)),
        );
        let store = store_with(vec![rt.clone()], make_test_db().await);

        store.register_attach_client("abc1234").await;
        store
            .attach_detach("abc1234")
            .await
            .expect("detach should succeed");

        assert!(
            store
                .silent_candidates(suppression_window, min_interval)
                .is_empty(),
            "detach should keep the recent-activity suppression alive"
        );
    }

    #[tokio::test]
    async fn test_silent_candidates_drops_short_age_notifications() {
        let silence = Duration::from_secs(1);
        let min_interval = Duration::from_secs(10);
        let rt = make_runtime(
            "abc1234",
            SessionStatus::Running,
            "prompt> ",
            Some(Duration::from_secs(5)),
        );
        {
            let mut locked = rt.write();
            locked.last_notified_at = Some(Instant::now() - Duration::from_secs(3));
        }
        let store = store_with(vec![rt], make_test_db().await);
        let candidates = store.silent_candidates(silence, min_interval);
        assert!(
            candidates.is_empty(),
            "should drop candidates inside cooldown window"
        );
    }

    // -----------------------------------------------------------------------
    // attach_input — data forwarding and last_input_at tracking
    // -----------------------------------------------------------------------
}
