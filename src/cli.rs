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
    /// Override a session's title, tags, and notification setting.
    Update(UpdateArgs),
    /// Enable or disable notifications for a running session.
    Notify(NotifyArgs),
    /// Display the oly skill markdown.
    Skill(SkillArgs),
    /// List sessions. Order is most recently created last.
    #[command(name = "ls")]
    List(ListArgs),
    /// Start a new session from an existing session's persisted launch metadata.
    Restart(RestartArgs),
    /// Machine-readable session cursor (liveness + canonical stream offset).
    Observe(SessionRefArgs),
    /// Verify journal integrity (sealed-part manifests) for one or all sessions.
    Doctor(DoctorArgs),
    /// Stop a session by ID.
    Stop(StopArgs),
    /// Delete a session and its files. Stopped sessions are removed directly; use --force to also remove a running one.
    #[command(name = "rm", visible_alias = "delete")]
    Remove(RemoveArgs),
    /// Attach to a running session.
    Attach(AttachArgs),
    /// Show session output: rendered log tail (default), the visible screen
    /// (`--screen`), a raw byte window from a cursor (`--from`), or block
    /// until a condition is met (`--after`/`--exit`/`--idle-ms`/`--pattern`).
    /// Uses live screen state when running, otherwise replays the journal;
    /// `--from-file` forces the journal replay.
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
    /// Override default HTTP bind address.
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,
    /// Override default HTTP port.
    #[arg(long, short = 'p')]
    pub port: Option<u16>,
    /// Override the configured local notification hook for this daemon run.
    #[arg(long, value_name = "PATH")]
    pub notification_hook: Option<String>,
    /// Route outbound web push delivery through an HTTP(S) or SOCKS proxy.
    #[arg(long, value_name = "URL")]
    pub web_push_proxy: Option<String>,
    /// Disable HTTP authentication. You will be asked to confirm the security risk.
    #[arg(long)]
    pub no_auth: bool,
    /// Disable HTTP authentication without asking for confirmation. Implies --no-auth.
    #[arg(long)]
    pub no_auth_without_ask: bool,
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
    #[arg(long, conflicts_with = "follow")]
    pub json: bool,
    /// Follow sessions in an interactive realtime terminal UI.
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// Only show sessions with these statuses (repeatable).
    #[arg(long = "status", short = 's', value_enum)]
    pub status: Vec<ListStatus>,
    /// Created at or after (RFC3339, e.g. 2026-03-04T15:04:05Z).
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,
    /// Created at or before (RFC3339, e.g. 2026-03-04T15:04:05Z).
    #[arg(long, value_name = "RFC3339")]
    pub until: Option<String>,
    /// Maximum number of sessions to return (defaults to 100 with --follow, otherwise 10).
    #[arg(
        long,
        default_value_t = 10,
        default_value_if("follow", "true", Some("100"))
    )]
    pub limit: usize,
    /// Target a secondary node by name. Repeat to monitor multiple nodes.
    #[arg(long, short = 'n', value_name = "NODE")]
    pub node: Vec<String>,
    /// Include sessions from the current (or primary) daemon.
    #[arg(long)]
    pub node_local: bool,
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
pub struct UpdateArgs {
    /// Session ID to update.
    pub id: String,
    /// Override the session title. Pass an empty string to clear it; omit to leave unchanged.
    #[arg(long, short = 't')]
    pub title: Option<String>,
    /// Override the session tags. Repeat to add multiple tags; pass an empty string to clear all tags; omit to leave unchanged.
    #[arg(long = "tag")]
    pub tags: Option<Vec<String>>,
    /// Override notifications for a running session; omit to leave unchanged.
    #[arg(long, value_enum, value_name = "enabled|disabled")]
    pub notifications: Option<NotificationSetting>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NotificationSetting {
    Enabled,
    Disabled,
}

impl NotificationSetting {
    pub fn enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
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
pub struct RestartArgs {
    /// Session ID whose launch metadata should be reused.
    pub id: String,
    /// Kill a running or stopping source session before starting its replacement.
    #[arg(long)]
    pub force: bool,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Session ID to delete. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Delete even if the session is still running (it is killed first).
    #[arg(long)]
    pub force: bool,
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
    /// Attach view-only: never drives input or geometry, never takes control.
    #[arg(long, conflicts_with = "takeover")]
    pub observer: bool,
    /// Take the control lease from the current controller (it becomes an
    /// observer). This is the default for interactive attach: the most
    /// recently activated client always drives input and geometry.
    #[arg(long)]
    pub takeover: bool,
}

impl AttachArgs {
    /// Wire role token for the attach subscription. Interactive attach
    /// defaults to takeover: a client that becomes active takes the control
    /// lease (and with it geometry authority) from whoever held it, matching
    /// tmux-style last-attach-wins semantics. `--observer` opts out.
    pub fn role(&self) -> &'static str {
        if self.observer {
            "observer"
        } else {
            "takeover"
        }
    }
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    /// Session ID to show logs for. If omitted, uses the most recently created session.
    pub id: Option<String>,

    // ── What to read (at most one; default: the rendered log tail) ──────
    /// Print the visible screen instead of the log tail.
    #[arg(long, conflicts_with_all = ["from", "raw"])]
    pub screen: bool,
    /// Read a raw byte window of the canonical filtered stream starting at
    /// OFFSET (pair with `oly observe` for the cursor; page with the
    /// returned `next` offset).
    #[arg(long, value_name = "OFFSET", conflicts_with = "raw")]
    pub from: Option<u64>,
    /// Export the whole raw output byte stream (journal-derived, unfiltered
    /// by rendering). May contain terminal control sequences — meant for
    /// pipes and files; a warning is printed when stdout is a terminal.
    /// Local sessions only.
    #[arg(
        long = "raw",
        conflicts_with_all = [
            "tail", "keep_color", "from_file", "no_truncate", "cols",
            "wait_for_prompt", "after", "exit", "idle_ms", "pattern", "json"
        ]
    )]
    pub raw: bool,

    // ── When to read it (optional gate; with nothing to read selected,
    //    prints the condition result and the new cursor instead) ─────────
    /// Block until the session needs input (or exits) before reading.
    #[arg(
        long = "wait-for-prompt",
        short = 'w',
        conflicts_with_all = ["after", "exit", "idle_ms", "pattern"]
    )]
    pub wait_for_prompt: bool,
    /// Block until output appears after this filtered-stream offset (or
    /// another gate condition below is met) before reading.
    #[arg(long, value_name = "OFFSET")]
    pub after: Option<u64>,
    /// Gate condition: the session exited.
    #[arg(long)]
    pub exit: bool,
    /// Gate condition: no output for this many milliseconds (heuristic:
    /// likely idle or waiting for input, never proof).
    #[arg(long)]
    pub idle_ms: Option<u64>,
    /// Gate condition: regex matches output produced after --after.
    #[arg(long)]
    pub pattern: Option<String>,
    /// Timeout for --wait-for-prompt and the gate conditions. Accepts plain
    /// milliseconds or units like 10s, 5m, or 1h; 0 waits forever.
    /// Defaults: 5m with --wait-for-prompt, 30s for gate conditions.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout_ms)]
    pub timeout: Option<u64>,

    // ── How to format it ─────────────────────────────────────────────────
    /// Number of recent lines to display (rendered tail). Defaults to the
    /// terminal height - 1, or 40 if it cannot be determined.
    #[arg(long, conflicts_with_all = ["screen", "from", "raw"])]
    pub tail: Option<usize>,
    /// Keep ANSI color codes (rendered modes: log tail, --screen).
    #[arg(long = "keep-color", conflicts_with_all = ["from", "raw"])]
    pub keep_color: bool,
    /// Render width in columns (default: local terminal width, fallback 80).
    #[arg(long, conflicts_with_all = ["from", "raw", "no_truncate"])]
    pub cols: Option<u32>,
    /// Do not truncate columns (rendered tail).
    #[arg(long = "no-truncate", conflicts_with_all = ["screen", "from", "raw"])]
    pub no_truncate: bool,
    /// Force rendering from the persisted journal instead of live screen
    /// state (rendered modes: log tail, --screen).
    #[arg(long = "from-file", conflicts_with_all = ["from", "raw"])]
    pub from_file: bool,
    /// Maximum bytes for a --from window (bounded; larger spans need
    /// multiple calls).
    #[arg(long, requires = "from")]
    pub limit: Option<u32>,
    /// Emit machine-readable JSON (with --from, or with a wait condition
    /// and no read selected).
    #[arg(
        long,
        conflicts_with_all = [
            "screen", "raw", "tail", "keep_color", "cols", "no_truncate", "from_file"
        ]
    )]
    pub json: bool,

    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

impl LogsArgs {
    /// A wait gate is configured when any wait condition flag is present.
    pub fn wait_mode(&self) -> bool {
        self.after.is_some() || self.exit || self.idle_ms.is_some() || self.pattern.is_some()
    }

    /// Wait-only mode: gate flags but nothing to read selected (nor any
    /// render modifier that would imply the default tail read). Prints the
    /// condition result and the new cursor.
    pub fn wait_only(&self) -> bool {
        self.wait_mode()
            && !self.screen
            && self.from.is_none()
            && self.tail.is_none()
            && !self.keep_color
            && !self.no_truncate
            && !self.from_file
            && self.cols.is_none()
    }
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Session ID to verify. If omitted, verifies all sessions.
    pub id: Option<String>,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
}

#[derive(Debug, Args)]
pub struct SessionRefArgs {
    /// Session ID. If omitted, uses the most recently created session.
    pub id: Option<String>,
    /// Emit one JSON object instead of tab-separated text.
    #[arg(long)]
    pub json: bool,
    /// Target a secondary node by name.
    #[arg(long, short = 'n')]
    pub node: Option<String>,
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
    /// Comma-separated scopes: observe,control,manage,node,all
    /// (default: node — the historical node-join-only key).
    #[arg(long, default_value = "node")]
    pub scopes: String,
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
    use super::{
        Cli, Commands, DaemonCommand, NotificationSetting, NotifyCommand, parse_timeout_ms,
    };
    use clap::Parser;

    #[test]
    fn restart_parses_required_id_force_and_node() {
        let cli = Cli::try_parse_from([
            "oly",
            "restart",
            "session-1",
            "--force",
            "--node",
            "worker-a",
        ])
        .unwrap();
        let Commands::Restart(args) = cli.command else {
            panic!("expected restart command");
        };
        assert_eq!(args.id, "session-1");
        assert!(args.force);
        assert_eq!(args.node.as_deref(), Some("worker-a"));
        assert!(Cli::try_parse_from(["oly", "restart"]).is_err());
    }

    #[test]
    fn rm_parses_id_force_and_node() {
        let cli = Cli::try_parse_from(["oly", "rm", "session-1", "--force", "--node", "worker-a"])
            .unwrap();
        let Commands::Remove(args) = cli.command else {
            panic!("expected remove command");
        };
        assert_eq!(args.id.as_deref(), Some("session-1"));
        assert!(args.force);
        assert_eq!(args.node.as_deref(), Some("worker-a"));
    }

    #[test]
    fn rm_defaults_force_off_and_accepts_alias() {
        let cli = Cli::try_parse_from(["oly", "delete", "session-2"]).unwrap();
        let Commands::Remove(args) = cli.command else {
            panic!("expected remove command via delete alias");
        };
        assert_eq!(args.id.as_deref(), Some("session-2"));
        assert!(!args.force);
        assert_eq!(args.node, None);
    }

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
        assert_eq!(args.timeout, Some(10_000));
    }

    #[test]
    fn logs_screen_accepts_keep_color_but_not_tail() {
        // --screen renders through the same engine as the default mode, so
        // --keep-color is meaningful there; --tail is not (the screen is
        // always the whole visible viewport).
        let cli = Cli::try_parse_from(["oly", "logs", "s1", "--screen", "--keep-color"]).unwrap();
        let Commands::Logs(args) = cli.command else {
            panic!("expected logs command");
        };
        assert!(args.screen && args.keep_color);
        assert!(Cli::try_parse_from(["oly", "logs", "s1", "--screen", "--tail", "5"]).is_err());
    }

    /// The logs surface is three orthogonal axes: what to read (default
    /// tail / --screen / --from / --raw), an optional gate (-w or
    /// --after/--exit/--idle-ms/--pattern), and format modifiers. Any
    /// meaningful combination parses; meaningless ones are usage errors
    /// instead of being silently ignored.
    #[test]
    fn logs_ergonomic_axis_combinations_parse() {
        let ok: &[&[&str]] = &[
            // Gates compose with every read selector (block, then read).
            &["oly", "logs", "s1", "--after", "10", "--screen"],
            &["oly", "logs", "s1", "--exit", "--tail", "40"],
            &[
                "oly", "logs", "s1", "--from", "10", "--after", "10", "--json",
            ],
            &[
                "oly",
                "logs",
                "s1",
                "--idle-ms",
                "800",
                "--screen",
                "--keep-color",
            ],
            &["oly", "logs", "s1", "-w", "--screen"],
            &["oly", "logs", "s1", "-w", "--from", "0", "--json"],
            &["oly", "logs", "s1", "--pattern", "DONE", "--after", "0"],
            // Format modifiers across the rendered modes.
            &[
                "oly",
                "logs",
                "s1",
                "--screen",
                "--from-file",
                "--keep-color",
            ],
            &["oly", "logs", "s1", "--cols", "120"],
            &["oly", "logs", "s1", "--cols", "120", "--screen"],
            &["oly", "logs", "s1", "--tail", "10", "--keep-color", "-w"],
            // Wait-only with JSON.
            &["oly", "logs", "s1", "--after", "0", "--json"],
        ];
        for argv in ok {
            assert!(Cli::try_parse_from(*argv).is_ok(), "should parse: {argv:?}");
        }

        let err: &[&[&str]] = &[
            // Two read selectors.
            &["oly", "logs", "s1", "--screen", "--from", "0"],
            &["oly", "logs", "s1", "--raw", "--screen"],
            // Two gates.
            &["oly", "logs", "s1", "-w", "--after", "0"],
            // Render modifiers that do not apply to the selected read.
            &["oly", "logs", "s1", "--from", "0", "--keep-color"],
            &["oly", "logs", "s1", "--from", "0", "--cols", "80"],
            &["oly", "logs", "s1", "--screen", "--no-truncate"],
            &["oly", "logs", "s1", "--cols", "80", "--no-truncate"],
            &["oly", "logs", "s1", "--raw", "--keep-color"],
            // Raw export and gates do not compose.
            &["oly", "logs", "s1", "--raw", "--exit"],
            &["oly", "logs", "s1", "--raw", "--after", "0"],
            // JSON only shapes --from windows and wait-only results.
            &["oly", "logs", "s1", "--screen", "--json"],
            &["oly", "logs", "s1", "--tail", "5", "--json"],
            // --limit / --cols need their selector.
            &["oly", "logs", "s1", "--limit", "100"],
        ];
        for argv in err {
            assert!(Cli::try_parse_from(*argv).is_err(), "should fail: {argv:?}");
        }
    }

    #[test]
    fn logs_timeout_defaults_to_none_for_mode_defaults() {
        // No explicit --timeout: run_logs applies the per-mode defaults
        // (5m with --wait-for-prompt, 30s in wait mode).
        let cli = Cli::try_parse_from(["oly", "logs", "session-1"]).unwrap();
        let Commands::Logs(args) = cli.command else {
            panic!("expected logs command");
        };
        assert_eq!(args.timeout, None);
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
    fn update_parses_title_tags_and_node() {
        let cli = Cli::try_parse_from([
            "oly",
            "update",
            "session-1",
            "--title",
            "Deploy ready",
            "--tag",
            "prod",
            "--tag",
            "release",
            "--notifications",
            "disabled",
            "--node",
            "worker-a",
        ])
        .unwrap();
        let Commands::Update(args) = cli.command else {
            panic!("expected update command");
        };
        assert_eq!(args.id, "session-1");
        assert_eq!(args.title.as_deref(), Some("Deploy ready"));
        assert_eq!(
            args.tags,
            Some(vec!["prod".to_string(), "release".to_string()])
        );
        assert_eq!(args.notifications, Some(NotificationSetting::Disabled));
        assert_eq!(args.node.as_deref(), Some("worker-a"));
    }

    #[test]
    fn update_distinguishes_omitted_and_empty_overrides() {
        let cli = Cli::try_parse_from(["oly", "update", "session-1", "--title", ""]).unwrap();
        let Commands::Update(args) = cli.command else {
            panic!("expected update command");
        };
        assert_eq!(args.id, "session-1");
        assert_eq!(args.title.as_deref(), Some(""));
        assert_eq!(args.tags, None);
        assert_eq!(args.notifications, None);

        let cli = Cli::try_parse_from(["oly", "update", "session-1", "--tag", ""]).unwrap();
        let Commands::Update(args) = cli.command else {
            panic!("expected update command");
        };
        assert_eq!(args.title, None);
        assert_eq!(args.tags, Some(vec!["".to_string()]));
        assert_eq!(args.notifications, None);

        let cli = Cli::try_parse_from(["oly", "update", "session-1", "--notifications", "enabled"])
            .unwrap();
        let Commands::Update(args) = cli.command else {
            panic!("expected update command");
        };
        assert_eq!(args.title, None);
        assert_eq!(args.tags, None);
        assert_eq!(args.notifications, Some(NotificationSetting::Enabled));
    }

    #[test]
    fn list_parses_repeatable_tag_filters() {
        let cli = Cli::try_parse_from(["oly", "ls", "--tag", "prod", "--tag", "release"]).unwrap();
        let Commands::List(args) = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(args.tags, vec!["prod".to_string(), "release".to_string()]);
    }

    #[test]
    fn list_parses_multiple_nodes_and_local_node() {
        let cli = Cli::try_parse_from([
            "oly",
            "ls",
            "--follow",
            "--node",
            "worker-a",
            "--node",
            "worker-b",
            "--node-local",
        ])
        .unwrap();
        let Commands::List(args) = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(args.node, vec!["worker-a", "worker-b"]);
        assert!(args.node_local);
    }

    #[test]
    fn list_parses_follow_and_rejects_json_combination() {
        let cli = Cli::try_parse_from(["oly", "ls", "--follow"]).unwrap();
        let Commands::List(args) = cli.command else {
            panic!("expected list command");
        };
        assert!(args.follow);
        assert_eq!(args.limit, 100);
        assert!(Cli::try_parse_from(["oly", "ls", "--follow", "--json"]).is_err());
    }

    #[test]
    fn list_limit_defaults_depend_on_follow_and_explicit_value_wins() {
        let cli = Cli::try_parse_from(["oly", "ls"]).unwrap();
        let Commands::List(args) = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(args.limit, 10);

        let cli = Cli::try_parse_from(["oly", "ls", "--follow", "--limit", "25"]).unwrap();
        let Commands::List(args) = cli.command else {
            panic!("expected list command");
        };
        assert_eq!(args.limit, 25);
    }

    #[test]
    fn daemon_start_parses_notification_hook_override() {
        let cli = Cli::try_parse_from([
            "oly",
            "daemon",
            "start",
            "--notification-hook",
            "C:/tools/notify.exe",
        ])
        .unwrap();
        let Commands::Daemon(args) = cli.command else {
            panic!("expected daemon command");
        };
        let DaemonCommand::Start(args) = args.command else {
            panic!("expected daemon start subcommand");
        };
        assert_eq!(
            args.notification_hook.as_deref(),
            Some("C:/tools/notify.exe")
        );
    }

    #[test]
    fn daemon_start_parses_web_push_proxy_override() {
        let cli = Cli::try_parse_from([
            "oly",
            "daemon",
            "start",
            "--web-push-proxy",
            "socks5://127.0.0.1:1080",
        ])
        .unwrap();
        let Commands::Daemon(args) = cli.command else {
            panic!("expected daemon command");
        };
        let DaemonCommand::Start(args) = args.command else {
            panic!("expected daemon start subcommand");
        };
        assert_eq!(
            args.web_push_proxy.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
    }

    #[test]
    fn daemon_start_parses_bind_override() {
        let cli = Cli::try_parse_from(["oly", "daemon", "start", "--bind", "0.0.0.0"]).unwrap();
        let Commands::Daemon(args) = cli.command else {
            panic!("expected daemon command");
        };
        let DaemonCommand::Start(args) = args.command else {
            panic!("expected daemon start subcommand");
        };
        assert_eq!(args.bind.as_deref(), Some("0.0.0.0"));
    }
}
