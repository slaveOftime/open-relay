use std::{fmt::Write as _, fs::File, process::Stdio, sync::Arc, time::Duration};

use interprocess::local_socket::traits::tokio::Listener as _;
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    client,
    config::AppConfig,
    db::Database,
    error::{AppError, Result},
    http,
    http::AuthState,
    ipc,
    node::NodeRegistry,
    notification::event::NotificationEvent,
    protocol::{RpcRequest, RpcResponse},
    session::{SessionEvent, SessionStore},
    storage,
};

use super::{
    JoinHandles,
    auth::{confirm_no_auth_risk, prompt_and_hash_password},
    rpc::handle_client,
};

const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(120);
const LOCK_STARTUP_GRACE: Duration = Duration::from_secs(3);

pub struct DaemonGuard {
    _lock: File,
    config: Arc<AppConfig>,
}

fn build_env_filter(config: &AppConfig) -> tracing_subscriber::EnvFilter {
    if let Ok(filter) = std::env::var("RUST_LOG") {
        return tracing_subscriber::EnvFilter::try_new(filter)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    }

    tracing_subscriber::EnvFilter::try_new(config.log_level.as_str())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = storage::remove_file_if_exists(&self.config.lock_file);
        let _ = storage::remove_file_if_exists(&self.config.socket_file);
    }
}

async fn daemon_is_healthy(config: &AppConfig) -> bool {
    matches!(
        ipc::send_request(config, RpcRequest::Health).await,
        Ok(RpcResponse::Health { .. })
    )
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    type Handle = *mut core::ffi::c_void;

    unsafe extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
        fn CloseHandle(object: Handle) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }

        let mut exit_code = 0;
        let ok = GetExitCodeProcess(process, &mut exit_code);
        let _ = CloseHandle(process);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    const EPERM: i32 = 1;

    unsafe {
        if kill(pid as i32, 0) == 0 {
            return true;
        }

        std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_running(_pid: u32) -> bool {
    false
}

async fn acquire_daemon_start_lock(config: &AppConfig) -> Result<File> {
    let deadline = std::time::Instant::now() + LOCK_STARTUP_GRACE;

    loop {
        match storage::try_acquire_daemon_lock(&config.lock_file) {
            Ok(file) => return Ok(file),
            Err(AppError::DaemonAlreadyRunning) => {
                if daemon_is_healthy(config).await {
                    return Err(AppError::DaemonAlreadyRunning);
                }

                if let Some(pid) = storage::read_pid(&config.lock_file)? {
                    if process_is_running(pid) {
                        return Err(AppError::DaemonAlreadyRunning);
                    }
                }

                if std::time::Instant::now() >= deadline {
                    storage::remove_file_if_exists(&config.lock_file)?;
                    storage::remove_file_if_exists(&config.socket_file)?;
                    return storage::try_acquire_daemon_lock(&config.lock_file);
                }

                tokio::time::sleep(LOCK_RETRY_INTERVAL).await;
            }
            Err(err) => return Err(err),
        }
    }
}

pub async fn start(
    config: AppConfig,
    detach: bool,
    foreground_internal: bool,
    no_auth: bool,
    no_http: bool,
    auth_hash_internal: Option<String>,
) -> Result<()> {
    if daemon_is_healthy(&config).await {
        return Err(AppError::DaemonAlreadyRunning);
    }

    let auth_hash: Option<String> = if foreground_internal {
        auth_hash_internal
    } else if !no_http {
        if no_auth {
            confirm_no_auth_risk()?;
            None
        } else {
            let hash = prompt_and_hash_password()?;
            Some(hash)
        }
    } else {
        None
    };

    if detach && !foreground_internal {
        spawn_detached(no_auth, no_http, auth_hash.as_deref(), config.http_port)?;
        wait_for_daemon_ready(&config, std::time::Duration::from_secs(60)).await?;
        print_detached_start_summary(&config, no_http, no_auth);
        return Ok(());
    }

    run_foreground(config, auth_hash, no_http).await
}

fn print_detached_start_summary(config: &AppConfig, no_http: bool, no_auth: bool) {
    print!("{}", detached_start_summary(config, no_http, no_auth));
}

fn detached_start_summary(config: &AppConfig, no_http: bool, no_auth: bool) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Daemon started in background.");
    let _ = writeln!(out);

    if no_http {
        let _ = writeln!(out, "  HTTP:         disabled (--no-http)");
    } else {
        let _ = writeln!(out, "  Web UI/API:   http://127.0.0.1:{}", config.http_port);
        let _ = writeln!(
            out,
            "  Auth:         {}",
            if no_auth {
                "disabled (--no-auth)"
            } else {
                "enabled"
            }
        );
    }

    let _ = writeln!(out, "  Root:         {}", config.state_dir.display());
    let _ = writeln!(
        out,
        "  Logs:         {}",
        config.state_dir.join("logs").display()
    );
    let _ = writeln!(out, "  Sessions:     {}", config.sessions_dir.display());
    let _ = writeln!(out);
    let _ = writeln!(out, "Tips:");
    let _ = writeln!(out, "  Stop daemon:      oly daemon stop");
    let _ = writeln!(out, "  Create a session: oly start --detach <cmd>");
    out
}

pub async fn stop(config: AppConfig, grace_seconds: u64) -> Result<()> {
    match ipc::send_request_checked(&config, RpcRequest::DaemonStop { grace_seconds }).await? {
        RpcResponse::DaemonStop { stopped } => {
            if !stopped {
                eprintln!(
                    "warning: daemon stopped but one or more sessions may not have stopped cleanly"
                );
            }
            Ok(())
        }
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

async fn wait_for_daemon_ready(config: &AppConfig, timeout: std::time::Duration) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if daemon_is_healthy(config).await {
            return Ok(());
        }

        // If the child already exited, stop waiting and surface a helpful message.
        if let Some(pid) = storage::read_pid(&config.lock_file)? {
            if !process_is_running(pid) {
                return Err(AppError::DaemonUnavailable(format!(
                    "daemon process exited before becoming ready. Check logs under {}",
                    config.state_dir.display()
                )));
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    Err(AppError::DaemonUnavailable(
        "daemon failed to become ready in time".to_string(),
    ))
}

fn spawn_detached(no_auth: bool, no_http: bool, auth_hash: Option<&str>, port: u16) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("start")
        .arg("--foreground-internal")
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if no_auth {
        cmd.arg("--no-auth");
    } else if let Some(hash) = auth_hash {
        cmd.arg("--auth-hash-internal").arg(hash);
    }
    if no_http {
        cmd.arg("--no-http");
    }
    // On Windows the spawned process must be placed in its own process group
    // and detached from the parent console.  Without these flags the daemon
    // stays in the same console process group as the launching terminal; closing
    // that terminal sends CTRL_CLOSE_EVENT to every process in the group and
    // kills the daemon silently.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS  (0x00000008): no console window, detached from parent console
        // CREATE_NEW_PROCESS_GROUP (0x00000200): own signal group, won't receive Ctrl+C/Break from parent
        cmd.creation_flags(0x00000008 | 0x00000200);
    }
    // On Unix the child must start a new session so it is not killed by SIGHUP
    // when the launching terminal closes.  This mirrors the Windows
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP flags above.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Safety: setsid() is async-signal-safe (POSIX.1-2008).
        unsafe {
            cmd.pre_exec(|| {
                unsafe extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            });
        }
    }
    cmd.spawn()?;
    Ok(())
}

async fn run_foreground(config: AppConfig, auth_hash: Option<String>, no_http: bool) -> Result<()> {
    let config = Arc::new(config);

    info!(
        no_http,
        auth_enabled = auth_hash.is_some(),
        state_dir = ?config.state_dir,
        sessions_dir = ?config.sessions_dir,
        "daemon foreground initialization"
    );

    storage::ensure_state_dirs(&config.state_dir, &config.sessions_dir)?;

    let lock = acquire_daemon_start_lock(&config).await?;

    storage::write_pid(&config.lock_file, std::process::id())?;

    let _guard = DaemonGuard {
        _lock: lock,
        config: Arc::clone(&config),
    };

    let file_appender =
        tracing_appender::rolling::daily(config.state_dir.join("logs"), "daemon.log");
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .compact()
        .without_time()
        .with_target(false);

    let env_filter = build_env_filter(&config);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    let pid = std::process::id();
    info!(pid, log_level = %config.log_level, "daemon started");

    let db = Arc::new(Database::open(&config.db_file, config.sessions_dir.clone()).await?);
    info!(db_file = ?config.db_file, "database opened");

    let node_registry = Arc::new(NodeRegistry::new());
    let (notification_tx, _) = tokio::sync::broadcast::channel::<NotificationEvent>(100);

    let join_handles: JoinHandles = Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Remove any stale socket file left by a crashed daemon.  On macOS (and
    // other platforms without abstract-namespace sockets) the file-based Unix
    // domain socket persists on disk after an unclean exit.  Binding to an
    // existing socket file fails with EADDRINUSE, which silently prevents the
    // daemon from starting and makes the parent `wait_for_daemon_ready` loop
    // appear to hang.
    storage::remove_file_if_exists(&config.socket_file)?;

    let listener = ipc::bind(&config)?;
    info!(socket_file = ?config.socket_file, "ipc listener bound");
    let (store, startup_failed_sessions) = {
        let store = SessionStore::new(config.session_eviction_seconds, db.clone());
        let startup_failed_sessions = store.load_running_stopping_sessions().await;
        (store, startup_failed_sessions)
    };
    let session_store = Arc::new(store);
    let event_tx = session_store.event_tx();
    for join in client::join::load_join_configs(&config) {
        let (abort, stop_tx) = super::rpc_nodes::spawn_join_connector(
            join.clone(),
            Arc::clone(&config),
            event_tx.subscribe(),
        );
        join_handles
            .lock()
            .await
            .insert(join.name, (abort, stop_tx));
    }
    {
        let count = join_handles.lock().await.len();
        info!(count, "join connectors initialized");
    }
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();
    let notifier = Arc::new(crate::notification::build_notifier(db.clone(), &config));

    let auth_state = auth_hash.map(AuthState::new);
    if !no_http {
        let http_state = http::AppState {
            store: session_store.clone(),
            config: Arc::clone(&config),
            db: db.clone(),
            notifier: notifier.clone(),
            event_tx: event_tx.clone(),
            auth: auth_state,
            node_registry: node_registry.clone(),
        };
        tokio::spawn(http::serve(http_state));
        info!("http server task spawned");
    } else {
        info!("http server disabled by --no-web");
    }

    let notify_store = session_store.clone();
    let notify_config = Arc::clone(&config);
    let notify_event_tx = event_tx.clone();
    let notify_notification_tx = notification_tx.clone();
    let notify_notifier = notifier.clone();
    tokio::spawn(async move {
        crate::notification::run_notification_monitor(
            notify_notifier,
            notify_store,
            notify_config,
            notify_event_tx,
            notify_notification_tx,
        )
        .await;
    });
    info!("notification monitor task spawned");

    if !startup_failed_sessions.is_empty() {
        let notifier = notifier.clone();
        let event = NotificationEvent::startup_recovery(&startup_failed_sessions);
        let outcome = notifier.dispatch(&event).await;

        if outcome.any_delivered() {
            info!(
                count = startup_failed_sessions.len(),
                delivered = outcome.delivered,
                attempted = outcome.attempted,
                "startup stale-session notification delivered"
            );
        } else {
            warn!(
                count = startup_failed_sessions.len(),
                attempted = outcome.attempted,
                failed_channels = ?outcome.failed_channels,
                "startup stale-session notification failed on all channels"
            );
        }

        let _ = notification_tx.send(event.clone());
        let _ = event_tx.send(SessionEvent::SessionNotification {
            kind: event.kind.as_str().to_string(),
            title: event.title,
            description: event.description,
            body: event.body,
            navigation_url: event.navigation_url,
            session_ids: event.session_ids,
            trigger_rule: event.trigger_rule.map(|rule| rule.as_str().to_string()),
            trigger_detail: event.trigger_detail,
            node: event.node,
        });
    }

    let mut session_maintenance_tick = tokio::time::interval(Duration::from_secs(1));
    session_maintenance_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("daemon received ctrl-c, shutting down");
                break;
            }
            _ = shutdown_rx.recv() => {
                info!("daemon received stop request, shutting down");
                break;
            }
            _ = session_maintenance_tick.tick() => {
                session_store.run_maintenance().await;
            }
            incoming = listener.accept() => {
                match incoming {
                    Ok(stream) => {
                        let config_clone = Arc::clone(&config);
                        let store_clone = session_store.clone();
                        let shutdown_tx_clone = shutdown_tx.clone();
                        let registry_clone = node_registry.clone();
                        let db_clone = db.clone();
                        let handles_clone = join_handles.clone();
                        let event_tx_clone = event_tx.clone();
                        let notification_tx_clone = notification_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(
                                stream,
                                config_clone,
                                store_clone,
                                shutdown_tx_clone,
                                registry_clone,
                                db_clone,
                                handles_clone,
                                event_tx_clone,
                                notification_tx_clone,
                            ).await {
                                error!(%err, "client handling error");
                            }
                        });
                    }
                    Err(err) => {
                        error!(%err, "accept error");
                    }
                }
            }
        }
    }

    info!("daemon stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::detached_start_summary;
    use crate::config::AppConfig;

    fn test_config() -> AppConfig {
        let state_dir = PathBuf::from("test-state");
        AppConfig {
            http_port: 15443,
            log_level: "info".to_string(),
            ring_buffer_bytes: 1_048_576,
            stop_grace_seconds: 5,
            prompt_patterns: Vec::new(),
            web_push_subject: None,
            web_push_vapid_public_key: None,
            web_push_vapid_private_key: None,
            state_dir: state_dir.clone(),
            sessions_dir: state_dir.join("sessions"),
            db_file: state_dir.join("oly.db"),
            lock_file: state_dir.join("daemon.lock"),
            socket_name: "test.sock".to_string(),
            socket_file: state_dir.join("daemon.sock"),
            silence_seconds: 10,
            session_eviction_seconds: 15,
            max_running_sessions: 50,
            notification_hook: None,
        }
    }

    #[test]
    fn detached_summary_includes_http_url_and_paths() {
        let config = test_config();
        let summary = detached_start_summary(&config, false, true);

        assert!(summary.contains("Daemon started in background."));
        assert!(summary.contains("Web UI/API:   http://127.0.0.1:15443"));
        assert!(summary.contains("Auth:         disabled (--no-auth)"));
        assert!(summary.contains(&format!("Root:         {}", config.state_dir.display())));
        assert!(summary.contains(&format!(
            "Logs:         {}",
            config.state_dir.join("logs").display()
        )));
        assert!(summary.contains(&format!("Sessions:     {}", config.sessions_dir.display())));
        assert!(summary.contains("Stop daemon:      oly daemon stop"));
        assert!(summary.contains("Create a session: oly start --detach <cmd>"));
    }

    #[test]
    fn detached_summary_marks_http_disabled() {
        let config = test_config();
        let summary = detached_start_summary(&config, true, false);

        assert!(summary.contains("HTTP:         disabled (--no-http)"));
        assert!(!summary.contains("Web UI/API:"));
        assert!(!summary.contains("Web root:"));
        assert!(!summary.contains("Auth:"));
    }
}
