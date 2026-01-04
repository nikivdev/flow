use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{net::IpAddr, path::PathBuf};

use crate::commit::ReviewModelArg;

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
    const BUILD_TIMESTAMP_STR: &str =
        include_str!(concat!(env!("OUT_DIR"), "/build_timestamp.txt"));

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
        about = "Run tasks from the global flow config.",
        long_about = "Run tasks defined in ~/.config/flow/flow.toml without project discovery.",
        alias = "g"
    )]
    Global(GlobalCommand),
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
    #[command(about = "Re-run the last task executed in this project.")]
    Rerun(RerunOpts),
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
    #[command(
        about = "Fuzzy search AI sessions across all projects and copy context.",
        long_about = "Browse AI sessions (Claude, Codex) across all projects. On selection, copies the session context since last checkpoint to clipboard for passing to another session.",
        alias = "ss"
    )]
    Sessions(SessionsOpts),
    #[command(
        about = "Show or set the active project.",
        long_about = "The active project is used as a fallback for commands like `f logs` when not in a project directory."
    )]
    Active(ActiveOpts),
    #[command(
        about = "Start the flow HTTP server for log ingestion and queries.",
        long_about = "Runs an HTTP server with endpoints for log ingestion (/logs/ingest) and queries (/logs/query)."
    )]
    Server(ServerOpts),
    #[command(
        about = "Match a natural language query to a task using LM Studio.",
        long_about = "Uses a local LM Studio model to intelligently match your query to an available task. Requires LM Studio running on localhost:1234 (or custom port).",
        alias = "m"
    )]
    Match(MatchOpts),
    #[command(
        about = "AI-powered commit with code review and GitEdit sync.",
        long_about = "Stages all changes, runs code review for bugs/security, generates commit message, commits, pushes, and syncs AI sessions to gitedit.dev.",
        alias = "c"
    )]
    Commit(CommitOpts),
    #[command(
        about = "Simple AI commit without code review.",
        long_about = "Stages all changes, uses OpenAI to generate a commit message from the diff, commits, and pushes. No code review.",
        visible_alias = "commitSimple",
        hide = true
    )]
    CommitSimple(CommitOpts),
    #[command(
        about = "AI commit with code review (no GitEdit sync).",
        long_about = "Like 'commit' but without syncing to gitedit.dev.",
        alias = "cc",
        visible_alias = "commitWithCheck",
        hide = true
    )]
    CommitWithCheck(CommitOpts),
    #[command(
        about = "Fix common TOML syntax errors in flow.toml.",
        long_about = "Automatically fixes common issues in flow.toml that can break parsing, such as invalid escape sequences (\\$, \\n in basic strings), unclosed quotes, and other TOML syntax errors."
    )]
    Fixup(FixupOpts),
    #[command(
        about = "Manage background daemons (start, stop, status).",
        long_about = "Start, stop, and monitor background daemons defined in flow.toml. Daemons are long-running processes like sync servers, API servers, or file watchers.",
        alias = "d"
    )]
    Daemon(DaemonCommand),
    #[command(
        about = "Manage AI coding sessions (Claude Code).",
        long_about = "Track, list, and resume Claude Code sessions for the current project. Sessions are stored in .ai/sessions/claude/ and can be named for easy recall."
    )]
    Ai(AiCommand),
    #[command(
        about = "Manage project env vars and 1focus sync.",
        long_about = "With no arguments, lists project env vars for the current environment. Use subcommands to manage env vars via 1focus or run the sync workflow."
    )]
    Env(EnvCommand),
    #[command(
        about = "Manage project todos.",
        long_about = "Create, list, edit, and complete lightweight todos stored in .ai/todos/todos.json."
    )]
    Todo(TodoCommand),
    #[command(
        about = "Manage Codex skills (.ai/skills/).",
        long_about = "Create, list, and manage Codex skills for this project. Skills are stored in .ai/skills/ and help Codex understand project-specific workflows."
    )]
    Skills(SkillsCommand),
    #[command(
        about = "Install or update project dependencies.",
        long_about = "Detects the package manager from lockfiles and runs install/update at the project root."
    )]
    Deps(DepsCommand),
    #[command(
        about = "Manage storage providers (e.g., Jazz).",
        long_about = "Provision storage backends and populate environment variables for services like Jazz."
    )]
    Storage(StorageCommand),
    #[command(
        about = "Manage AI tools (.ai/tools/*.ts).",
        long_about = "Create, list, and run TypeScript tools via Bun. Tools are fast, reusable scripts stored in .ai/tools/. Use 'codify' to generate tools from natural language.",
        alias = "t"
    )]
    Tools(ToolsCommand),
    #[command(
        about = "Send a proposal notification to Lin for approval.",
        long_about = "Sends a proposal to the Lin app widget for user approval. Used for human-in-the-loop AI workflows."
    )]
    Notify(NotifyCommand),
    #[command(
        about = "Browse and analyze git commits with AI session metadata.",
        long_about = "Fuzzy search through git commits, showing attached AI sessions and review metadata. Allows jumping between commits to see the context and reasoning behind changes."
    )]
    Commits(CommitsOpts),
    #[command(
        about = "Print aliases or run setup task for the current project.",
        long_about = "Outputs shell aliases from flow.toml or runs the 'setup' task if defined."
    )]
    Setup(SetupOpts),
    #[command(
        about = "Invoke gen AI agents.",
        long_about = "Run gen agents with prompts. Supports project and global agents. Special: flow (flow-aware).",
        alias = "a"
    )]
    Agents(AgentsCommand),
    #[command(
        about = "Manage upstream fork workflow.",
        long_about = "Set up and manage upstream forks. Creates a local 'upstream' branch to cleanly track the original repo, making merges easier.",
        alias = "up"
    )]
    Upstream(UpstreamCommand),
    #[command(
        about = "Deploy project to host or cloud platform.",
        long_about = "Deploy your project to a Linux host (via SSH), Cloudflare Workers, or Railway. Automatically detects platform from flow.toml [host], [cloudflare], or [railway] sections."
    )]
    Deploy(DeployCommand),
    #[command(
        about = "Run the project's release task.",
        long_about = "Runs the task configured by flow.release_task in flow.toml. Falls back to a task named 'release' or 'release-build', then to flow.primary_task."
    )]
    Release(ReleaseOpts),
    #[command(
        about = "Publish project to GitHub.",
        long_about = "Create a new GitHub repository and push the current project. Infers repo name from folder, asks for public/private visibility."
    )]
    Publish(PublishOpts),
    #[command(
        about = "Clone repositories into a structured local directory.",
        long_about = "Clone repositories into ~/repos/<owner>/<repo> with SSH URLs and optional upstream setup for forks."
    )]
    Repos(ReposCommand),
    #[command(
        about = "Run tasks in parallel with pretty status display.",
        long_about = "Execute multiple shell commands in parallel with a real-time status display showing spinners, progress, and output. Useful for running independent tasks concurrently.",
        alias = "p"
    )]
    Parallel(ParallelCommand),
    #[command(
        about = "Manage auto-generated documentation in .ai/docs/.",
        long_about = "AI-maintained documentation that stays in sync with the codebase. Docs are stored in .ai/docs/ and can be updated based on recent commits."
    )]
    Docs(DocsCommand),
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
pub struct GlobalCommand {
    #[command(subcommand)]
    pub action: Option<GlobalAction>,
    /// Task name to run (omit to list global tasks).
    #[arg(value_name = "TASK")]
    pub task: Option<String>,
    /// List global tasks.
    #[arg(long, short)]
    pub list: bool,
    /// Additional arguments passed to the task command.
    #[arg(value_name = "ARGS", trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum GlobalAction {
    /// List global tasks.
    List,
    /// Run a global task by name.
    Run {
        /// Task name to run.
        #[arg(value_name = "TASK")]
        task: String,
        /// Additional arguments passed to the task command.
        #[arg(value_name = "ARGS", trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Match a query against global tasks (LM Studio).
    Match(MatchOpts),
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
    /// Hub task ID to fetch logs for (from delegated tasks).
    #[arg(long)]
    pub task_id: Option<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct DoctorOpts {}

#[derive(Args, Debug, Clone)]
pub struct RerunOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
}

#[derive(Args, Debug, Clone, Default)]
pub struct ActiveOpts {
    /// Project name to set as active.
    #[arg(value_name = "PROJECT")]
    pub project: Option<String>,
    /// Clear the active project.
    #[arg(long, short)]
    pub clear: bool,
}

#[derive(Args, Debug, Clone, Default)]
pub struct SessionsOpts {
    /// Filter by provider (claude, codex, or all).
    #[arg(long, short, default_value = "all")]
    pub provider: String,
    /// Number of exchanges to copy (default: all since checkpoint).
    #[arg(long, short)]
    pub count: Option<usize>,
    /// Show sessions but don't copy to clipboard.
    #[arg(long, short)]
    pub list: bool,
    /// Get full session context, ignoring checkpoints.
    #[arg(long, short)]
    pub full: bool,
    /// Generate summaries for stale sessions (uses Gemini).
    #[arg(long)]
    pub summarize: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ServerOpts {
    /// Host to bind the server to.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Port for the HTTP server.
    #[arg(long, default_value_t = 9060)]
    pub port: u16,
    #[command(subcommand)]
    pub action: Option<ServerAction>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum ServerAction {
    #[command(about = "Start the server in the foreground")]
    Foreground,
    #[command(about = "Stop the background server")]
    Stop,
}

#[derive(Args, Debug, Clone)]
pub struct InitOpts {
    /// Where to write the scaffolded flow.toml (defaults to ./flow.toml).
    #[arg(long)]
    pub path: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct HubCommand {
    #[command(subcommand)]
    pub action: Option<HubAction>,

    #[command(flatten)]
    pub opts: HubOpts,
}

#[derive(Args, Debug, Clone)]
pub struct HubOpts {
    /// Hostname or IP address of the hub daemon.
    #[arg(long, default_value = "127.0.0.1", global = true)]
    pub host: IpAddr,

    /// TCP port for the daemon's HTTP interface.
    #[arg(long, default_value_t = 9050, global = true)]
    pub port: u16,

    /// Optional path to the lin hub config (defaults to lin's built-in lookup).
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Skip launching the hub TUI after ensuring the daemon is running.
    #[arg(long, global = true)]
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

#[derive(Args, Debug, Clone)]
pub struct StorageCommand {
    #[command(subcommand)]
    pub action: StorageAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum StorageAction {
    /// Jazz worker accounts and env wiring.
    Jazz(JazzStorageCommand),
}

#[derive(Args, Debug, Clone)]
pub struct JazzStorageCommand {
    #[command(subcommand)]
    pub action: JazzStorageAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum JazzStorageAction {
    /// Create a new Jazz worker account and store env vars.
    New {
        /// What the worker account will be used for.
        #[arg(long, value_enum, default_value = "mirror")]
        kind: JazzStorageKind,
        /// Optional name for the worker account.
        #[arg(long)]
        name: Option<String>,
        /// Optional sync server (peer) URL.
        #[arg(long)]
        peer: Option<String>,
        /// Optional Jazz API key (used when constructing the default peer).
        #[arg(long)]
        api_key: Option<String>,
        /// Environment to store in (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum JazzStorageKind {
    /// Mirror worker account (gitedit-style mirror sync).
    Mirror,
    /// Env store worker account (1focus env storage).
    EnvStore,
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

#[derive(Args, Debug, Clone)]
pub struct MatchOpts {
    /// Natural language query describing the task you want to run.
    #[arg(value_name = "QUERY", trailing_var_arg = true, num_args = 1..)]
    pub query: Vec<String>,

    /// LM Studio model to use (defaults to qwen3-8b).
    #[arg(long)]
    pub model: Option<String>,

    /// LM Studio API port (defaults to 1234).
    #[arg(long, default_value_t = 1234)]
    pub port: u16,

    /// Only show the match without running the task.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CommitOpts {
    /// Skip pushing after commit.
    #[arg(long, short = 'n')]
    pub no_push: bool,
    /// Run synchronously (don't delegate to hub).
    #[arg(long, visible_alias = "no-hub")]
    pub sync: bool,
    /// Include AI session context in code review (default: off).
    #[arg(long)]
    pub context: bool,
    /// Dry run: show context that would be passed to review without committing.
    #[arg(long)]
    pub dry: bool,
    /// Use Codex instead of Claude for code review (default: Claude).
    #[arg(long)]
    pub codex: bool,
    /// Choose a specific review model (claude-opus, codex-high, codex-mini).
    #[arg(long, value_enum)]
    pub review_model: Option<ReviewModelArg>,
    /// Custom message to include in commit (appended after author line).
    #[arg(long, short = 'm')]
    pub message: Option<String>,
    /// Max tokens for AI session context (default: 1000).
    #[arg(long, short = 't', default_value = "1000")]
    pub tokens: usize,
}

#[derive(Args, Debug, Clone)]
pub struct FixupOpts {
    /// Path to the flow.toml to fix (defaults to ./flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Only show what would be fixed without making changes.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DaemonCommand {
    #[command(subcommand)]
    pub action: Option<DaemonAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DaemonAction {
    /// Start a daemon by name.
    Start {
        /// Name of the daemon to start.
        name: String,
    },
    /// Stop a running daemon.
    Stop {
        /// Name of the daemon to stop.
        name: String,
    },
    /// Restart a daemon (stop then start).
    Restart {
        /// Name of the daemon to restart.
        name: String,
    },
    /// Show status of all configured daemons.
    Status,
    /// List available daemons.
    #[command(alias = "ls")]
    List,
}

#[derive(Args, Debug, Clone)]
pub struct AiCommand {
    #[command(subcommand)]
    pub action: Option<AiAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AiAction {
    /// List all AI sessions for this project (Claude + Codex).
    #[command(alias = "ls")]
    List,
    /// Claude Code sessions only.
    Claude {
        #[command(subcommand)]
        action: Option<ProviderAiAction>,
    },
    /// Codex sessions only.
    Codex {
        #[command(subcommand)]
        action: Option<ProviderAiAction>,
    },
    /// Resume an AI session by name or ID.
    Resume {
        /// Session name or ID to resume.
        session: Option<String>,
    },
    /// Save/bookmark the current or most recent session with a name.
    Save {
        /// Name for the session.
        name: String,
        /// Session ID to save (defaults to most recent).
        #[arg(long)]
        id: Option<String>,
    },
    /// Open or create notes for a session.
    Notes {
        /// Session name or ID.
        session: String,
    },
    /// Remove a saved session from tracking (doesn't delete the actual session).
    Remove {
        /// Session name or ID to remove.
        session: String,
    },
    /// Initialize .ai folder structure in current project.
    Init,
    /// Import all existing sessions for this project.
    Import,
    /// Copy session history to clipboard (fuzzy search to select).
    Copy {
        /// Session name or ID to copy (if not provided, shows fuzzy search).
        session: Option<String>,
    },
    /// Copy last prompt and response from a session to clipboard (for context passing).
    /// Usage: f ai context [session] [path] [count]
    Context {
        /// Session name or ID (if not provided, shows fuzzy search).
        session: Option<String>,
        /// Path to project directory (default: current directory).
        path: Option<String>,
        /// Number of exchanges to include (default: 1).
        #[arg(default_value = "1")]
        count: usize,
    },
}

/// Provider-specific AI actions (for claude/codex subcommands).
#[derive(Subcommand, Debug, Clone)]
pub enum ProviderAiAction {
    /// List sessions for this provider.
    #[command(alias = "ls")]
    List,
    /// Resume a session.
    Resume {
        /// Session name or ID to resume.
        session: Option<String>,
    },
    /// Copy session history to clipboard.
    Copy {
        /// Session name or ID to copy.
        session: Option<String>,
    },
    /// Copy last prompt and response to clipboard (for context passing).
    /// Usage: f ai claude context [session] [path] [count]
    Context {
        /// Session name or ID to copy.
        session: Option<String>,
        /// Path to project directory (default: current directory).
        path: Option<String>,
        /// Number of exchanges to include (default: 1).
        #[arg(default_value = "1")]
        count: usize,
    },
}

#[derive(Args, Debug, Clone)]
pub struct EnvCommand {
    #[command(subcommand)]
    pub action: Option<EnvAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum EnvAction {
    /// Sync project settings and set up autonomous agent workflow.
    Sync,
    /// Unlock env read access (Touch ID on macOS).
    Unlock,
    /// Authenticate with 1focus to fetch env vars.
    Login,
    /// Fetch env vars from 1focus and write to .env.
    Pull {
        /// Environment to fetch (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Push local .env to 1focus.
    Push {
        /// Environment to push to (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Guided prompt to set required env vars from flow.toml.
    Guide {
        /// Environment to set in (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Apply env vars from 1focus to the configured Cloudflare worker.
    Apply,
    /// Bootstrap Cloudflare secrets from flow.toml (interactive).
    Bootstrap,
    /// Interactive env setup (uses flow.toml when configured).
    Setup {
        /// Optional .env file path to preselect.
        #[arg(short = 'f', long)]
        env_file: Option<PathBuf>,
        /// Optional environment to preselect.
        #[arg(short, long)]
        environment: Option<String>,
    },
    /// List env vars for this project.
    #[command(alias = "ls")]
    List {
        /// Environment to list (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Set a single env var.
    Set {
        /// KEY=VALUE pair to set.
        pair: String,
        /// Environment to set in (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
        /// Optional description for this env var.
        #[arg(short, long)]
        description: Option<String>,
    },
    /// Delete env var(s).
    Delete {
        /// Key(s) to delete.
        keys: Vec<String>,
        /// Environment to delete from (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Show current auth status.
    Status,
    /// Get specific env var(s) and print to stdout.
    Get {
        /// Key(s) to fetch.
        keys: Vec<String>,
        /// Fetch from personal env vars instead of project.
        #[arg(long)]
        personal: bool,
        /// Environment to fetch from (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
        /// Output format: env (KEY=VALUE), json, or value (just the value, single key only).
        #[arg(short, long, default_value = "env")]
        format: String,
    },
    /// Run a command with env vars injected from 1focus.
    Run {
        /// Fetch from personal env vars instead of project.
        #[arg(long)]
        personal: bool,
        /// Environment to fetch from (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
        /// Specific keys to inject (if empty, injects all).
        #[arg(long, short = 'k')]
        keys: Vec<String>,
        /// Command and arguments to run.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Show configured env keys from flow.toml.
    Keys,
}

#[derive(Args, Debug, Clone)]
pub struct TodoCommand {
    #[command(subcommand)]
    pub action: Option<TodoAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum TodoAction {
    /// Add a new todo.
    Add {
        /// Short title for the todo.
        title: String,
        /// Optional note to store with the todo.
        #[arg(short, long)]
        note: Option<String>,
        /// Attach a specific AI session reference (provider:session_id).
        #[arg(long, conflicts_with = "no_session")]
        session: Option<String>,
        /// Skip attaching the most recent AI session.
        #[arg(long)]
        no_session: bool,
        /// Initial status (pending, in-progress, completed, blocked).
        #[arg(short, long, value_enum, default_value_t = TodoStatusArg::Pending)]
        status: TodoStatusArg,
    },
    /// List todos (active by default).
    #[command(alias = "ls")]
    List {
        /// Include completed todos.
        #[arg(long)]
        all: bool,
    },
    /// Mark a todo as completed.
    Done {
        /// Todo id (full or prefix).
        id: String,
    },
    /// Edit a todo.
    Edit {
        /// Todo id (full or prefix).
        id: String,
        /// Update the title.
        #[arg(short, long)]
        title: Option<String>,
        /// Update the status.
        #[arg(short, long, value_enum)]
        status: Option<TodoStatusArg>,
        /// Update the note (empty clears).
        #[arg(short, long)]
        note: Option<String>,
    },
    /// Remove a todo.
    Remove {
        /// Todo id (full or prefix).
        id: String,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum TodoStatusArg {
    Pending,
    #[value(alias = "in_progress")]
    InProgress,
    Completed,
    Blocked,
}

#[derive(Args, Debug, Clone)]
pub struct DepsCommand {
    #[command(subcommand)]
    pub action: Option<DepsAction>,
    /// Force a package manager instead of auto-detect.
    #[arg(long, value_enum)]
    pub manager: Option<DepsManager>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DepsAction {
    /// Install dependencies.
    Install {
        /// Extra args to pass to the package manager.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Update dependencies to latest.
    Update {
        /// Extra args to pass to the package manager.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Fuzzy-pick a dependency or linked repo and fetch it to ~/repos.
    #[command(alias = "pick", alias = "find", alias = "search")]
    Pick,
    /// Add an external repo dependency and link it under .ai/repos.
    Repo {
        /// Repository URL, owner/repo, or repo name (searches ~/repos).
        repo: String,
        /// Root directory for clones (default: ~/repos).
        #[arg(long, default_value = "~/repos")]
        root: String,
        /// Create a private fork in your GitHub account and set origin.
        #[arg(long, alias = "private-origin")]
        private: bool,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum DepsManager {
    Pnpm,
    Npm,
    Yarn,
    Bun,
}

#[derive(Args, Debug, Clone)]
pub struct SkillsCommand {
    #[command(subcommand)]
    pub action: Option<SkillsAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SkillsAction {
    /// List all skills for this project.
    #[command(alias = "ls")]
    List,
    /// Create a new skill.
    New {
        /// Skill name (kebab-case recommended).
        name: String,
        /// Short description of what the skill does.
        #[arg(short, long)]
        description: Option<String>,
    },
    /// Show skill details.
    Show {
        /// Skill name.
        name: String,
    },
    /// Edit a skill in your editor.
    Edit {
        /// Skill name.
        name: String,
    },
    /// Remove a skill.
    Remove {
        /// Skill name.
        name: String,
    },
    /// Install a curated skill from the registry.
    Install {
        /// Skill name to install.
        name: String,
    },
    /// Search for skills in the remote registry.
    Search {
        /// Search query (optional).
        query: Option<String>,
    },
    /// Sync flow.toml tasks as skills.
    Sync,
}

#[derive(Args, Debug, Clone)]
pub struct ToolsCommand {
    #[command(subcommand)]
    pub action: Option<ToolsAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ToolsAction {
    /// List all tools for this project.
    #[command(alias = "ls")]
    List,
    /// Run a tool.
    Run {
        /// Tool name (without .ts extension).
        name: String,
        /// Arguments to pass to the tool.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Create a new tool.
    New {
        /// Tool name (kebab-case recommended).
        name: String,
        /// Short description of what the tool does.
        #[arg(short, long)]
        description: Option<String>,
        /// Use AI (localcode) to generate the tool implementation.
        #[arg(long)]
        ai: bool,
    },
    /// Edit a tool in your editor.
    Edit {
        /// Tool name.
        name: String,
    },
    /// Remove a tool.
    Remove {
        /// Tool name.
        name: String,
    },
}

#[derive(Args, Debug, Clone)]
#[command(args_conflicts_with_subcommands = true)]
pub struct AgentsCommand {
    #[command(subcommand)]
    pub action: Option<AgentsAction>,
    /// Run a global agent directly (e.g., `f agents explore`).
    #[arg(trailing_var_arg = true)]
    pub agent: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum AgentsAction {
    /// List available agents.
    #[command(alias = "ls")]
    List,
    /// Run an agent with a prompt.
    Run {
        /// Agent name (flow, codify, explore, general).
        agent: String,
        /// Prompt for the agent.
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Run a global agent (prompt optional).
    #[command(alias = "g")]
    Global {
        /// Global agent name.
        agent: String,
        /// Optional custom prompt (uses default if not provided).
        #[arg(trailing_var_arg = true)]
        prompt: Option<Vec<String>>,
    },
}

#[derive(Args, Debug, Clone)]
pub struct PublishOpts {
    /// Repository name (defaults to current folder name).
    #[arg(short, long)]
    pub name: Option<String>,
    /// Make the repository public.
    #[arg(long)]
    pub public: bool,
    /// Make the repository private.
    #[arg(long)]
    pub private: bool,
    /// Description for the repository.
    #[arg(short, long)]
    pub description: Option<String>,
    /// Skip confirmation prompts.
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ReposCommand {
    #[command(subcommand)]
    pub action: Option<ReposAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ReposAction {
    /// Clone a repository into ~/repos/<owner>/<repo>.
    Clone(ReposCloneOpts),
}

#[derive(Args, Debug, Clone)]
pub struct ReposCloneOpts {
    /// Repository URL or owner/repo.
    pub url: String,
    /// Root directory for clones (default: ~/repos).
    #[arg(long, default_value = "~/repos")]
    pub root: String,
    /// Perform a full clone (skip shallow clone + background history fetch).
    #[arg(long)]
    pub full: bool,
    /// Skip automatic upstream setup for forks.
    #[arg(long)]
    pub no_upstream: bool,
    /// Upstream URL override (defaults to fork parent via gh).
    #[arg(short = 'u', long)]
    pub upstream_url: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct UpstreamCommand {
    #[command(subcommand)]
    pub action: Option<UpstreamAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum UpstreamAction {
    /// Show current upstream configuration.
    Status,
    /// Set up upstream remote and local tracking branch.
    Setup {
        /// URL of the upstream repository.
        #[arg(short, long)]
        upstream_url: Option<String>,
        /// Branch name on upstream (default: main).
        #[arg(short, long)]
        upstream_branch: Option<String>,
    },
    /// Pull changes from upstream into local 'upstream' branch.
    Pull {
        /// Also merge into this branch after pulling.
        #[arg(short, long)]
        branch: Option<String>,
    },
    /// Full sync: pull upstream, merge to dev/main, push to origin.
    Sync {
        /// Skip pushing to origin.
        #[arg(long)]
        no_push: bool,
    },
}

#[derive(Args, Debug, Clone)]
pub struct NotifyCommand {
    /// Title of the proposal (shown in widget header).
    #[arg(short, long)]
    pub title: Option<String>,

    /// The action/command to propose (e.g., "f deploy").
    pub action: String,

    /// Optional context or description.
    #[arg(short, long)]
    pub context: Option<String>,

    /// Expiration time in seconds (default: 300 = 5 minutes).
    #[arg(short, long, default_value = "300")]
    pub expires: u64,
}

#[derive(Args, Debug, Clone, Default)]
pub struct CommitsOpts {
    /// Number of commits to show (default: 100).
    #[arg(long, short = 'n', default_value_t = 100)]
    pub limit: usize,
    /// Show commits across all branches.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DeployCommand {
    #[command(subcommand)]
    pub action: Option<DeployAction>,
}

#[derive(Args, Debug, Clone)]
pub struct ReleaseOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Additional arguments passed to the release task command.
    #[arg(value_name = "ARGS", trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DeployAction {
    /// Deploy to Linux host via SSH.
    #[command(alias = "h")]
    Host {
        /// Build remotely instead of syncing local build artifacts.
        #[arg(long)]
        remote_build: bool,
        /// Run setup script even if already deployed.
        #[arg(long)]
        setup: bool,
    },
    /// Deploy to Cloudflare Workers.
    #[command(alias = "cf")]
    Cloudflare {
        /// Also set secrets from env_file.
        #[arg(long)]
        secrets: bool,
        /// Run in dev mode instead of deploying.
        #[arg(long)]
        dev: bool,
    },
    /// Interactive deploy setup (Cloudflare Workers for now).
    Setup,
    /// Deploy to Railway.
    Railway,
    /// Show deployment status.
    Status,
    /// View deployment logs.
    Logs {
        /// Follow logs in real-time.
        #[arg(long, short)]
        follow: bool,
        /// Number of lines to show.
        #[arg(long, short = 'n', default_value_t = 100)]
        lines: usize,
    },
    /// Restart the deployed service.
    Restart,
    /// Stop the deployed service.
    Stop,
    /// SSH into the host (for host deployments).
    Shell,
    /// Configure host for deployment.
    #[command(alias = "set")]
    SetHost {
        /// SSH connection string (user@host:port or user@host).
        connection: String,
    },
    /// Show current host configuration.
    ShowHost,
    /// Check if deployment is healthy (HTTP health check).
    Health {
        /// Custom URL to check (defaults to domain from config).
        #[arg(long)]
        url: Option<String>,
        /// Expected HTTP status code.
        #[arg(long, default_value_t = 200)]
        status: u16,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ParallelCommand {
    /// Maximum number of concurrent jobs (default: number of CPU cores).
    #[arg(long, short = 'j')]
    pub jobs: Option<usize>,
    /// Stop all tasks on first failure.
    #[arg(long, short = 'f')]
    pub fail_fast: bool,
    /// Tasks to run as "label:command" pairs, or just commands (auto-labeled).
    #[arg(value_name = "TASK", trailing_var_arg = true, num_args = 1..)]
    pub tasks: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DocsCommand {
    #[command(subcommand)]
    pub action: Option<DocsAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DocsAction {
    /// Sync documentation with recent commits.
    Sync {
        /// Number of commits to analyze (default: 10).
        #[arg(long, short = 'n', default_value_t = 10)]
        commits: usize,
        /// Dry run: show what would be updated without changing files.
        #[arg(long)]
        dry: bool,
    },
    /// List documentation files.
    #[command(alias = "ls")]
    List,
    /// Show documentation status (what needs updating).
    Status,
    /// Open a doc file in editor.
    Edit {
        /// Doc file name (without .md).
        name: String,
    },
}
