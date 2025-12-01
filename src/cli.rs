use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{net::IpAddr, path::PathBuf};

/// Command line interface for the flow daemon / CLI hybrid.
#[derive(Parser, Debug)]
#[command(
    name = "flow",
    version = version_with_build_time(),
    about = "Your second OS",
    subcommand_required = false,
    arg_required_else_help = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Returns version string with relative build time (e.g., "0.1.0 (built 5m ago)")
fn version_with_build_time() -> &'static str {
    use std::sync::OnceLock;
    static VERSION: OnceLock<String> = OnceLock::new();

    // Include the generated timestamp file to force recompilation when it changes
    const BUILD_TIMESTAMP_STR: &str = include_str!(concat!(env!("OUT_DIR"), "/build_timestamp.txt"));

    VERSION.get_or_init(|| {
        let version = env!("CARGO_PKG_VERSION");
        let build_timestamp: u64 = BUILD_TIMESTAMP_STR.trim().parse().unwrap_or(0);

        if build_timestamp == 0 {
            return version.to_string();
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let elapsed = now.saturating_sub(build_timestamp);
        let relative = format_relative_time(elapsed);

        format!("{version} (built {relative})")
    })
}

fn format_relative_time(seconds: u64) -> String {
    if seconds < 60 {
        format!("{}s ago", seconds)
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        let hours = seconds / 3600;
        format!("{}h ago", hours)
    } else {
        let days = seconds / 86400;
        format!("{}d ago", days)
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    #[command(
        about = "Fuzzy search global commands/tasks without a project flow.toml.",
        long_about = "Browse global commands and tasks from your global flow config (e.g., ~/.config/flow/flow.toml). Useful when you are outside a project directory.",
        alias = "s"
    )]
    Search,
    #[command(
        about = "Ensure the background hub daemon is running (spawns it if missing).",
        long_about = "Checks the /health endpoint on the configured host/port (defaults to 127.0.0.1:9050). If unreachable, a daemon is launched in the background using the lin runtime recorded via `lin register` (or PATH), then a TUI opens so you can inspect managed servers and aggregated logs."
    )]
    Hub(HubCommand),
    #[command(
        about = "Scaffold a new flow.toml in the current directory.",
        long_about = "Creates a starter flow.toml with stub tasks (setup, dev) so you can fill in commands later."
    )]
    Init(InitOpts),
    #[command(
        about = "Verify required tools and shell integrations.",
        long_about = "Checks for flox (for managed deps), lin (hub helper), and direnv + shell hook presence."
    )]
    Doctor(DoctorOpts),
    #[command(
        about = "List tasks from the current project flow.toml (name + description).",
        long_about = "Prints the tasks defined in the active flow.toml along with any descriptions, suitable for piping into a launcher."
    )]
    Tasks(TasksOpts),
    /// Execute a specific project task (hidden; used by the palette and task shortcuts).
    #[command(hide = true)]
    Run(TaskRunOpts),
    /// Invoke tasks directly via `f <task>` without typing `run`.
    #[command(external_subcommand)]
    TaskShortcut(Vec<String>),
    #[command(about = "Show the last task input and its output/error.")]
    LastCmd,
    #[command(about = "Show the last task run (command, status, and output) recorded by flow.")]
    LastCmdFull,
    #[command(
        about = "List running flow processes for the current project.",
        long_about = "Lists flow-started processes tracked for this project. Use --all to see processes across all projects."
    )]
    Ps(ProcessOpts),
    #[command(
        about = "Stop running flow processes.",
        long_about = "Kill flow-started processes by task name, PID, or all for the project. Sends SIGTERM first, then SIGKILL after timeout."
    )]
    Kill(KillOpts),
    #[command(
        about = "View logs from running or recent tasks.",
        long_about = "Tail the log output of a running task. Use -f to follow in real-time."
    )]
    Logs(TaskLogsOpts),
    #[command(
        about = "List registered projects.",
        long_about = "Shows all projects that have been registered (projects with a 'name' field in flow.toml)."
    )]
    Projects,
}

#[derive(Args, Debug, Clone)]
pub struct DaemonOpts {
    /// Address to bind the Axum server to.
    #[arg(long, default_value = "0.0.0.0")]
    pub host: IpAddr,

    /// TCP port for the daemon's HTTP interface.
    #[arg(long, default_value_t = 9050)]
    pub port: u16,

    /// Target FPS for the mock frame generator until a real screen capture backend lands.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=120))]
    pub fps: u8,

    /// Buffer size for the broadcast channel that fans screen frames out to connected clients.
    #[arg(long, default_value_t = 512)]
    pub frame_buffer: usize,

    /// Optional path to the flow config TOML (defaults to ~/.config/flow/config.toml).
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ScreenOpts {
    /// Number of frames to preview before exiting.
    #[arg(long, default_value_t = 10)]
    pub frames: u16,

    /// Frame generation rate for the preview stream.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=60))]
    pub fps: u8,

    /// How many frames we keep buffered locally while previewing.
    #[arg(long, default_value_t = 64)]
    pub frame_buffer: usize,
}

#[derive(Args, Debug, Clone)]
pub struct LogsOpts {
    /// Hostname or IP address of the running flowd daemon.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// TCP port of the daemon's HTTP interface.
    #[arg(long, default_value_t = 9050)]
    pub port: u16,

    /// Specific server to fetch logs for (omit to dump all servers).
    #[arg(long)]
    pub server: Option<String>,

    /// Number of log lines to fetch per server when not streaming.
    #[arg(long, default_value_t = 200)]
    pub limit: usize,

    /// Stream logs in real-time (requires --server).
    #[arg(long)]
    pub follow: bool,

    /// Disable ANSI color output in log prefixes.
    #[arg(long)]
    pub no_color: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TraceOpts {
    /// Show the last command's input/output instead of streaming events.
    #[arg(long)]
    pub last_command: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ServersOpts {
    /// Hostname or IP address of the running flowd daemon.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// TCP port of the daemon's HTTP interface.
    #[arg(long, default_value_t = 9050)]
    pub port: u16,
}

#[derive(Args, Debug, Clone)]
pub struct TasksOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
}

impl Default for TasksOpts {
    fn default() -> Self {
        Self {
            config: PathBuf::from("flow.toml"),
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct TaskRunOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Hand off the task to the hub daemon instead of running it locally.
    #[arg(long)]
    pub delegate_to_hub: bool,
    /// Hub host to delegate tasks to (defaults to the local lin daemon).
    #[arg(long, default_value = "127.0.0.1")]
    pub hub_host: IpAddr,
    /// Hub port to delegate tasks to.
    #[arg(long, default_value_t = 9050)]
    pub hub_port: u16,
    /// Name of the task to execute.
    #[arg(value_name = "TASK")]
    pub name: String,
    /// Additional arguments passed to the task command.
    #[arg(value_name = "ARGS", trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct TaskActivateOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct ProcessOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Show all running flow processes across all projects.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug, Clone)]
pub struct KillOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Kill by task name.
    #[arg(value_name = "TASK")]
    pub task: Option<String>,
    /// Kill by PID directly.
    #[arg(long)]
    pub pid: Option<u32>,
    /// Kill all processes for this project.
    #[arg(long)]
    pub all: bool,
    /// Force kill (SIGKILL) without graceful shutdown.
    #[arg(long, short)]
    pub force: bool,
    /// Timeout in seconds before sending SIGKILL (default: 5).
    #[arg(long, default_value_t = 5)]
    pub timeout: u64,
}

#[derive(Args, Debug, Clone)]
pub struct TaskLogsOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Task name to view logs for.
    #[arg(value_name = "TASK")]
    pub task: Option<String>,
    /// Follow the log in real-time (like tail -f).
    #[arg(long, short)]
    pub follow: bool,
    /// Number of lines to show from the end.
    #[arg(long, short = 'n', default_value_t = 50)]
    pub lines: usize,
    /// Show logs for all projects.
    #[arg(long)]
    pub all: bool,
    /// List available log files instead of showing content.
    #[arg(long, short)]
    pub list: bool,
    /// Look up logs by registered project name instead of config path.
    #[arg(long, short)]
    pub project: Option<String>,
    /// Suppress headers, output only log content.
    #[arg(long, short)]
    pub quiet: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorOpts {}

#[derive(Args, Debug, Clone)]
pub struct InitOpts {
    /// Where to write the scaffolded flow.toml (defaults to ./flow.toml).
    #[arg(long)]
    pub path: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct HubCommand {
    #[command(flatten)]
    pub opts: HubOpts,

    #[command(subcommand)]
    pub action: Option<HubAction>,
}

#[derive(Args, Debug, Clone)]
pub struct HubOpts {
    /// Hostname or IP address of the hub daemon.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: IpAddr,

    /// TCP port for the daemon's HTTP interface.
    #[arg(long, default_value_t = 9050)]
    pub port: u16,

    /// Optional path to the lin hub config (defaults to lin's built-in lookup).
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Skip launching the hub TUI after ensuring the daemon is running.
    #[arg(long)]
    pub no_ui: bool,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum HubAction {
    #[command(about = "Start or ensure the hub daemon is running")]
    Start,
    #[command(about = "Stop the hub daemon if it was started by flow")]
    Stop,
}

#[derive(Args, Debug, Clone)]
pub struct SecretsCommand {
    #[command(subcommand)]
    pub action: SecretsAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SecretsAction {
    #[command(about = "List configured secret environments")]
    List(SecretsListOpts),
    #[command(about = "Fetch secrets for a specific environment")]
    Pull(SecretsPullOpts),
}

#[derive(Args, Debug, Clone)]
pub struct SecretsListOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct SecretsPullOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,

    /// Environment name defined in the storage config.
    #[arg(value_name = "ENV")]
    pub env: String,

    /// Optional override for the storage hub URL (default flow.1focus.ai).
    #[arg(long)]
    pub hub: Option<String>,

    /// Optional file to write secrets to (defaults to stdout).
    #[arg(long)]
    pub output: Option<PathBuf>,

    /// Output format for rendered secrets.
    #[arg(long, default_value_t = SecretsFormat::Shell, value_enum)]
    pub format: SecretsFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SecretsFormat {
    Shell,
    Dotenv,
}

#[derive(Args, Debug, Clone)]
pub struct SetupOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct IndexOpts {
    /// Codanna binary to execute (defaults to looking up 'codanna' in PATH).
    #[arg(long, default_value = "codanna")]
    pub binary: String,

    /// Directory to index; defaults to the current working directory.
    #[arg(long)]
    pub project_root: Option<PathBuf>,

    /// SQLite destination for snapshots (defaults to ~/.db/flow/flow.sqlite).
    #[arg(long)]
    pub database: Option<PathBuf>,
}
