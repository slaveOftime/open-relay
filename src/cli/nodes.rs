use crate::node::config::{NodeRole, NodeStatus};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct NodeArgs {
    #[command(subcommand)]
    pub command: NodeCommand,
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// Create and enable a peer. Primary peers require --url and --as.
    Add(NodeAddArgs),
    /// List all configured peers, including disconnected and disabled peers.
    #[command(name = "ls")]
    List(NodeListArgs),
    /// Show peer configuration, live state, and retry diagnostics.
    Info(NodeInfoArgs),
    /// Change a key or endpoint explicitly; closes the old connection.
    Update(NodeUpdateArgs),
    /// Permit connections again, keeping the saved configuration.
    Enable(NodeNameArgs),
    /// Close the connection and block reconnects, keeping configuration and trust.
    Disable(NodeNameArgs),
    /// Delete configuration and authorization (not sessions).
    #[command(name = "rm")]
    Remove(NodeNameArgs),
}

#[derive(Debug, Args)]
pub struct NodeAddArgs {
    /// Local name for the remote peer.
    pub name: String,
    /// Remote peer's role: a primary controls our sessions; we control a secondary's.
    #[arg(long, value_enum)]
    pub role: NodeRole,
    /// Independently verified remote public identity: ed25519 <base64>.
    #[arg(long)]
    pub key: String,
    /// HTTP(S) base URL to dial (primary peers only).
    #[arg(long)]
    pub url: Option<String>,
    /// Our name as authorized on that primary (primary peers only).
    #[arg(long = "as")]
    pub as_name: Option<String>,
}

#[derive(Debug, Args)]
pub struct NodeListArgs {
    #[arg(long, value_enum)]
    pub role: Option<NodeRole>,
    #[arg(long, value_enum)]
    pub status: Option<NodeStatus>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct NodeInfoArgs {
    pub name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct NodeNameArgs {
    pub name: String,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("changes").required(true).multiple(true).args(["key", "url", "as_name"])))]
pub struct NodeUpdateArgs {
    pub name: String,
    #[arg(long)]
    pub key: Option<String>,
    #[arg(long)]
    pub url: Option<String>,
    #[arg(long = "as")]
    pub as_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;
    #[test]
    fn peer_roles_names_and_filters_are_explicit() {
        let parsed = Cli::try_parse_from([
            "oly",
            "node",
            "add",
            "home",
            "--role",
            "primary",
            "--url",
            "http://host",
            "--as",
            "laptop",
            "--key",
            "key",
        ])
        .unwrap();
        let Commands::Node(NodeArgs {
            command: NodeCommand::Add(args),
        }) = parsed.command
        else {
            panic!()
        };
        assert_eq!(args.name, "home");
        assert_eq!(args.as_name.as_deref(), Some("laptop"));
        let parsed = Cli::try_parse_from([
            "oly",
            "node",
            "ls",
            "--role",
            "secondary",
            "--status",
            "connected",
            "--json",
        ])
        .unwrap();
        let Commands::Node(NodeArgs {
            command: NodeCommand::List(args),
        }) = parsed.command
        else {
            panic!()
        };
        assert_eq!(args.role, Some(NodeRole::Secondary));
        assert_eq!(args.status, Some(NodeStatus::Connected));
        assert!(args.json);
    }
    #[test]
    fn removed_commands_and_empty_updates_fail() {
        for args in [
            vec!["oly", "join", "ls"],
            vec!["oly", "node", "accept"],
            vec!["oly", "node", "ls", "--primary"],
            vec!["oly", "node", "update", "home"],
            vec!["oly", "node", "add", "home", "--key", "key"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}
