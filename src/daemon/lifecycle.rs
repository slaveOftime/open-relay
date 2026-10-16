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
    protocol::{DaemonStatusReport, RpcRequest, RpcResponse},
    session::SessionStore,
    storage,
    utils::format_http_url,
};

use super::{
    JoinHandles, NotifierHandle, SessionStoreHandle,
    auth::{confirm_no_auth_risk, prompt_and_hash_password},
    components::{ComponentRegistry, spawn_supervised},
    crash,
    health::DaemonHealth,
    rpc::handle_client,
};

/// Every supervised daemon-lifetime component: (name, ready-critical?).
/// Ready-critical components gate `/api/ready`, the `DaemonStatus` RPC, and
/// the daemon-start readiness probe. Registered before any spawn so
/// readiness is never trivially true while a component is still starting.
const SUPERVISED_COMPONENTS: &[(&str, bool)] = &[
    // HTTP is registered here even under `--no-http`, where it is marked
    // disabled instead of spawned (disabled satisfies readiness).
    ("http", true),
    ("sse_encoder", true),
    ("activity_sampler", true),
    ("notification_monitor", true),
    ("disk_sampler", true),
    // Convenience loops: supervised and visible in diagnostics, but a dead
    // config reloader or retention sweeper does not make the daemon
    // unready (running sessions still work; the failure is not silent).
    ("config_reloader", false),
    ("journal_retention", false),
];

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

pub async fn status(config: AppConfig, node: Option<String>, json: bool) -> Result<()> {
    if !daemon_is_healthy(&config).await {
        if json {
            // Machine-readable "not running" so scripts do not have to parse
            // prose; still exit 0 like the human path (a stopped daemon is
            // not a command failure).
            println!("{}", serde_json::json!({ "alive": false }));
        } else {
            eprintln!("Daemon is not running.");
        }
        return Ok(());
    }

    // Remote node: the local daemon relays the read-only status request.
    // Nothing is presented as "the local daemon" — every field in the report
    // describes the node's own daemon and its own machine paths.
    let request = match &node {
        Some(name) => RpcRequest::NodeProxy {
            node: name.clone(),
            inner: Box::new(RpcRequest::DaemonStatus),
        },
        None => RpcRequest::DaemonStatus,
    };

    if json {
        let report = match ipc::send_request_checked(&config, request).await? {
            RpcResponse::DaemonStatus { status } => status,
            other => {
                return Err(AppError::Protocol(format!(
                    "unexpected response for daemon status: {other:?}"
                )));
            }
        };
        let rendered = serde_json::to_string_pretty(&report)
            .map_err(|err| AppError::Protocol(format!("failed to encode daemon status: {err}")))?;
        println!("{rendered}");
        return Ok(());
    }

    if let Some(node) = &node {
        println!("Daemon status for node: {}", node);
        return match ipc::send_request_checked(&config, request).await? {
            RpcResponse::DaemonStatus { status } => {
                print_daemon_status_report(&status);
                Ok(())
            }
            other => Err(AppError::Protocol(format!(
                "unexpected response for daemon status of node {node}: {other:?}"
            ))),
        };
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

    // Read-only diagnostic summary: readiness, component states, disk
    // capacity, failure-record locations, affected sessions. Printed from
    // the daemon's own view; a degraded daemon still answers this. Human
    // mode degrades loudly if an older daemon cannot answer; the summary
    // above is still useful.
    match ipc::send_request_checked(&config, request).await {
        Ok(RpcResponse::DaemonStatus { status }) => print_daemon_status_report(&status),
        Ok(other) => {
            eprintln!("warning: unexpected response to daemon status request: {other:?}");
        }
        Err(err) => {
            eprintln!("warning: daemon status report unavailable: {err}");
        }
    }
    Ok(())
}

/// Print the daemon's diagnostic summary. Every unknown stays visibly
/// unknown: missing sample, missing reason, partial breakdown.
fn print_daemon_status_report(report: &DaemonStatusReport) {
    println!("Version:      {}", report.version);
    println!(
        "Readiness:    {}",
        if report.ready { "ready" } else { "DEGRADED" }
    );
    for reason in &report.unready_reasons {
        println!("              - {reason}");
    }
    if !report.components.is_empty() {
        println!("Components:");
        for component in &report.components {
            let critical = if component.ready_critical {
                ""
            } else {
                " (non-critical)"
            };
            let failures = if component.failures > 0 {
                format!(", {} failures", component.failures)
            } else {
                String::new()
            };
            println!(
                "  {:<20} {}{}{}",
                component.name, component.state, critical, failures
            );
            if let Some(reason) = &component.last_failure {
                println!("  {:<20}   last failure: {}", "", reason);
            }
        }
    }

    match report.disk.free_bytes {
        Some(free) => println!("Disk free:    {}", crate::utils::format_bytes_human(free)),
        None => println!(
            "Disk free:    unknown ({})",
            report
                .disk
                .free_unknown_reason
                .as_deref()
                .unwrap_or("no sample yet")
        ),
    }
    if let Some(sampled_at) = report.disk.sampled_at {
        let age = (chrono::Utc::now() - sampled_at).num_seconds().max(0);
        println!("Disk sampled: {}s ago", age);
    }
    let usage = [
        ("journal", report.disk.journal_bytes),
        ("uploads", report.disk.uploads_bytes),
        ("sqlite", report.disk.sqlite_bytes),
        ("logs", report.disk.logs_bytes),
    ]
    .iter()
    .filter_map(|(label, bytes)| {
        bytes.map(|bytes| format!("{label} {}", crate::utils::format_bytes_human(bytes)))
    })
    .collect::<Vec<_>>();
    if !usage.is_empty() {
        println!("Disk usage:   {}", usage.join(", "));
    }
    if let Some(reason) = &report.disk.breakdown_partial_reason {
        println!("Disk usage:   partial ({reason})");
    }
    let guard = if report.disk.guard_enabled {
        report.disk.guard_decision.clone()
    } else {
        format!("{} (opt-in; disabled)", report.disk.guard_decision)
    };
    println!("Disk guard:   {}", guard);
    if let Some(reason) = &report.disk.guard_refusal_reason {
        println!("              new sessions refused: {reason}");
    }
    if report.disk.warning_active {
        println!(
            "Disk warning: free space below {}",
            crate::utils::format_bytes_human(report.disk.warn_below_bytes)
        );
    }

    if !report.failure_records.is_empty() {
        println!("Failure records:");
        for path in &report.failure_records {
            println!("  {}", path);
        }
    }

    if !report.affected_sessions.is_empty() {
        println!("Affected sessions ({}):", report.affected_sessions_total);
        for session in &report.affected_sessions {
            println!("  {} [{}] {}", session.id, session.status, session.reason);
        }
        let hidden = report
            .affected_sessions_total
            .saturating_sub(report.affected_sessions.len());
        if hidden > 0 {
            println!("  ... and {} more", hidden);
        }
    }
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

    // Surface the daemon's identity so the operator can paste it onto a
    // peer (or hand it to `oly node accept -k ...`). Reads from the
    // on-disk `.pub` file (lifecycle creates it on daemon start) so status
    // works even if the daemon was started after this CLI process.
    match crate::http::NodeIdentity::read_published_pubkey(&config.paths.state_dir) {
        Ok(Some(pub_key)) => {
            let _ = writeln!(out, "ID-PUB-KEY:   {}", pub_key);
        }
        Ok(None) => {
            let _ = writeln!(
                out,
                "ID-PUB-KEY:  (not generated; start the daemon at least once)"
            );
        }
        Err(err) => {
            let _ = writeln!(out, "ID-PUB-KEY:  failed to read id pub key: {}", err);
        }
    }

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

    let node_registry = NodeRegistry::new();
    let (notification_tx, _) = tokio::sync::broadcast::channel::<NotificationEvent>(100);
    // Wrap in Arc now so every consumer (HTTP, IPC, federation, activity
    // sampler) shares the same pool. `bind_event_tx` is interior-mutating
    // (OnceLock) and can be called through the Arc.
    let node_registry = Arc::new(node_registry);

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
            // Live journal deletion is disabled: a byte cap cannot bound
            // the current incarnation, so production stores run
            // "unlimited" and removed config keys fail explicitly instead
            // of silently doing nothing. Only `journal_retention_days`
            // (stopped-session age) deletes production state.
            0,
            db.clone(),
        );
        store.set_resume_patterns(config.resume.patterns.clone());
        let startup_failed_sessions = store.load_running_stopping_sessions().await;
        (store, startup_failed_sessions)
    };
    let session_store = Arc::new(store);
    let event_tx = session_store.event_tx();
    // CR-4 (node_state fan-out): wire the registry to event_tx so
    // connect/disconnect publish `SessionEvent::NodeState {…}` for any
    // active SSE listener to consume without polling.
    node_registry.bind_event_tx(event_tx.clone());
    for join in client::join::load_join_configs(&config) {
        // Lifecycle replay runs at daemon startup before any IPC
        // client is around; there's no caller to surface the first
        // attempt outcome to, so don't pass an `on_attempt` oneshot.
        let (abort, stop_tx) = super::rpc_nodes::spawn_join_connector(
            join.clone(),
            Arc::clone(&config),
            event_tx.clone(),
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

    // Supervised components: register everything up front, then run
    // each long-lived task through `spawn_supervised` so an early exit or
    // panic is recorded by name and restarted with a bounded budget instead
    // of silently disappearing. The disk observer is installed on the store
    // before any session can start.
    let components = Arc::new(ComponentRegistry::new());
    for &(name, ready_critical) in SUPERVISED_COMPONENTS {
        components.register(name, ready_critical);
    }
    let disk_monitor = Arc::new(crate::disk::DiskMonitor::new(
        live_config.clone(),
        Box::new(crate::disk::OsSpaceProbe),
    ));
    session_store.set_disk_monitor(Arc::clone(&disk_monitor));
    let daemon_health = DaemonHealth::new(Arc::clone(&components), Arc::clone(&disk_monitor));

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
        let (event_bytes_tx, _) = tokio::sync::broadcast::channel::<http::EncodedSessionEvent>(
            http::sse::SSE_BROADCAST_CAPACITY,
        );
        let http_state = http::AppState {
            store: session_store.clone(),
            config: live_config.clone(),
            db: db.clone(),
            notifier: notifier.clone(),
            event_tx: event_tx.clone(),
            event_bytes_tx: event_bytes_tx.clone(),
            auth: auth_state,
            node_registry: node_registry.clone(),
            node_identity: node_identity.clone(),
            daemon: Some(daemon_health.clone()),
        };
        // CR-3: one dedicated task that drains `event_tx` and re-broadcasts
        // the encoded frames. Every SSE handler that subscribes afterward
        // does NOT re-serialize events, so a busy relay does not multiply
        // CPU per reminder count.
        spawn_supervised(Arc::clone(&components), "sse_encoder", true, {
            let event_tx = event_tx.clone();
            let event_bytes_tx = event_bytes_tx.clone();
            move || {
                let event_bytes_tx = event_bytes_tx.clone();
                // Subscribe in the factory, which `spawn_supervised` invokes
                // at wiring time (and again on restart): a receiver-less
                // broadcast drops events, so the subscription must exist
                // before the encoder's first poll.
                let rx = event_tx.subscribe();
                async move {
                    http::run_event_encoder(rx, event_bytes_tx).await;
                    // Reached only if event_tx closed while the daemon
                    // still runs; the supervisor records and restarts.
                    Err("SSE encoder stopped: session event channel closed".to_string())
                }
            }
        });
        spawn_supervised(Arc::clone(&components), "http", true, move || {
            let state = http_state.clone();
            async move { http::serve(state).await }
        });
        info!("http server task spawned");
    } else {
        components.mark_disabled("http");
        components.mark_disabled("sse_encoder");
        info!("http server disabled by --no-http");
    }

    {
        let activity_store = session_store.clone();
        let activity_event_tx = event_tx.clone();
        spawn_supervised(
            Arc::clone(&components),
            "activity_sampler",
            true,
            move || {
                let store = activity_store.clone();
                let event_tx = activity_event_tx.clone();
                async move {
                    store.run_activity_sampler(event_tx).await;
                    Err("activity sampler stopped".to_string())
                }
            },
        );
        info!("session activity sampler task spawned");
    }

    spawn_supervised(Arc::clone(&components), "notification_monitor", true, {
        let notify_store = session_store.clone();
        let notify_config = live_config.clone();
        let notify_event_tx = event_tx.clone();
        let notify_notification_tx = notification_tx.clone();
        let notify_notifier = notifier.clone();
        move || {
            let store = notify_store.clone();
            let config = notify_config.clone();
            let event_tx = notify_event_tx.clone();
            let notification_tx = notify_notification_tx.clone();
            let notifier = notify_notifier.clone();
            async move {
                crate::notification::run_notification_monitor(
                    notifier,
                    store,
                    config,
                    event_tx,
                    notification_tx,
                )
                .await;
                Err("notification monitor stopped: event channel closed".to_string())
            }
        }
    });
    info!("notification monitor task spawned");

    spawn_supervised(Arc::clone(&components), "config_reloader", false, {
        let reload_config = live_config.clone();
        let reload_store = session_store.clone();
        let reload_notifier = notifier.clone();
        let reload_db = db.clone();
        move || {
            let config = reload_config.clone();
            let store = reload_store.clone();
            let notifier = reload_notifier.clone();
            let db = reload_db.clone();
            let log_filter_handle = log_filter_handle.clone();
            async move {
                super::reload::run_config_reloader(config, store, notifier, db, log_filter_handle)
                    .await;
                Err("config reloader stopped".to_string())
            }
        }
    });
    info!("config hot-reload task spawned");

    spawn_supervised(Arc::clone(&components), "journal_retention", false, {
        let retention_config = live_config.clone();
        let retention_db = db.clone();
        let retention_root = config.paths.sessions_dir.clone();
        move || {
            let config = retention_config.clone();
            let db = retention_db.clone();
            let root = retention_root.clone();
            async move {
                super::journal_retention::run_journal_retention_sweeper(config, db, root).await;
                Err("journal retention sweeper stopped".to_string())
            }
        }
    });
    info!("journal retention sweeper task spawned");

    spawn_supervised(Arc::clone(&components), "disk_sampler", true, {
        let monitor = Arc::clone(&disk_monitor);
        move || {
            let monitor = Arc::clone(&monitor);
            async move { crate::disk::run_disk_sampler(monitor).await }
        }
    });
    info!("state volume disk sampler task spawned");

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

    // Service managers stop daemons with SIGTERM; give it the same
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
                        let health_clone = daemon_health.clone();
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
                                health_clone,
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

/// Signal-driven shutdown must not orphan managed children — drain
/// every session (soft stop, process-group SIGTERM, tree kill) before the
/// daemon exits. `oly daemon stop` performs the same termination and resume
/// flush in its RPC handler before notifying the main loop to exit.
async fn drain_sessions_for_shutdown(session_store: &SessionStoreHandle) {
    let drained = session_store
        .stop_all_sessions(DAEMON_SHUTDOWN_GRACE_SECONDS)
        .await;
    if !drained {
        warn!("shutdown drain finished with sessions still running");
    }
    session_store.flush_resume_hints_on_shutdown().await;
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
                journal_retention_days: 0,
                disk_guard_enabled: false,
                disk_min_free_bytes: crate::config::DEFAULT_DISK_MIN_FREE_BYTES,
                disk_warn_free_bytes: crate::config::DEFAULT_DISK_WARN_FREE_BYTES,
                disk_sample_interval_seconds: 30,
                disk_sample_max_age_seconds: 120,
            },
            web_push: crate::config::WebPushConfig {
                subject: None,
                vapid_public_key: None,
                vapid_private_key: None,
                proxy: None,
            },
            resume: crate::config::ResumeConfig::default(),
            log_level: "info".to_string(),
            trusted_apps_enabled: false,
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
