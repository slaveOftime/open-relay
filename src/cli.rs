use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Timeout in milliseconds for --wait-for-prompt (0 = infinite).
    #[arg(long, default_value_t = 30000, value_name = "MS")]
    pub timeout: u64,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    /// Session ID to send input to. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Input chunks, processed left to right. Plain text is sent literally.
    /// Prefix with key: for special keys, e.g. key:enter, key:ctrl+c, key:up.
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
