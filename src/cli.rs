use clap::{Args, Parser, Subcommand};
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
        about = "Run the Axum daemon so other processes can interact with it.",
        long_about = "Boot the Axum-based daemon that powers flow. This exposes /health, /screen/latest, and /screen/stream endpoints and hosts any managed servers/watchers defined in your config."
    )]
    Daemon(DaemonOpts),
    #[command(
        about = "Generate a short mock preview of frames as if they were captured off the screen.",
        long_about = "Preview mock screen frames directly in the terminal for quick FPS/buffer tuning without bringing up the daemon."
    )]
    Screen(ScreenOpts),
    #[command(
        about = "Inspect managed HTTP servers and view their logs in a TUI.",
        long_about = "Connect to the running daemon (default 127.0.0.1:9050) and display managed servers in a terminal UI. Use arrow keys or j/k to navigate, Enter to toggle logs, and q to exit."
    )]
    Servers(ServersOpts),
    #[command(
        about = "Ensure the background hub daemon is running (spawns it if missing).",
        long_about = "Checks the /health endpoint on the configured host/port (defaults to 127.0.0.1:6000). If unreachable, a daemon is launched in the background using ~/.config/flow/config.toml and the command waits until it is healthy."
    )]
    Hub(HubCommand),
    #[command(
        about = "Emit shell helpers (aliases) defined in flow.toml.",
        long_about = "Print alias definitions derived from the current flow.toml so you can source them (e.g., eval \"$(f setup)\") inside your shell session."
    )]
    Setup(SetupOpts),
    #[command(
        about = "List project automation tasks defined in flow.toml.",
        long_about = "Read the project flow.toml, render a numbered list of tasks plus their descriptions, and exit."
    )]
    Tasks(TasksOpts),
    #[command(
        about = "Execute a specific project task.",
        long_about = "Look up the named task from flow.toml (respecting declared dependencies) and run its shell command."
    )]
    Run(TaskRunOpts),
    #[command(
        about = "Index the current repository with Codanna and store the stats snapshot.",
        long_about = "Runs 'codanna index' for the current project, then captures 'codanna mcp get_index_info --json' and persists the payload to ~/.db/flow/flow.sqlite so other tools can consume it."
    )]
    Index(IndexOpts),
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
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum HubAction {
    #[command(about = "Start or ensure the hub daemon is running")]
    Start,
    #[command(about = "Stop the hub daemon if it was started by flow")]
    Stop,
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
