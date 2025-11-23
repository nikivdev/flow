use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{net::IpAddr, path::PathBuf};

/// Command line interface for the flow daemon / CLI hybrid.
#[derive(Parser, Debug)]
#[command(
    name = "flow",
    version,
    about = "Your second OS",
    subcommand_required = false,
    arg_required_else_help = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
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
        long_about = "Checks the /health endpoint on the configured host/port (defaults to 127.0.0.1:6000). If unreachable, a daemon is launched in the background using ~/.config/flow/config.toml, then a TUI opens so you can inspect managed servers and aggregated logs."
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
    /// Execute a specific project task (hidden; used by the palette and task shortcuts).
    #[command(hide = true)]
    Run(TaskRunOpts),
    /// Invoke tasks directly via `f <task>` without typing `run`.
    #[command(external_subcommand)]
    TaskShortcut(Vec<String>),
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
    /// Name of the task to execute.
    #[arg(value_name = "TASK")]
    pub name: String,
}

#[derive(Args, Debug, Clone)]
pub struct TaskActivateOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
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
    #[arg(long, default_value_t = 6000)]
    pub port: u16,

    /// Optional path to the global flow config (defaults to ~/.config/flow/config.toml).
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
