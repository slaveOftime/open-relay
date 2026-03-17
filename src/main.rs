mod cli;
mod client;
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
mod utils;

use clap::Parser;
use cli::{ApiKeyCommand, Cli, Commands, DaemonCommand, JoinCommand, NodeCommand};
use crossterm::terminal;
use error::{AppError, Result};
use protocol::{ListQuery, ListSortField, RpcRequest, RpcResponse, SortOrder};

use crate::config::AppConfig;

use libmimalloc_sys::{mi_option_set_default, mi_option_set_enabled_default};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

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

    match ipc::send_request(config, request).await? {
        RpcResponse::List { sessions, .. } => {
            if let Some(session) = sessions.into_iter().next() {
                Ok(session.id)
            } else {
                Err(AppError::Protocol(
                    "no sessions found; start one with: oly start --detach <cmd>".to_string(),
                ))
            }
        }
        RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = config::AppConfig::load()?;

    match cli.command {
        Commands::Daemon(args) => match args.command {
            DaemonCommand::Start(start_args) => {
                let http_port = start_args.port.unwrap_or(config.http_port);
                daemon::start(
                    AppConfig {
                        http_port,
                        ..config
                    },
                    start_args.detach,
                    start_args.foreground_internal,
                    start_args.no_auth,
                    start_args.no_http,
                    start_args.auth_hash_internal,
                )
                .await
            }
            DaemonCommand::Stop(stop_args) => daemon::stop(config, stop_args.grace).await,
        },

        Commands::List(list_args) => {
            let node = list_args.node.clone();
            client::run_list(&config, list_args, node).await
        }

        Commands::Start(start_args) => {
            let cli::StartArgs {
                title,
                detach,
                disable_notifications,
                cwd,
                cmd_and_args,
                node,
            } = start_args;

            let mut iter = cmd_and_args.into_iter();
            let cmd = iter.next().unwrap(); // guaranteed by num_args = 1..
            let args: Vec<String> = iter.collect();
            let cwd = cwd.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.display().to_string())
            });
            let (rows, cols) = if detach {
                (None, None)
            } else {
                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                (Some(rows), Some(cols))
            };
            let inner = RpcRequest::Start {
                title,
                cmd,
                args,
                cwd,
                rows,
                cols,
                disable_notifications,
            };
            let request = node_wrap(node, inner);
            match ipc::send_request(&config, request).await? {
                RpcResponse::Start { session_id } => {
                    if detach {
                        println!("{session_id}");
                        return Ok(());
                    }
                    if let Err(err) = client::run_attach(&config, &session_id).await {
                        eprintln!("");
                        eprintln!(
                            "warning: started session {session_id}, but failed to attach: {err}"
                        );
                    }
                    Ok(())
                }
                RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
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
            match ipc::send_request(&config, node_wrap(stop_args.node, inner)).await? {
                RpcResponse::Stop { stopped } if stopped => {
                    println!("Session {id} stopped. Check logs with `oly logs {id}`");
                    Ok(())
                }
                RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
                _ => Err(AppError::Protocol("unexpected response type".to_string())),
            }
        }

        Commands::Attach(attach_args) => {
            let id = resolve_session_id(&config, attach_args.id.clone(), attach_args.node.as_ref())
                .await?;
            if attach_args.node.is_some() {
                client::run_attach_node(&config, &id, attach_args.node).await
            } else {
                client::run_attach(&config, &id).await
            }
        }

        Commands::Logs(logs_args) => {
            let id =
                resolve_session_id(&config, logs_args.id.clone(), logs_args.node.as_ref()).await?;
            let node = logs_args.node.clone();
            client::run_logs(
                &config,
                &id,
                logs_args.tail,
                logs_args.keep_color,
                logs_args.no_truncate,
                node,
                logs_args.wait_for_prompt,
                logs_args.timeout,
            )
            .await
        }

        Commands::Send(send_args) => {
            let id =
                resolve_session_id(&config, send_args.id.clone(), send_args.node.as_ref()).await?;
            let node = send_args.node.clone();
            let send_args = cli::SendArgs {
                id: Some(id),
                ..send_args
            };
            client::run_send(&config, send_args, node).await
        }

        // ── API key management (primary side) ────────────────────────────────
        Commands::ApiKey(api_key_args) => match api_key_args.command {
            ApiKeyCommand::Add(args) => {
                match ipc::send_request(&config, RpcRequest::ApiKeyAdd { name: args.name }).await? {
                    RpcResponse::ApiKeyAdd { plaintext_key } => {
                        println!(
                            "API key registered. Key (store it securely — printed only once):"
                        );
                        println!("{plaintext_key}");
                        Ok(())
                    }
                    RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
            ApiKeyCommand::List => {
                match ipc::send_request(&config, RpcRequest::ApiKeyList).await? {
                    RpcResponse::ApiKeyList { keys } => {
                        if keys.is_empty() {
                            println!("No API keys registered.");
                        } else {
                            println!("{:<24} {}", "NAME", "CREATED");
                            for k in keys {
                                let created = k
                                    .created_at
                                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                                    .unwrap_or_else(|| "unknown".to_string());
                                println!("{:<24} {}", k.name, created);
                            }
                        }
                        Ok(())
                    }
                    RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
            ApiKeyCommand::Remove(args) => {
                match ipc::send_request(
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
                    RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
        },

        // ── Node listing (primary side) ──────────────────────────────────────
        Commands::Node(node_args) => match node_args.command {
            NodeCommand::List => match ipc::send_request(&config, RpcRequest::NodeList).await? {
                RpcResponse::NodeList { nodes } => {
                    if nodes.is_empty() {
                        println!("No secondary nodes connected.");
                    } else {
                        println!("{}", "NAME");
                        for n in nodes {
                            println!("{}", n);
                        }
                    }
                    Ok(())
                }
                RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
                _ => Err(AppError::Protocol("unexpected response".into())),
            },
        },

        // ── Join management (secondary side) ─────────────────────────────────
        Commands::Join(join_args) => match join_args.command {
            JoinCommand::Start(args) => {
                client::run_join(&config, args.url, args.name, args.key).await
            }
            JoinCommand::Stop(args) => client::run_join_stop(&config, args.name).await,
            JoinCommand::List(args) => {
                match ipc::send_request(
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
                            println!("{:<24} {:<12} {}", "NAME", "STATUS", "PRIMARY URL");
                            for j in joins {
                                let status = if j.connected { "connected" } else { "saved" };
                                println!("{:<24} {:<12} {}", j.name, status, j.primary_url);
                            }
                        }
                        Ok(())
                    }
                    RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
                    _ => Err(AppError::Protocol("unexpected response".into())),
                }
            }
        },
    }
}
