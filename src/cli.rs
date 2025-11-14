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
    /// Run the HTTP daemon so other processes can interact with it.
    Daemon(DaemonOpts),
    /// Generate a short mock preview of frames as if they were captured off the screen.
    Screen(ScreenOpts),
    /// Inspect managed HTTP servers and view their logs in a TUI.
    Servers(ServersOpts),
    /// List project automation tasks defined in flow.toml (default command).
    Tasks(TasksOpts),
    /// Execute a specific project task.
    Run(TaskRunOpts),
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
