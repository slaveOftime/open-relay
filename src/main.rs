mod cli;
mod client;
mod clipboard;
mod config;
mod daemon;
mod db;
mod error;
mod http;
mod ipc;
mod node;
mod notification;
mod protocol;
mod session;
mod storage;
mod terminal;
mod terminal_guards;
mod utils;

use clap::Parser;
use cli::{ApiKeyCommand, Cli, Commands, DaemonCommand, JoinCommand, NodeCommand, NotifyCommand};
use error::{AppError, Result};
use protocol::{ListQuery, ListSortField, RpcRequest, RpcResponse, SortOrder};
use std::path::{Path, PathBuf};

use crate::config::AppConfig;

#[cfg(not(windows))]
use libmimalloc_sys::{mi_option_set_default, mi_option_set_enabled_default};
#[cfg(not(windows))]
use mimalloc::MiMalloc;

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const OLY_SKILL_MARKDOWN: &str = include_str!("../skills/oly/SKILL.md");
const OLY_APPS_SKILL_MARKDOWN: &str = include_str!("../skills/oly-apps/SKILL.md");

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    configure_mimalloc_defaults();

    let code = match run().await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(windows)]
fn configure_mimalloc_defaults() {}

#[cfg(not(windows))]
fn configure_mimalloc_defaults() {
    // libmimalloc-sys does not expose the newer v3 purge constants as stable
    // Rust constants, so use the documented enum values from mimalloc.h.
    const MI_OPTION_PURGE_DECOMMITS: i32 = 5;
    const MI_OPTION_ABANDONED_PAGE_PURGE: i32 = 12;
    const MI_OPTION_PURGE_DELAY: i32 = 15;

    unsafe {
        // Set defaults instead of forcing values so users can still override
        // behavior through mimalloc environment variables when needed.
        mi_option_set_enabled_default(MI_OPTION_PURGE_DECOMMITS, true);
        mi_option_set_enabled_default(MI_OPTION_ABANDONED_PAGE_PURGE, true);
        mi_option_set_default(MI_OPTION_PURGE_DELAY, 0);
    }
}

/// Wrap `req` in a `NodeProxy` envelope when `node` is `Some`.
fn node_wrap(node: Option<String>, req: RpcRequest) -> RpcRequest {
    match node {
        None => req,
        Some(name) => RpcRequest::NodeProxy {
            node: name,
            inner: Box::new(req),
        },
    }
}

fn resolve_start_cwd(base_dir: &Path, cwd: Option<String>) -> Result<String> {
    let resolved = match cwd {
        Some(cwd) => {
            let path = PathBuf::from(cwd);
            if path.is_absolute() {
                path
            } else {
                base_dir.join(path)
            }
        }
        None => base_dir.to_path_buf(),
    };

    if !resolved.exists() {
        return Err(AppError::Protocol(format!(
            "working directory does not exist: {}",
            resolved.display()
        )));
    }

    if !resolved.is_dir() {
        return Err(AppError::Protocol(format!(
            "working directory is not a directory: {}",
            resolved.display()
        )));
    }

    Ok(resolved.to_string_lossy().into_owned())
}

/// Resolve a session ID: return the given ID or fetch the most recently created session.
async fn resolve_session_id(
    config: &AppConfig,
    id: Option<String>,
    node: Option<&String>,
) -> Result<String> {
    if let Some(id) = id {
        return Ok(id);
    }

    let query = ListQuery {
        search: None,
        tags: vec![],
        statuses: vec![],
        since: None,
        until: None,
        limit: 1,
        offset: 0,
        sort: ListSortField::CreatedAt,
        order: SortOrder::Desc,
    };
    let inner = RpcRequest::List { query };
    let request = match node {
        Some(name) => RpcRequest::NodeProxy {
            node: name.clone(),
            inner: Box::new(inner),
        },
        None => inner,
    };

    match ipc::send_request_checked(config, request).await? {
        RpcResponse::List { sessions, .. } => {
            if let Some(session) = sessions.into_iter().next() {
                Ok(session.id)
            } else {
                Err(AppError::Protocol(
                    "no sessions found; start one with: oly start --detach <cmd>".to_string(),
                ))
            }
        }
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = config::AppConfig::load()?;

    match cli.command {
        Commands::Skill(args) => {
            if args.apps {
                print!("{}", OLY_APPS_SKILL_MARKDOWN);
            } else {
                print!("{}", OLY_SKILL_MARKDOWN);
            }
            Ok(())
        }

        Commands::Daemon(args) => match args.command {
            DaemonCommand::Start(start_args) => {
                daemon::start(
                    config.with_runtime_overrides(
                        start_args.bind.clone(),
                        start_args.port,
                        start_args.notification_hook.clone(),
                        start_args.web_push_proxy.clone(),
                    ),
                    start_args.detach,
                    start_args.foreground_internal,
                    start_args.no_auth,
                    start_args.no_auth_without_ask,
                    start_args.no_http,
                    start_args.auth_hash_internal,
                )
                .await
            }
            DaemonCommand::Stop(stop_args) => daemon::stop(config, stop_args.grace).await,
            DaemonCommand::Status => daemon::status(config).await,
        },

        Commands::List(list_args) => client::run_list(&config, list_args).await,

        Commands::Start(start_args) => {
            let cli::StartArgs {
                title,
                tags,
                detach,
                disable_notifications,
                cwd,
                cmd_and_args,
                node,
            } = start_args;

            let mut iter = cmd_and_args.into_iter();
            let cmd = iter.next().unwrap(); // guaranteed by num_args = 1..
            let args: Vec<String> = iter.collect();
            let cwd = Some(resolve_start_cwd(&std::env::current_dir()?, cwd)?);
            let (rows, cols) = if detach {
                (None, None)
            } else {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                (Some(rows), Some(cols))
            };
            let inner = RpcRequest::Start {
                title,
                tags,
                cmd,
                args,
                cwd,
                rows,
                cols,
                disable_notifications,
            };
            let request = node_wrap(node, inner);
            match ipc::send_request_checked(&config, request).await? {
                RpcResponse::Start { session_id } => {
                    if detach {
                        println!("{session_id}");
                        return Ok(());
                    }
                    // Interactive attach takes control by default (see
                    // AttachArgs::role): the just-started session is driven
                    // by the terminal that launched it.
                    if let Err(err) =
                        client::run_attach(&config, &session_id, Some("takeover")).await
                    {
                        eprintln!();
                        eprintln!(
                            "warning: started session {session_id}, but failed to attach: {err}"
                        );
                    }
                    Ok(())
                }
                _ => Err(AppError::Protocol("unexpected response type".to_string())),
            }
        }

        Commands::Update(update_args) => {
            let inner = RpcRequest::SessionMetadataSet {
                id: update_args.id.clone(),
                title: update_args.title,
                tags: update_args.tags,
                notifications_enabled: update_args.notifications.map(|value| value.enabled()),
            };
            match ipc::send_request_checked(&config, node_wrap(update_args.node, inner)).await? {
                RpcResponse::Session { summary } => {
                    println!(
                        "Updated session {}. Title: {}. Tags: {}. Notifications: {}",
                        summary.id,
                        summary.title.as_deref().unwrap_or("—"),
                        if summary.tags.is_empty() {
                            "—".to_string()
                        } else {
                            summary.tags.join(", ")
                        },
                        if summary.notifications_enabled {
                            "enabled"
                        } else {
                            "disabled"
                        }
                    );
                    Ok(())
                }
                _ => Err(AppError::Protocol("unexpected response type".to_string())),
            }
        }

        Commands::Notify(notify_args) => match notify_args.command {
            NotifyCommand::Disable(args) => {
                let id = resolve_session_id(&config, args.id.clone(), args.node.as_ref()).await?;
                let inner = RpcRequest::NotifySet {
                    id: id.clone(),
                    enabled: false,
                };
                match ipc::send_request_checked(&config, node_wrap(args.node, inner)).await? {
                    RpcResponse::Ack => {
                        println!("Notifications disabled for session {id}.");
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response type".to_string())),
                }
            }
            NotifyCommand::Enable(args) => {
                let id = resolve_session_id(&config, args.id.clone(), args.node.as_ref()).await?;
                let inner = RpcRequest::NotifySet {
                    id: id.clone(),
                    enabled: true,
                };
                match ipc::send_request_checked(&config, node_wrap(args.node, inner)).await? {
                    RpcResponse::Ack => {
                        println!("Notifications enabled for session {id}.");
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response type".to_string())),
                }
            }
            NotifyCommand::Send(args) => {
                let inner = RpcRequest::NotifySend {
                    source: args.source,
                    title: args.title,
                    description: args.description,
                    body: args.body,
                    url: args.url,
                };
                match ipc::send_request_checked(&config, node_wrap(args.node, inner)).await? {
                    RpcResponse::Ack => {
                        println!("Notification sent.");
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response type".to_string())),
                }
            }
        },

        Commands::Restart(restart_args) => {
            let source_id = restart_args.id.clone();
            let inner = RpcRequest::Restart {
                id: source_id.clone(),
                force: restart_args.force,
            };
            match ipc::send_request_checked(&config, node_wrap(restart_args.node, inner)).await? {
                RpcResponse::Restart {
                    source_id,
                    session_id,
                } => {
                    println!("Session {source_id} restarted as {session_id}.");
                    Ok(())
                }
                _ => Err(AppError::Protocol("unexpected response type".to_string())),
            }
        }

        Commands::Stop(stop_args) => {
            let id =
                resolve_session_id(&config, stop_args.id.clone(), stop_args.node.as_ref()).await?;
            let inner = RpcRequest::Stop {
                id: id.clone(),
                grace_seconds: stop_args.grace,
            };
            match ipc::send_request_checked(&config, node_wrap(stop_args.node, inner)).await? {
                RpcResponse::Stop { stopped } if stopped => {
                    println!("Session {id} stopped. Check logs with `oly logs {id}`");
                    Ok(())
                }
                _ => Err(AppError::Protocol("unexpected response type".to_string())),
            }
        }

        Commands::Remove(remove_args) => {
            let id = resolve_session_id(&config, remove_args.id.clone(), remove_args.node.as_ref())
                .await?;
            let inner = RpcRequest::Remove {
                id: id.clone(),
                force: remove_args.force,
            };
            match ipc::send_request_checked(&config, node_wrap(remove_args.node, inner)).await? {
                RpcResponse::Remove { removed } if removed => {
                    println!("Session {id} deleted.");
                    Ok(())
                }
                _ => Err(AppError::Protocol("unexpected response type".to_string())),
            }
        }

        Commands::Attach(attach_args) => {
            let id = resolve_session_id(&config, attach_args.id.clone(), attach_args.node.as_ref())
                .await?;
            let role = Some(attach_args.role());
            if attach_args.node.is_some() {
                client::run_attach_node(&config, &id, attach_args.node, role).await
            } else {
                client::run_attach(&config, &id, role).await
            }
        }

        Commands::Logs(logs_args) => {
            let id =
                resolve_session_id(&config, logs_args.id.clone(), logs_args.node.as_ref()).await?;
            let node = logs_args.node.clone();

            // --json only has a defined shape for window reads and
            // wait-only results; anywhere else it would be silently ignored.
            if logs_args.json && logs_args.from.is_none() && !logs_args.wait_only() {
                return Err(AppError::Protocol(
                    "--json requires --from or a wait-only condition \
                     (--after/--exit/--idle-ms/--pattern with nothing to read selected)"
                        .to_string(),
                ));
            }

            // Wait-only mode: gate conditions with no read selected — print
            // the condition result and the new cursor (exit 0 met, 2
            // timeout, 1 error).
            if logs_args.wait_only() {
                return client::run_wait(
                    &config,
                    &id,
                    client::WaitCondition {
                        after: logs_args.after.unwrap_or(0),
                        exit: logs_args.exit,
                        idle_ms: logs_args.idle_ms,
                        pattern: logs_args.pattern.clone(),
                        // 0 = wait forever; default 30s.
                        timeout_secs: logs_args.timeout.map(|ms| ms.div_ceil(1000)).unwrap_or(30),
                    },
                    logs_args.json,
                    node,
                )
                .await;
            }

            // Gate conditions + something to read: block first, then read
            // (`--after N --screen`, `--exit --tail 40`, `--from N --after N`,
            // ...). A gate timeout exits 2 without reading.
            if logs_args.wait_mode() {
                eprintln!("Waiting for session {id}…");
                client::wait_for_condition(
                    &config,
                    &id,
                    &client::WaitCondition {
                        after: logs_args.after.unwrap_or(0),
                        exit: logs_args.exit,
                        idle_ms: logs_args.idle_ms,
                        pattern: logs_args.pattern.clone(),
                        timeout_secs: logs_args.timeout.map(|ms| ms.div_ceil(1000)).unwrap_or(30),
                    },
                    node.as_deref(),
                )
                .await?;
            }

            if logs_args.screen {
                // `oly logs --screen`: the visible screen as text.
                client::run_screen(
                    &config,
                    &id,
                    logs_args.cols,
                    logs_args.keep_color,
                    logs_args.from_file,
                    node,
                )
                .await
            } else if let Some(from) = logs_args.from {
                // `oly logs --from`: raw window of the canonical filtered
                // stream starting at a cursor (agent reads).
                client::run_history(
                    &config,
                    &id,
                    from,
                    logs_args.limit.unwrap_or(131072),
                    logs_args.json,
                    node,
                )
                .await
            } else {
                client::run_logs(
                    &config,
                    &id,
                    logs_args.tail,
                    logs_args.keep_color,
                    logs_args.from_file,
                    logs_args.no_truncate,
                    logs_args.raw,
                    logs_args.cols,
                    node,
                    logs_args.wait_for_prompt,
                    // --wait-for-prompt default: 5 minutes.
                    logs_args.timeout.unwrap_or(300_000),
                )
                .await
            }
        }

        Commands::Send(send_args) => {
            let id =
                resolve_session_id(&config, send_args.id.clone(), send_args.node.as_ref()).await?;
            client::run_send(&config, &id, send_args.node, send_args.chunks).await
        }

        Commands::Observe(args) => {
            let id = resolve_session_id(&config, args.id.clone(), args.node.as_ref()).await?;
            client::run_observe(&config, &id, args.node, args.json).await
        }
        Commands::Doctor(args) => client::run_doctor(&config, args.id, args.node).await,

        // ── API key management (primary side) ────────────────────────────────
        Commands::ApiKey(api_key_args) => match api_key_args.command {
            ApiKeyCommand::Add(args) => {
                match ipc::send_request_checked(
                    &config,
                    RpcRequest::ApiKeyAdd {
                        name: args.name,
                        scopes: args.scopes.clone(),
                    },
                )
                .await?
                {
                    RpcResponse::ApiKeyAdd { plaintext_key } => {
                        println!(
                            "API key registered. Key (store it securely — printed only once):"
                        );
                        println!("{plaintext_key}");
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
            ApiKeyCommand::List => {
                match ipc::send_request_checked(&config, RpcRequest::ApiKeyList).await? {
                    RpcResponse::ApiKeyList { keys } => {
                        if keys.is_empty() {
                            println!("No API keys registered.");
                        } else {
                            println!("{:<24} {:<20} CREATED", "NAME", "SCOPES");
                            for k in keys {
                                let created = k
                                    .created_at
                                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                                    .unwrap_or_else(|| "unknown".to_string());
                                println!("{:<24} {:<20} {}", k.name, k.scopes, created);
                            }
                        }
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
            ApiKeyCommand::Remove(args) => {
                match ipc::send_request_checked(
                    &config,
                    RpcRequest::ApiKeyRemove {
                        name: args.name.clone(),
                    },
                )
                .await?
                {
                    RpcResponse::ApiKeyRemove { removed } => {
                        if removed {
                            println!("API key \"{}\" removed.", args.name);
                        } else {
                            eprintln!("API key \"{}\" not found.", args.name);
                        }
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
        },

        // ── Node listing (primary side) ──────────────────────────────────────
        Commands::Node(node_args) => match node_args.command {
            NodeCommand::List => {
                match ipc::send_request_checked(&config, RpcRequest::NodeList).await? {
                    RpcResponse::NodeList { nodes } => {
                        if nodes.is_empty() {
                            println!("No secondary nodes connected.");
                        } else {
                            println!("NAME");
                            for n in nodes {
                                println!("{}", n);
                            }
                        }
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
        },

        // ── Join management (secondary side) ─────────────────────────────────
        Commands::Join(join_args) => match join_args.command {
            JoinCommand::Start(args) => {
                client::run_join(&config, args.url, args.name, args.key).await
            }
            JoinCommand::Stop(args) => client::run_join_stop(&config, args.name).await,
            JoinCommand::List(args) => {
                match ipc::send_request_checked(
                    &config,
                    RpcRequest::JoinList {
                        primary: args.primary,
                    },
                )
                .await?
                {
                    RpcResponse::JoinList { joins } => {
                        if joins.is_empty() {
                            println!("No active joins.");
                        } else if args.primary {
                            for j in joins {
                                println!("{:<24}", j.name);
                            }
                        } else {
                            println!("{:<24} {:<12} PRIMARY URL", "NAME", "STATUS");
                            for j in joins {
                                let status = if j.connected { "connected" } else { "saved" };
                                println!("{:<24} {:<12} {}", j.name, status, j.primary_url);
                            }
                        }
                        Ok(())
                    }
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_start_cwd;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oly-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn resolves_relative_cwd_against_current_dir() {
        let base = unique_temp_dir("base");
        let child = base.join("child");
        fs::create_dir_all(&child).unwrap();

        let resolved = resolve_start_cwd(&base, Some("child".to_string())).unwrap();

        assert_eq!(resolved, child.to_string_lossy());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn rejects_missing_cwd() {
        let base = unique_temp_dir("missing-base");
        fs::create_dir_all(&base).unwrap();
        let missing = base.join("missing");

        let err = resolve_start_cwd(&base, Some("missing".to_string())).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!(
                "protocol error: working directory does not exist: {}",
                missing.display()
            )
        );
        fs::remove_dir_all(base).unwrap();
    }
}
