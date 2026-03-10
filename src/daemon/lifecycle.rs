use std::{fs::File, process::Stdio, sync::Arc};

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
    session::SessionStore,
    storage,
};

use super::{
    JoinHandles,
    auth::{confirm_no_auth_risk, prompt_and_hash_password},
    rpc::handle_client,
};

pub struct DaemonGuard {
    _lock: File,
    config: AppConfig,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = storage::remove_file_if_exists(&self.config.lock_file);
        let _ = storage::remove_file_if_exists(&self.config.socket_file);
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
        spawn_detached(no_auth, no_http, auth_hash.as_deref())?;
        wait_for_daemon_ready(&config, std::time::Duration::from_secs(60)).await?;
        println!(
            "Daemon started in background. To create a session, run `oly start --detach <cmd>`"
        );
        return Ok(());
    }

    run_foreground(config, auth_hash, no_http).await
}

pub async fn stop(config: AppConfig) -> Result<()> {
    match ipc::send_request(&config, RpcRequest::DaemonStop).await? {
        RpcResponse::DaemonStop { stopped } => {
            if !stopped {
                eprintln!(
                    "warning: daemon stopped but one or more sessions may not have stopped cleanly"
                );
            }
            Ok(())
        }
        RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

async fn wait_for_daemon_ready(config: &AppConfig, timeout: std::time::Duration) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if matches!(
            ipc::send_request(config, RpcRequest::Health).await,
            Ok(RpcResponse::Health { .. })
        ) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    Err(AppError::DaemonUnavailable(
        "daemon failed to become ready in time".to_string(),
    ))
}

fn spawn_detached(no_auth: bool, no_http: bool, auth_hash: Option<&str>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("start")
        .arg("--foreground-internal")
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
    cmd.spawn()?;
    Ok(())
}

async fn run_foreground(config: AppConfig, auth_hash: Option<String>, no_http: bool) -> Result<()> {
    info!(
        no_http,
        auth_enabled = auth_hash.is_some(),
        state_dir = ?config.state_dir,
        sessions_dir = ?config.sessions_dir,
        "daemon foreground initialization"
    );

    storage::ensure_state_dirs(&config.state_dir, &config.sessions_dir)?;

    let lock = match storage::try_acquire_daemon_lock(&config.lock_file) {
        Ok(file) => file,
        Err(AppError::DaemonAlreadyRunning) => {
            if matches!(
                ipc::send_request(&config, RpcRequest::Health).await,
                Ok(RpcResponse::Health { .. })
            ) {
                return Err(AppError::DaemonAlreadyRunning);
            }
            storage::remove_file_if_exists(&config.lock_file)?;
            storage::try_acquire_daemon_lock(&config.lock_file)?
        }
        Err(err) => return Err(err),
    };

    storage::write_pid(&config.lock_file, std::process::id())?;

    let _guard = DaemonGuard {
        _lock: lock,
        config: config.clone(),
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

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with(file_layer)
        .with(stderr_layer)
        .init();

    let pid = std::process::id();
    info!(pid, "daemon started");

    let db = Arc::new(Database::open(&config.db_file, config.sessions_dir.clone()).await?);
    info!(db_file = ?config.db_file, "database opened");

    let node_registry = Arc::new(NodeRegistry::new());
    let (notification_tx, _) = tokio::sync::broadcast::channel::<NotificationEvent>(512);

    let join_handles: JoinHandles = Arc::new(Mutex::new(std::collections::HashMap::new()));
    for join in client::join::load_join_configs(&config) {
        let (abort, stop_tx) =
            client::spawn_join_connector(join.clone(), config.clone(), notification_tx.subscribe());
        join_handles
            .lock()
            .await
            .insert(join.name, (abort, stop_tx));
    }
    {
        let count = join_handles.lock().await.len();
        info!(count, "join connectors initialized");
    }

    let listener = ipc::bind(&config)?;
    info!(socket_file = ?config.socket_file, "ipc listener bound");
    let (store, startup_failed_sessions) = {
        let mut store = SessionStore::new(config.session_eviction_seconds, db.clone());
        let startup_failed_sessions = store.load_running_stopping_sessions().await;
        (store, startup_failed_sessions)
    };
    let session_store = Arc::new(Mutex::new(store));
    let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<()>();

    let (event_tx, _) = tokio::sync::broadcast::channel::<http::SessionEvent>(512);

    let auth_state = auth_hash.map(AuthState::new);
    if !no_http {
        let http_state = http::AppState {
            store: session_store.clone(),
            config: config.clone(),
            db: db.clone(),
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
    let notify_config = config.clone();
    let notify_db = db.clone();
    let notify_event_tx = event_tx.clone();
    let notify_notification_tx = notification_tx.clone();
    tokio::spawn(async move {
        crate::notification::run_notification_monitor(
            notify_store,
            notify_config,
            notify_db,
            notify_event_tx,
            notify_notification_tx,
        )
        .await;
    });
    info!("notification monitor task spawned");

    if !startup_failed_sessions.is_empty() {
        let notifier = crate::notification::build_notifier(db.clone(), &config);
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
        let _ = event_tx.send(http::SessionEvent::SessionNotification {
            kind: event.kind.as_str().to_string(),
            summary: event.summary,
            body: event.body,
            session_ids: event.session_ids,
            trigger_rule: event.trigger_rule.map(|rule| rule.as_str().to_string()),
            trigger_detail: event.trigger_detail,
        });
    }

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
            incoming = listener.accept() => {
                match incoming {
                    Ok(stream) => {
                        let config_clone = config.clone();
                        let store_clone = session_store.clone();
                        let shutdown_tx_clone = shutdown_tx.clone();
                        let registry_clone = node_registry.clone();
                        let db_clone = db.clone();
                        let handles_clone = join_handles.clone();
                        let notification_tx_clone = notification_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(
                                stream,
                                &config_clone,
                                store_clone,
                                shutdown_tx_clone,
                                registry_clone,
                                db_clone,
                                handles_clone,
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
