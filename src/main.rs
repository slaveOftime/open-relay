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
use protocol::{RpcRequest, RpcResponse};

use crate::config::AppConfig;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    };
    std::process::exit(code);
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

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let config = config::AppConfig::load()?;

    match cli.command {
        Commands::Daemon(args) => match args.command {
            DaemonCommand::Start(start_args) => {
                let http_port = start_args.port.unwrap_or(config.http_port.clone());
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
            DaemonCommand::Stop => daemon::stop(config).await,
        },

        Commands::List(list_args) => {
            let node = list_args.node.clone();
            client::run_list(&config, list_args, node).await
        }

        Commands::Start(start_args) => {
            let mut iter = start_args.cmd_and_args.into_iter();
            let cmd = iter.next().unwrap(); // guaranteed by num_args = 1..
            let args: Vec<String> = iter.collect();
            let cwd = std::env::current_dir()
                .ok()
                .map(|path| path.display().to_string());
            let (rows, cols) = if start_args.detach {
                (None, None)
            } else {
                let (cols, rows) = terminal::size().unwrap_or((80, 24));
                (Some(rows), Some(cols))
            };
            let inner = RpcRequest::Start {
                title: start_args.title,
                cmd,
                args,
                cwd,
                rows,
                cols,
                disable_notifications: start_args.disable_notifications,
            };
            let request = node_wrap(start_args.node, inner);
            match ipc::send_request(&config, request).await? {
                RpcResponse::Start { session_id } => {
                    if start_args.detach {
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
            let id = stop_args.id.clone();
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
            if attach_args.node.is_some() {
                // Remote attach: proxy the snapshot/poll/input/resize RPCs.
                client::run_attach_node(&config, &attach_args.id, attach_args.node).await
            } else {
                client::run_attach(&config, &attach_args.id).await
            }
        }

        Commands::Logs(logs_args) => {
            let node = logs_args.node.clone();
            client::run_logs(
                &config,
                &logs_args.id,
                logs_args.tail,
                logs_args.keep_color,
                logs_args.no_truncate,
                node,
                logs_args.wait_for_prompt,
                logs_args.timeout,
            )
            .await
        }

        Commands::Input(input_args) => {
            let node = input_args.node.clone();
            client::run_input(&config, input_args, node).await
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
            JoinCommand::List => match ipc::send_request(&config, RpcRequest::JoinList).await? {
                RpcResponse::JoinList { joins } => {
                    if joins.is_empty() {
                        println!("No active joins.");
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
            },
        },
    }
}
