use crate::{
    cli::{NodeArgs, NodeCommand},
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    node::config::{NodeConfig, NodeSummary, NodeUpdate},
    protocol::{RpcRequest, RpcResponse},
};

pub async fn run_nodes(config: &AppConfig, args: NodeArgs) -> Result<()> {
    let (request, json, detailed) = match args.command {
        NodeCommand::Add(args) => {
            let mut node = NodeConfig {
                name: args.name,
                role: args.role,
                key: args.key,
                url: args.url,
                as_name: args.as_name,
                enabled: true,
                revision: String::new(),
            };
            node.validate()?;
            (RpcRequest::NodeAdd { config: node }, false, false)
        }
        NodeCommand::Update(args) => (
            RpcRequest::NodeUpdate {
                name: args.name,
                update: NodeUpdate {
                    key: args.key,
                    url: args.url,
                    as_name: args.as_name,
                },
            },
            false,
            false,
        ),
        NodeCommand::Enable(args) => (
            RpcRequest::NodeSetEnabled {
                name: args.name,
                enabled: true,
            },
            false,
            false,
        ),
        NodeCommand::Disable(args) => (
            RpcRequest::NodeSetEnabled {
                name: args.name,
                enabled: false,
            },
            false,
            false,
        ),
        NodeCommand::Remove(args) => {
            let name = args.name.clone();
            match ipc::send_request_checked(config, RpcRequest::NodeRemove { name: args.name })
                .await?
            {
                RpcResponse::Ack => {
                    println!("Removed node '{name}'.");
                    return Ok(());
                }
                _ => {
                    return Err(AppError::Protocol(
                        "unexpected response to node removal".into(),
                    ));
                }
            }
        }
        NodeCommand::List(args) => (
            RpcRequest::NodeList {
                role: args.role,
                status: args.status,
            },
            args.json,
            false,
        ),
        NodeCommand::Info(args) => (RpcRequest::NodeInfo { name: args.name }, args.json, true),
    };
    match ipc::send_request_checked(config, request).await? {
        RpcResponse::NodeList { items, total } => {
            if json {
                println!("{}", serde_json::json!({"items": items, "total": total}));
            } else if items.is_empty() {
                println!("No configured nodes match.");
            } else {
                print_header();
                for node in &items {
                    print_row(node);
                }
            }
        }
        RpcResponse::NodeInfo { node } => {
            if json {
                println!("{}", serde_json::to_string(&node)?);
            } else if detailed {
                println!(
                    "NAME:          {}\nROLE:          {}\nSTATUS:        {}\nENABLED:       {}\nKEY:           {}\nURL:           {}\nAS:            {}\nLAST ERROR:    {}\nNEXT RETRY:    {}",
                    node.config.name,
                    node.config.role.as_str(),
                    node.status.as_str(),
                    node.config.enabled,
                    node.config.key,
                    node.config.url.as_deref().unwrap_or("-"),
                    node.config.as_name.as_deref().unwrap_or("-"),
                    node.last_error.as_deref().unwrap_or("-"),
                    node.next_retry_at
                        .map(|t| t.to_rfc3339())
                        .as_deref()
                        .unwrap_or("-")
                );
            } else {
                print_header();
                print_row(&node);
            }
        }
        _ => {
            return Err(AppError::Protocol(
                "unexpected response to node command".into(),
            ));
        }
    }
    Ok(())
}

fn print_header() {
    println!(
        "{:<24} {:<10} {:<12} {:<36} AS",
        "NAME", "ROLE", "STATUS", "URL"
    );
}
fn print_row(node: &NodeSummary) {
    println!(
        "{:<24} {:<10} {:<12} {:<36} {}",
        node.config.name,
        node.config.role.as_str(),
        node.status.as_str(),
        node.config.url.as_deref().unwrap_or("-"),
        node.config.as_name.as_deref().unwrap_or("-")
    );
}
