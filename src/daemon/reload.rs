//! Hot reload of `config.json` inside the running daemon.
//!
//! The reloader polls the config file's modification time and, when it
//! changes, rebuilds the [`AppConfig`] and swaps it into the shared
//! [`LiveConfig`]. Subsystems that read through `LiveConfig` (notification
//! monitor, session start paths, HTTP handlers) pick the new values up on
//! their own; the reloader applies the side effects that cannot be picked
//! up lazily: rebuilding notification channels, adjusting the session
//! eviction TTL and reloading the log filter.
//!
//! Only a subset of fields is hot-reloadable — see
//! [`AppConfig::hot_reload_changes`]. Edits to the bind address, HTTP port
//! or socket paths are logged and still require a daemon restart.

use std::{sync::Arc, time::Duration};

use tracing::{info, warn};

use crate::{config::LiveConfig, db::Database, notification::build_notifier};

use super::{
    NotifierHandle, SessionStoreHandle,
    lifecycle::{LogFilterHandle, build_env_filter},
};

/// How often the daemon polls `config.json` for changes.
///
/// mtime polling is one metadata syscall per tick, works on every platform
/// and filesystem, and avoids a filesystem-watcher dependency.
const CONFIG_RELOAD_POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(super) async fn run_config_reloader(
    live: LiveConfig,
    store: SessionStoreHandle,
    notifier: NotifierHandle,
    db: Arc<Database>,
    log_filter: LogFilterHandle,
) {
    let path = live.get().state_dir.join("config.json");
    let mut last_seen = file_modified_at(&path);

    loop {
        tokio::time::sleep(CONFIG_RELOAD_POLL_INTERVAL).await;

        let modified = file_modified_at(&path);
        if modified == last_seen {
            continue;
        }
        // Remember the attempt even when the reload fails, so a broken file
        // warns once instead of every poll; it is retried on the next edit.
        last_seen = modified;

        let old = live.get();
        let new = match old.try_reload() {
            Ok(new) => new,
            Err(err) => {
                warn!(%err, "config reload failed; keeping current configuration");
                continue;
            }
        };

        let hot_changes = old.hot_reload_changes(&new);
        let restart_changes = old.restart_required_changes(&new);
        if hot_changes.is_empty() && restart_changes.is_empty() {
            continue;
        }

        info!(changed = ?hot_changes, "config.json reloaded");
        if !restart_changes.is_empty() {
            warn!(
                changed = ?restart_changes,
                "config changes require a daemon restart to take effect"
            );
        }

        let rebuild_notifier = old.notification_hook != new.notification_hook
            || old.web_push_subject != new.web_push_subject
            || old.web_push_vapid_public_key != new.web_push_vapid_public_key
            || old.web_push_vapid_private_key != new.web_push_vapid_private_key
            || old.web_push_proxy != new.web_push_proxy;
        let reload_log_filter = old.log_level != new.log_level;

        store.set_eviction_seconds(new.session_eviction_seconds);
        live.replace(new.clone());

        if rebuild_notifier {
            notifier.store(Arc::new(build_notifier(db.clone(), &new)));
            info!("notification channels rebuilt from reloaded config");
        }
        if reload_log_filter {
            match log_filter.reload(build_env_filter(&new)) {
                Ok(()) => info!(log_level = %new.log_level, "log level reloaded"),
                Err(err) => warn!(%err, "failed to reload log level filter"),
            }
        }
    }
}

fn file_modified_at(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}
