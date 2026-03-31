use clap::{Args, Parser, Subcommand, ValueEnum};

fn parse_timeout_ms(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("timeout cannot be empty".to_string());
    }

    if trimmed == "0" {
        return Ok(0);
    }

    let suffix_start = trimmed
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(trimmed.len());
    let (amount, unit) = trimmed.split_at(suffix_start);
    if amount.is_empty() {
        return Err(format!(
            "invalid timeout '{value}'; use a number optionally followed by ms, s, m, or h"
        ));
    }

    let amount = amount.parse::<u64>().map_err(|_| {
        format!("invalid timeout '{value}'; the numeric portion must be an unsigned integer")
    })?;
    let unit = unit.to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" | "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => {
            return Err(format!(
                "invalid timeout unit in '{value}'; supported units are ms, s, m, and h"
            ));
        }
    };

    amount
        .checked_mul(multiplier)
        .ok_or_else(|| format!("timeout is too large: {value}"))
}

#[derive(Debug, Parser)]
#[command(
    name = "oly",
    version,
    about = "A tool for managing terminal sessions on the Open Relay daemon."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start or stop the daemon process.
    Daemon(DaemonArgs),
    /// Create a session and run a command. Example: `oly start --detach --title "my fun demo" copilot`.
    Start(StartArgs),
    /// Enable or disable notifications for a running session.
    Notify(NotifyArgs),
    /// Display the oly skill markdown.
    Skill(SkillArgs),
    /// List sessions. Order is most recently created last.
    #[command(name = "ls")]
    List(ListArgs),
    /// Stop a session by ID.
    Stop(StopArgs),
    /// Attach to a running session.
    Attach(AttachArgs),
    /// Show session logs.
    Logs(LogsArgs),
    /// Send text or keys to a session. Example: `oly send <id> "hello" key:enter`.
    Send(SendArgs),
    /// Manage API keys on this (primary) daemon.
    ApiKey(ApiKeyArgs),
    /// Manage this daemon's outbound connections to a primary daemon.
    Join(JoinArgs),
    /// List secondary nodes currently connected to this (primary) daemon.
    Node(NodeArgs),
}

#[derive(Debug, Args)]
pub struct SkillArgs {
    /// Skill about how to create oly app.
    #[arg(long)]
    pub apps: bool,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub command: DaemonCommand,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Launch the daemon. Runs in the foreground unless `--detach` is given.
    Start(DaemonStartArgs),
    /// Gracefully shut down the running daemon.
    Stop(DaemonStopArgs),
    /// Status of the running daemon.
    Status,
}

#[derive(Debug, Args)]
pub struct DaemonStartArgs {
    /// Run the daemon in the background, detached from this terminal.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Override default HTTP port.
    #[arg(long, short = 'p')]
    pub port: Option<u16>,
    /// Disable HTTP authentication. You will be asked to confirm the security risk.
    #[arg(long)]
    pub no_auth: bool,
    /// Disable the HTTP API and web frontend entirely.
    #[arg(long)]
    pub no_http: bool,
    #[arg(long, hide = true)]
    pub foreground_internal: bool,
    /// Argon2 PHC hash passed from the parent process to the detached child; never set manually.
    #[arg(long, hide = true)]
    pub auth_hash_internal: Option<String>,
}

#[derive(Debug, Args)]
pub struct DaemonStopArgs {
    /// Seconds to wait for sessions to exit cleanly before forcing termination.
    #[arg(long, default_value_t = 15)]
    pub grace: u64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ListStatus {
    /// Created but not yet started.
    Created,
    /// Process is running.
    Running,
    /// Shutting down.
    Stopping,
    /// Exited cleanly.
    Stopped,
    /// Terminated immediately via hard stop.
    Killed,
    /// Exited with an error.
    Failed,
    /// Status could not be determined.
    Unknown,
}

impl ListStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Killed => "killed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Filter by title or ID substring (case-insensitive).
    #[arg(long)]
    pub search: Option<String>,
    /// Only show sessions containing these tags (repeatable).
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Print machine-readable JSON instead of the default table.
    #[arg(long)]
    pub json: bool,
    /// Only show sessions with these statuses (repeatable).
    #[arg(long = "status", short = 's', value_enum)]
    pub status: Vec<ListStatus>,
    /// Created at or after (RFC3339, e.g. 2026-03-04T15:04:05Z).
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,
    /// Created at or before (RFC3339, e.g. 2026-03-04T15:04:05Z).
    #[arg(long, value_name = "RFC3339")]
    pub until: Option<String>,
    /// Maximum number of sessions to return.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Title for the session.
    #[arg(long, short = 't')]
    pub title: Option<String>,
    /// Tag for the session. Repeat to add multiple tags.
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Start the session detached (in the background).
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// Disable notifications for this session.
    #[arg(long)]
    pub disable_notifications: bool,
    /// Working directory for the command. Relative paths are resolved from the caller's current directory.
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<String>,
    /// Command and arguments to run. Passed through as-is.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 1..,
        value_name = "CMD [ARGS]...",
    )]
    pub cmd_and_args: Vec<String>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct NotifyArgs {
    #[command(subcommand)]
    pub command: NotifyCommand,
}

#[derive(Debug, Subcommand)]
pub enum NotifyCommand {
    /// Disable notifications for a running session.
    Disable(NotifyToggleArgs),
    /// Enable notifications for a running session.
    Enable(NotifyToggleArgs),
    /// Send a notification, optionally associated with a session.
    Send(NotifySendArgs),
}

#[derive(Debug, Args)]
pub struct NotifyToggleArgs {
    /// Session ID to update. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct NotifySendArgs {
    /// Source session ID to associate with the notification.
    pub source: Option<String>,
    /// Notification title.
    #[arg(long, short = 't')]
    pub title: String,
    /// Optional short notification description.
    #[arg(long, short = 'd')]
    pub description: Option<String>,
    /// Optional notification body text.
    #[arg(long, short = 'b')]
    pub body: Option<String>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
    /// Url to open when the notification is clicked. Absolute or relative to oly http server. If omitted, defaults to the attach URL of the source session (if any).
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    /// Session ID to stop. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Seconds to wait for clean exit before forcibly killing.
    #[arg(long, default_value_t = 5)]
    pub grace: u64,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    /// Session ID to attach to. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Session ID to show logs for. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Number of recent lines to display. By default it uses the current terminal height - 1, or 40 if it cannot be determined.
    #[arg(long)]
    pub tail: Option<usize>,
    /// Keep ANSI color codes in output.
    #[arg(long = "keep-color")]
    pub keep_color: bool,
    /// Do not truncate columns.
    #[arg(long = "no-truncate")]
    pub no_truncate: bool,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
    /// Block until the session needs input (or exits), then print logs.
    #[arg(long = "wait-for-prompt", short = 'w')]
    pub wait_for_prompt: bool,
    /// Timeout for --wait-for-prompt. Accepts plain milliseconds or units like 10s, 5m, or 1h.
    #[arg(
        long,
        default_value = "5m",
        value_name = "DURATION",
        value_parser = parse_timeout_ms
    )]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    /// Session ID to send input to. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Input chunks, processed left to right. Plain text is sent literally.
    /// Prefix with key: for special keys, e.g. key:enter, key:ctrl+c, key:up.
    /// Use oly-clipboard to send clipboard text or uploaded clipboard files.
    /// Prefix with oly-file:<path> to upload a local file and send the saved session path.
    /// Supported keys: enter, tab, esc, backspace, up/down/left/right, home/end,
    /// pgup/pgdn, del/ins, ctrl+<char>, alt+<char|key>, shift+tab, hex:<bytes>.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        value_name = "CHUNK"
    )]
    pub chunks: Vec<String>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

// ---------------------------------------------------------------------------
// API key management (primary side)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct ApiKeyArgs {
    #[command(subcommand)]
    pub command: ApiKeyCommand,
}

#[derive(Debug, Subcommand)]
pub enum ApiKeyCommand {
    /// Generate a new API key and print it once. Keys are independent of node names.
    Add(ApiKeyAddArgs),
    /// List all registered API keys (names only; raw values are never stored).
    #[command(name = "ls")]
    List,
    /// Remove an API key by name. Nodes using it will be disconnected.
    Remove(ApiKeyRemoveArgs),
}

#[derive(Debug, Args)]
pub struct ApiKeyAddArgs {
    /// Friendly label for this key (must be unique).
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ApiKeyRemoveArgs {
    /// Label of the key to remove.
    pub name: String,
}

// ---------------------------------------------------------------------------
// Join management (secondary side)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct JoinArgs {
    #[command(subcommand)]
    pub command: JoinCommand,
}

#[derive(Debug, Subcommand)]
pub enum JoinCommand {
    /// Connect this daemon to a primary as a named secondary node. Config is persisted across restarts.
    Start(JoinStartArgs),
    /// Disconnect from a primary and delete the saved join config.
    Stop(JoinStopArgs),
    /// List all active join configs on this (secondary) daemon.
    #[command(name = "ls")]
    List(JoinListArgs),
}

// ---------------------------------------------------------------------------
// Node listing (primary side)
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
pub struct NodeArgs {
    #[command(subcommand)]
    pub command: NodeCommand,
}

#[derive(Debug, Subcommand)]
pub enum NodeCommand {
    /// List all secondary nodes currently connected to this (primary) daemon.
    #[command(name = "ls")]
    List,
}

#[derive(Debug, Args)]
pub struct JoinStartArgs {
    /// Name this daemon will be known as on the primary (must be unique per primary).
    #[arg(long, short = 'n')]
    pub name: String,
    /// API key printed by `oly api-key add` on the primary.
    #[arg(long, short = 'k')]
    pub key: String,
    #[arg(help = "HTTP base URL of the primary daemon, e.g. http://primary-host:15443")]
    pub url: String,
}

#[derive(Debug, Args)]
pub struct JoinStopArgs {
    /// Name of the join config to stop and remove.
    #[arg(long, short = 'n')]
    pub name: String,
}

#[derive(Debug, Args)]
pub struct JoinListArgs {
    /// For list all the nodes joined to the current daemon (primary)
    #[arg(long, short = 'p')]
    pub primary: bool,
}

#[cfg(test)]
mod tests {
    use super::{Cli, Commands, NotifyCommand, parse_timeout_ms};
    use clap::Parser;

    #[test]
    fn parses_timeout_units_directly() {
        assert_eq!(parse_timeout_ms("600").unwrap(), 600);
        assert_eq!(parse_timeout_ms("250ms").unwrap(), 250);
        assert_eq!(parse_timeout_ms("10s").unwrap(), 10_000);
        assert_eq!(parse_timeout_ms("2m").unwrap(), 120_000);
        assert_eq!(parse_timeout_ms("1h").unwrap(), 3_600_000);
        assert_eq!(parse_timeout_ms("0").unwrap(), 0);
    }

    #[test]
    fn clap_parses_logs_timeout_duration() {
        let cli = Cli::try_parse_from(["oly", "logs", "session-1", "--timeout", "10s"]).unwrap();
        let Commands::Logs(args) = cli.command else {
            panic!("expected logs command");
        };
        assert_eq!(args.timeout, 10_000);
    }

    #[test]
    fn logs_timeout_defaults_to_thirty_seconds() {
        let cli = Cli::try_parse_from(["oly", "logs", "session-1"]).unwrap();
        let Commands::Logs(args) = cli.command else {
            panic!("expected logs command");
        };
        assert_eq!(args.timeout, 300_000);
    }

    #[test]
    fn rejects_unknown_timeout_units() {
        let err = Cli::try_parse_from(["oly", "logs", "session-1", "--timeout", "10d"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid timeout unit"));
    }

    #[test]
    fn notify_disable_parses_node_and_id() {
        let cli = Cli::try_parse_from([
            "oly",
            "notify",
            "disable",
            "session-1",
            "--node",
            "worker-a",
        ])
        .unwrap();
        let Commands::Notify(args) = cli.command else {
            panic!("expected notify command");
        };
        let NotifyCommand::Disable(args) = args.command else {
            panic!("expected notify disable subcommand");
        };
        assert_eq!(args.id.as_deref(), Some("session-1"));
        assert_eq!(args.node.as_deref(), Some("worker-a"));
    }

    #[test]
    fn notify_send_parses_optional_source_and_payload() {
        let cli = Cli::try_parse_from([
            "oly",
            "notify",
            "send",
            "session-1",
            "--title",
            "Deploy ready",
            "--description",
            "Build finished",
            "--body",
            "Review the deployment logs.",
            "--node",
            "worker-a",
        ])
        .unwrap();
        let Commands::Notify(args) = cli.command else {
            panic!("expected notify command");
        };
        let NotifyCommand::Send(args) = args.command else {
            panic!("expected notify send subcommand");
        };
        assert_eq!(args.source.as_deref(), Some("session-1"));
        assert_eq!(args.title, "Deploy ready");
        assert_eq!(args.description.as_deref(), Some("Build finished"));
        assert_eq!(args.body.as_deref(), Some("Review the deployment logs."));
        assert_eq!(args.node.as_deref(), Some("worker-a"));
    }

    #[test]
    fn start_parses_repeatable_tags() {
        let cli = Cli::try_parse_from([
            "oly",
            "start",
            "--title",
            "Deploy ready",
            "--tag",
            "prod",
            "--tag",
            "release",
            "copilot",
        ])
        .unwrap();
        let Commands::Start(args) = cli.command else {
            panic!("expected start command");
        };
        assert_eq!(args.title.as_deref(), Some("Deploy ready"));
        assert_eq!(args.tags, vec!["prod".to_string(), "release".to_string()]);
        assert_eq!(args.cmd_and_args, vec!["copilot".to_string()]);
    }

    #[test]
    fn list_parses_repeatable_tag_filters() {
        let cli = Cli::try_parse_from(["oly", "ls", "--tag", "prod", "--tag", "release"]).unwrap();
        let Commands::List(args) = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(args.tags, vec!["prod".to_string(), "release".to_string()]);
    }
}
