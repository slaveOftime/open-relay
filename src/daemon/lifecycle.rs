use std::{fmt::Write as _, fs::File, path::Path, process::Stdio, sync::Arc, time::Duration};

use interprocess::local_socket::traits::tokio::Listener as _;
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    client,
    config::{AppConfig, LiveConfig},
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
    utils::format_http_url,
};

use super::{
    JoinHandles, NotifierHandle, SessionStoreHandle,
    auth::{confirm_no_auth_risk, prompt_and_hash_password},
    crash,
    rpc::handle_client,
};

const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(120);
const LOCK_STARTUP_GRACE: Duration = Duration::from_secs(3);

pub struct DaemonGuard {
    _lock: File,
    config: Arc<AppConfig>,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = storage::remove_file_if_exists(&self.config.paths.lock_file);
        let _ = storage::remove_file_if_exists(&self.config.paths.info_file);
        let _ = storage::remove_file_if_exists(&self.config.paths.socket_file);
    }
}

/// Handle to the daemon's reloadable log-level filter; the config hot-reload
/// task swaps in a new filter when `log_level` changes in `config.json`.
pub(super) type LogFilterHandle =
    tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

pub(super) fn build_env_filter(config: &AppConfig) -> tracing_subscriber::EnvFilter {
    if let Ok(filter) = std::env::var("RUST_LOG") {
        return tracing_subscriber::EnvFilter::try_new(filter)
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    }

    tracing_subscriber::EnvFilter::try_new(config.log_level.as_str())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
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
        match storage::try_acquire_daemon_lock(&config.paths.lock_file) {
            Ok(file) => return Ok(file),
            Err(AppError::DaemonAlreadyRunning) => {
                if daemon_is_healthy(config).await {
                    return Err(AppError::DaemonAlreadyRunning);
                }

                if let Some(pid) = storage::read_pid(&config.paths.lock_file)?
                    && process_is_running(pid)
                {
                    return Err(AppError::DaemonAlreadyRunning);
                }

                if std::time::Instant::now() >= deadline {
                    storage::remove_file_if_exists(&config.paths.lock_file)?;
                    storage::remove_file_if_exists(&config.paths.socket_file)?;
                    return storage::try_acquire_daemon_lock(&config.paths.lock_file);
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
    no_auth_without_ask: bool,
    no_http: bool,
    auth_hash_internal: Option<String>,
) -> Result<()> {
    if daemon_is_healthy(&config).await {
        // A start request with different flags cannot be honored while
        // another daemon owns this state directory (the common case: an
        // auto-spawned or earlier daemon runs with HTTP enabled and the
        // user now asks for --no-http). Say what is actually running and
        // how to replace it instead of failing with a bare error.
        eprintln!("a daemon is already running with this state directory:");
        eprintln!();
        eprint!("{}", running_daemon_summary(&config));
        eprintln!();
        eprintln!("stop it first (`oly daemon stop`) to start with different flags.");
        return Err(AppError::DaemonAlreadyRunning);
    }

    let no_auth = no_auth || no_auth_without_ask;

    let auth_hash: Option<String> = if foreground_internal {
        // Prefer the env var (new, secure), fall back to the CLI arg (legacy).
        auth_hash_internal.or_else(|| {
            std::env::var("OLY_AUTH_HASH_INTERNAL").ok().and_then(|v| {
                // Clear immediately after reading to minimise exposure window.
                // SAFETY: We are the only thread reading this variable at startup.
                unsafe { std::env::remove_var("OLY_AUTH_HASH_INTERNAL") };
                if v.is_empty() { None } else { Some(v) }
            })
        })
    } else if !no_http {
        if no_auth {
            if no_auth_without_ask {
                warn!(
                    "HTTP authentication disabled without confirmation. Make sure you understand the security implications."
                );
            } else {
                confirm_no_auth_risk()?;
            }
            None
        } else {
            let hash = prompt_and_hash_password()?;
            Some(hash)
        }
    } else {
        None
    };

    if detach && !foreground_internal {
        // Forward only genuine CLI flags to the detached child. Forwarding
        // the merged (file + CLI) values would pin them as runtime overrides
        // in the child, where they win over config.json on every hot reload
        // and silently defeat later file edits.
        let overrides = &config.runtime_overrides;
        let child_pid = spawn_detached(
            no_auth,
            no_http,
            auth_hash.as_deref(),
            overrides.http_bind.as_deref(),
            overrides.http_port,
            overrides.notification_hook.as_deref(),
            overrides.web_push_proxy.as_deref(),
            &config.paths.state_dir,
        )?;
        wait_for_daemon_ready(&config, Some(child_pid), std::time::Duration::from_secs(60)).await?;

        println!("Daemon started in background.");
        print_detached_start_summary(&config, no_http, no_auth);
        return Ok(());
    }

    run_foreground(config, auth_hash, no_http).await
}

pub async fn status(config: AppConfig) -> Result<()> {
    if !daemon_is_healthy(&config).await {
        eprintln!("Daemon is not running.");
        return Ok(());
    }

    println!("Daemon is running...");

    let info = storage::read_daemon_info(&config.paths.info_file)?;
    let (no_http, no_auth, started_at) = info
        .as_ref()
        .map(|i| (i.no_http, i.no_auth, Some(i.started_at.clone())))
        .unwrap_or((false, false, None));

    if let Some(started_at) = started_at {
        println!("Started at:   {}", format_status_timestamp(&started_at));
    }

    let effective = effective_status_config(&config, info.as_ref());
    print_detached_start_summary(&effective, no_http, no_auth);
    // Surface the daemon's SSH-key identity so the operator can paste it
    // onto a peer (or hand it to `oly node accept -k ...`). Reads from
    // the on-disk `.pub` file (lifecycle creates it on daemon start) so
    // status works even if the daemon was started after this CLI process.
    match crate::http::NodeIdentity::read_published_pubkey(&config.paths.state_dir) {
        Ok(Some(pub_key)) => {
            println!("SSH PUB:      {pub_key}");
        }
        Ok(None) => {
            println!("SSH PUB:      (not generated; start the daemon at least once)");
        }
        Err(err) => {
            eprintln!("warning: failed to read SSH pub key: {err}");
        }
    }
    Ok(())
}

/// Resolve the config view `status`/already-running messages report from:
/// the running daemon's own recorded flags and effective HTTP endpoint win
/// over the client's config file (CLI `--bind`/`--port` overrides and any
/// config drift since the daemon started would otherwise misreport).
fn effective_status_config(config: &AppConfig, info: Option<&storage::DaemonInfo>) -> AppConfig {
    let mut effective = config.clone();
    if let Some(info) = info {
        effective.http.bind = info.http_bind.clone();
        effective.http.port = info.http_port;
    }
    effective
}

/// The summary of the currently running daemon for the already-running
/// error path: same source-of-truth rules as `daemon status`.
fn running_daemon_summary(config: &AppConfig) -> String {
    let info = storage::read_daemon_info(&config.paths.info_file)
        .ok()
        .flatten();
    let (no_http, no_auth) = info
        .as_ref()
        .map(|i| (i.no_http, i.no_auth))
        .unwrap_or((false, false));
    let effective = effective_status_config(config, info.as_ref());
    detached_start_summary(&effective, no_http, no_auth)
}

fn format_status_timestamp(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|_| value.to_string())
}

fn print_detached_start_summary(config: &AppConfig, no_http: bool, no_auth: bool) {
    print!("{}", detached_start_summary(config, no_http, no_auth));
}

fn detached_start_summary(config: &AppConfig, no_http: bool, no_auth: bool) -> String {
    let mut out = String::new();

    if no_http {
        let _ = writeln!(out, "HTTP:         disabled (--no-http)");
    } else {
        let _ = writeln!(
            out,
            "HTTP:         {}",
            format_http_url(&config.http.bind, config.http.port)
        );
        let _ = writeln!(
            out,
            "Auth:         {}",
            if no_auth {
                "disabled (--no-auth)"
            } else {
                "enabled"
            }
        );
    }

    let _ = writeln!(out, "ROOT:         {}", config.paths.state_dir.display());
    let _ = writeln!(
        out,
        "LOGS:         {}",
        config.paths.state_dir.join("logs").display()
    );
    let _ = writeln!(out, "SESSIONS:     {}", config.paths.sessions_dir.display());
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

async fn wait_for_daemon_ready(
    config: &AppConfig,
    expected_pid: Option<u32>,
    timeout: std::time::Duration,
) -> Result<()> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if daemon_is_healthy(config).await {
            return Ok(());
        }

        // For detached start, track the actual spawned child PID rather than the
        // lockfile PID, which may still contain a stale value until the new daemon
        // acquires the startup lock and writes its own PID.
        if let Some(pid) = expected_pid
            && !process_is_running(pid)
        {
            return Err(AppError::DaemonUnavailable(format!(
                "daemon process exited before becoming ready. Check logs under {}",
                config.paths.state_dir.display()
            )));
        }

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }

    Err(AppError::DaemonUnavailable(
        "daemon failed to become ready in time".to_string(),
    ))
}

/// Command-line arguments for the detached daemon child.
///
/// Only genuine CLI overrides are forwarded: anything the child receives as
/// a flag becomes a runtime override that wins over `config.json` on every
/// hot reload, so forwarding merged (file + CLI) values would silently pin
/// them and defeat later file edits. Everything else is read from
/// `config.json` by the child itself.
fn detached_child_args(
    bind: Option<&str>,
    port: Option<u16>,
    notification_hook: Option<&str>,
    web_push_proxy: Option<&str>,
    no_auth: bool,
    no_http: bool,
) -> Vec<String> {
    let mut args = vec![
        "daemon".to_string(),
        "start".to_string(),
        "--foreground-internal".to_string(),
    ];
    if let Some(bind) = bind {
        args.push("--bind".to_string());
        args.push(bind.to_string());
    }
    if let Some(port) = port {
        args.push("--port".to_string());
        args.push(port.to_string());
    }
    if no_auth {
        args.push("--no-auth".to_string());
    }
    if no_http {
        args.push("--no-http".to_string());
    }
    if let Some(notification_hook) = notification_hook {
        args.push("--notification-hook".to_string());
        args.push(notification_hook.to_string());
    }
    if let Some(web_push_proxy) = web_push_proxy {
        args.push("--web-push-proxy".to_string());
        args.push(web_push_proxy.to_string());
    }
    args
}

#[allow(clippy::too_many_arguments)]
fn spawn_detached(
    no_auth: bool,
    no_http: bool,
    auth_hash: Option<&str>,
    bind: Option<&str>,
    port: Option<u16>,
    notification_hook: Option<&str>,
    web_push_proxy: Option<&str>,
    state_dir: &Path,
) -> Result<u32> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(detached_child_args(
        bind,
        port,
        notification_hook,
        web_push_proxy,
        no_auth,
        no_http,
    ));

    // The detached child has no console and nobody reads its stdout/stderr,
    // so historically `Stdio::null()` silently discarded everything the
    // process ever wrote there. That is exactly where the default Rust panic
    // hook prints ("thread 'x' panicked at ..."), where the stack-overflow
    // guard page handler prints before aborting, and where a lot of native
    // (non-Rust) crash output ends up. Redirect stderr to a durable file
    // instead so a future crash leaves evidence behind; stdin/stdout remain
    // discarded since nothing meaningful is expected there.
    let stderr_log_path = crash::stderr_log_path(state_dir);
    if let Some(parent) = stderr_log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let stderr_file = File::options()
        .create(true)
        .append(true)
        .open(&stderr_log_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_file);

    // Ensure panics/aborts print a backtrace to the (now-captured) stderr
    // file, unless the caller already asked for a specific level.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        cmd.env("RUST_BACKTRACE", "1");
    }

    if !no_auth && let Some(hash) = auth_hash {
        // Pass the Argon2 hash via an environment variable instead of a CLI
        // argument.  CLI args are visible to all local users via `ps aux` /
        // `/proc/<pid>/cmdline`, which would leak the PHC hash.
        cmd.env("OLY_AUTH_HASH_INTERNAL", hash);
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
    let child = cmd.spawn()?;
    Ok(child.id())
}

async fn run_foreground(config: AppConfig, auth_hash: Option<String>, no_http: bool) -> Result<()> {
    let config = Arc::new(config);

    info!(
        no_http,
        auth_enabled = auth_hash.is_some(),
        state_dir = ?config.paths.state_dir,
        sessions_dir = ?config.paths.sessions_dir,
        "daemon foreground initialization"
    );

    storage::ensure_state_dirs(&config.paths.state_dir, &config.paths.sessions_dir)?;

    let lock = acquire_daemon_start_lock(&config).await?;

    storage::write_pid(&config.paths.lock_file, std::process::id())?;

    let no_auth = auth_hash.is_none();
    storage::write_daemon_info(
        &config.paths.info_file,
        &storage::DaemonInfo {
            no_http,
            no_auth,
            started_at: chrono::Utc::now().to_rfc3339(),
            http_bind: config.http.bind.clone(),
            http_port: config.http.port,
        },
    )?;

    let _guard = DaemonGuard {
        _lock: lock,
        config: Arc::clone(&config),
    };

    let file_appender =
        tracing_appender::rolling::daily(config.paths.state_dir.join("logs"), "daemon.log");
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .compact()
        .without_time()
        .with_target(false);

    // Reloadable filter layer: the config hot-reload task swaps in a new
    // EnvFilter when `log_level` changes in config.json.
    let (env_filter_layer, log_filter_handle) =
        tracing_subscriber::reload::Layer::new(build_env_filter(&config));

    tracing_subscriber::registry()
        .with(env_filter_layer)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    // Install crash diagnostics as early as possible so nothing that happens
    // afterwards (DB open, session restore, HTTP/PTY startup, ...) can go
    // wrong silently. See daemon::crash for why this is needed: the process
    // normally runs fully detached with stdout/stderr discarded, so panics
    // and native crashes previously left no trace at all.
    crash::install_panic_hook(config.paths.state_dir.clone());
    crash::install_native_crash_handler(config.paths.state_dir.clone());

    let pid = std::process::id();
    info!(pid, log_level = %config.log_level, "daemon started");

    let db =
        Arc::new(Database::open(&config.paths.db_file, config.paths.sessions_dir.clone()).await?);
    info!(db_file = ?config.paths.db_file, "database opened");

    let node_registry = Arc::new(NodeRegistry::new());
    let (notification_tx, _) = tokio::sync::broadcast::channel::<NotificationEvent>(100);

    let join_handles: JoinHandles = Arc::new(Mutex::new(std::collections::HashMap::new()));

    // Remove any stale socket file left by a crashed daemon.  On macOS (and
    // other platforms without abstract-namespace sockets) the file-based Unix
    // domain socket persists on disk after an unclean exit.  Binding to an
    // existing socket file fails with EADDRINUSE, which silently prevents the
    // daemon from starting and makes the parent `wait_for_daemon_ready` loop
    // appear to hang.
    storage::remove_file_if_exists(&config.paths.socket_file)?;

    let listener = ipc::bind(&config)?;
    info!(socket_file = ?config.paths.socket_file, "ipc listener bound");
    let (store, startup_failed_sessions) = {
        let store = SessionStore::with_journal_byte_cap(
            config.limits.session_eviction_seconds,
            config.limits.max_journal_bytes_per_session,
            db.clone(),
        );
        let startup_failed_sessions = store.load_running_stopping_sessions().await;
        (store, startup_failed_sessions)
    };
    let session_store = Arc::new(store);
    let event_tx = session_store.event_tx();
    for join in client::join::load_join_configs(&config) {
        // Lifecycle replay runs at daemon startup before any IPC
        // client is around; there's no caller to surface the first
        // attempt outcome to, so don't pass an `on_attempt` oneshot.
        let (abort, stop_tx) = super::rpc_nodes::spawn_join_connector(
            join.clone(),
            Arc::clone(&config),
            event_tx.subscribe(),
            None,
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
    let notifier: NotifierHandle = Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::notification::build_notifier(db.clone(), &config),
    ));

    // Shared live view of the configuration. The hot-reload task swaps in a
    // rebuilt AppConfig when config.json changes on disk; subsystems read
    // through this to pick up hot-reloadable settings without a restart.
    let live_config = LiveConfig::from_arc(Arc::clone(&config));

    let auth_state = auth_hash.map(AuthState::new);
    // Every daemon — primary or secondary, HTTP-enabled or not — auto-
    // generates (or loads) an Ed25519 identity key at startup. The same
    // keypair serves both federation roles: primaries use it to sign the
    // host-challenge nonce, secondaries use it to sign the join payload.
    // We still tolerate a failed load: in that case SSH-key joins in
    // either direction are rejected (API key auth still works).
    let node_identity = http::NodeIdentity::create_or_load(&config.paths.state_dir)
        .await
        .inspect_err(
            |e| warn!(%e, "failed to create node identity key, SSH-key node joins will be rejected"),
        )
        .unwrap_or_else(|_| http::NodeIdentity::disabled());
    if !no_http {
        let http_state = http::AppState {
            store: session_store.clone(),
            config: live_config.clone(),
            db: db.clone(),
            notifier: notifier.clone(),
            event_tx: event_tx.clone(),
            auth: auth_state,
            node_registry: node_registry.clone(),
            node_identity: node_identity.clone(),
        };
        tokio::spawn(http::serve(http_state));
        info!("http server task spawned");
    } else {
        info!("http server disabled by --no-web");
    }

    let notify_store = session_store.clone();
    let notify_config = live_config.clone();
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

    {
        let reload_config = live_config.clone();
        let reload_store = session_store.clone();
        let reload_notifier = notifier.clone();
        let reload_db = db.clone();
        tokio::spawn(async move {
            super::reload::run_config_reloader(
                reload_config,
                reload_store,
                reload_notifier,
                reload_db,
                log_filter_handle,
            )
            .await;
        });
        info!("config hot-reload task spawned");
    }

    {
        let retention_config = live_config.clone();
        let retention_db = db.clone();
        let retention_root = config.paths.sessions_dir.clone();
        tokio::spawn(async move {
            super::journal_retention::run_journal_retention_sweeper(
                retention_config,
                retention_db,
                retention_root,
            )
            .await;
        });
        info!("journal retention sweeper task spawned");
    }

    if !startup_failed_sessions.is_empty() {
        let notifier = notifier.clone();
        let event = NotificationEvent::startup_recovery(&startup_failed_sessions);
        let outcome = notifier.load_full().dispatch(&event).await;

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
        let _ = event_tx.send(event.into_session_event(0, true));
    }

    // M5-5: service managers stop daemons with SIGTERM; give it the same
    // graceful drain as ctrl-c and the RPC stop path. (cfg'd out on Windows:
    // select! arms can't carry cfg attributes, so the future pends forever.)
    #[cfg(unix)]
    let sigterm_recv = {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        async move { sigterm.recv().await }
    };
    #[cfg(not(unix))]
    let sigterm_recv = std::future::pending::<Option<()>>();
    tokio::pin!(sigterm_recv);

    let mut session_maintenance_tick = tokio::time::interval(Duration::from_secs(1));
    session_maintenance_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("daemon received ctrl-c, draining sessions before shutdown");
                drain_sessions_for_shutdown(&session_store).await;
                break;
            }
            _ = &mut sigterm_recv => {
                info!("daemon received SIGTERM, draining sessions before shutdown");
                drain_sessions_for_shutdown(&session_store).await;
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
                        let live_config_clone = live_config.clone();
                        let store_clone = session_store.clone();
                        let shutdown_tx_clone = shutdown_tx.clone();
                        let registry_clone = node_registry.clone();
                        let db_clone = db.clone();
                        let handles_clone = join_handles.clone();
                        let event_tx_clone = event_tx.clone();
                        let notification_tx_clone = notification_tx.clone();
                        let notifier_clone = notifier.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_client(
                                stream,
                                config_clone,
                                live_config_clone,
                                store_clone,
                                shutdown_tx_clone,
                                registry_clone,
                                db_clone,
                                handles_clone,
                                event_tx_clone,
                                notification_tx_clone,
                                notifier_clone,
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

/// Grace window for the signal-driven shutdown drain. Matches the
/// `oly daemon stop` default so both shutdown paths behave identically.
const DAEMON_SHUTDOWN_GRACE_SECONDS: u64 = 15;

/// M5-5: signal-driven shutdown must not orphan managed children — drain
/// every session (soft stop, process-group SIGTERM, tree kill) before the
/// daemon exits. `oly daemon stop` drains in the RPC handler before the
/// shutdown signal even arrives, so both paths converge here.
async fn drain_sessions_for_shutdown(session_store: &SessionStoreHandle) {
    let drained = session_store
        .stop_all_sessions(DAEMON_SHUTDOWN_GRACE_SECONDS)
        .await;
    if !drained {
        warn!("shutdown drain finished with sessions still running");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{detached_start_summary, effective_status_config, format_status_timestamp};
    use crate::config::AppConfig;
    use crate::storage;

    fn test_config() -> AppConfig {
        let state_dir = PathBuf::from("test-state");
        AppConfig {
            paths: crate::config::PathsConfig {
                state_dir: state_dir.clone(),
                sessions_dir: state_dir.join("sessions"),
                db_file: state_dir.join("oly.db"),
                socket_name: "test.sock".to_string(),
                socket_file: state_dir.join("daemon.sock"),
                info_file: state_dir.join("daemon.info"),
                lock_file: state_dir.join("daemon.lock"),
            },
            http: crate::config::HttpConfig {
                bind: "127.0.0.1".to_string(),
                port: 15443,
            },
            notify: crate::config::NotifyConfig {
                min_interval_seconds: 10,
                prompt_patterns: Vec::new(),
                hook: None,
            },
            limits: crate::config::LimitsConfig {
                max_running_sessions: 50,
                session_eviction_seconds: 15,
                screen_scrollback_rows: crate::config::DEFAULT_SCREEN_SCROLLBACK_ROWS,
                silence_seconds: 10,
                stop_grace_seconds: 5,
                max_journal_bytes_per_session: 0,
                journal_retention_days: 0,
            },
            web_push: crate::config::WebPushConfig {
                subject: None,
                vapid_public_key: None,
                vapid_private_key: None,
                proxy: None,
            },
            log_level: "info".to_string(),
            runtime_overrides: Default::default(),
        }
    }

    #[test]
    fn detached_child_args_forward_only_explicit_cli_overrides() {
        // Regression: merged config values (e.g. notification_hook loaded
        // from config.json) must NOT be forwarded as CLI flags — they would
        // become runtime overrides in the child and pin the value against
        // later config.json edits, silently defeating hot reload.
        let args = super::detached_child_args(None, None, None, None, false, false);
        assert_eq!(args, ["daemon", "start", "--foreground-internal"]);

        let args = super::detached_child_args(
            Some("0.0.0.0"),
            Some(17000),
            Some("pwsh -File C:\\hooks\\notify.ps1"),
            Some("socks5://127.0.0.1:1080"),
            true,
            true,
        );
        assert_eq!(
            args,
            [
                "daemon",
                "start",
                "--foreground-internal",
                "--bind",
                "0.0.0.0",
                "--port",
                "17000",
                "--no-auth",
                "--no-http",
                "--notification-hook",
                "pwsh -File C:\\hooks\\notify.ps1",
                "--web-push-proxy",
                "socks5://127.0.0.1:1080",
            ]
        );
    }

    #[test]
    fn detached_summary_includes_http_url_and_paths() {
        let config = test_config();
        let summary = detached_start_summary(&config, false, true);

        assert!(summary.contains("HTTP:         http://127.0.0.1:15443"));
        assert!(summary.contains("Auth:         disabled (--no-auth)"));
        assert!(summary.contains(&format!(
            "ROOT:         {}",
            config.paths.state_dir.display()
        )));
        assert!(summary.contains(&format!(
            "LOGS:         {}",
            config.paths.state_dir.join("logs").display()
        )));
        assert!(summary.contains(&format!(
            "SESSIONS:     {}",
            config.paths.sessions_dir.display()
        )));
    }

    #[test]
    fn detached_summary_marks_http_disabled() {
        let config = test_config();
        let summary = detached_start_summary(&config, true, false);

        assert!(summary.contains("HTTP:         disabled (--no-http)"));
        assert!(!summary.contains("Auth:"));
    }

    #[test]
    fn status_endpoint_prefers_the_running_daemons_recorded_values() {
        // The daemon was started with `--port 9999` (a runtime override
        // that never reaches config.json): status must report the running
        // daemon's effective endpoint, not the client's config default.
        let config = test_config();
        let info = storage::DaemonInfo {
            no_http: false,
            no_auth: true,
            started_at: "2026-03-27T12:34:56Z".to_string(),
            http_bind: "0.0.0.0".to_string(),
            http_port: 9999,
        };
        let effective = effective_status_config(&config, Some(&info));
        let summary = detached_start_summary(&effective, info.no_http, info.no_auth);
        assert!(summary.contains("HTTP:         http://0.0.0.0:9999"));
        assert!(!summary.contains("15443"));
    }

    #[test]
    fn status_endpoint_falls_back_to_client_config_without_info() {
        let config = test_config();
        let effective = effective_status_config(&config, None);
        assert_eq!(effective.http.bind, "127.0.0.1");
        assert_eq!(effective.http.port, 15443);
    }

    #[test]
    fn daemon_info_without_endpoint_fields_still_parses() {
        // Info files written before the endpoint fields existed must stay
        // readable; the defaults mirror config.rs's built-in endpoint.
        let legacy = r#"{"no_http":true,"no_auth":true,"started_at":"2026-03-27T12:34:56Z"}"#;
        let info: storage::DaemonInfo = serde_json::from_str(legacy).expect("legacy info parses");
        assert!(info.no_http);
        assert_eq!(info.http_bind, "127.0.0.1");
        assert_eq!(info.http_port, 15443);
    }

    #[test]
    fn status_timestamp_is_rendered_in_local_time() {
        let raw = "2026-03-27T12:34:56Z";
        let expected = chrono::DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&chrono::Local)
            .to_rfc3339();

        assert_eq!(format_status_timestamp(raw), expected);
    }

    #[test]
    fn status_timestamp_falls_back_to_original_when_parse_fails() {
        let raw = "not-a-timestamp";
        assert_eq!(format_status_timestamp(raw), raw);
    }
}
