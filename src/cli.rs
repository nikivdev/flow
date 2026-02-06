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

    /// Output all commands in machine-readable JSON format for external tools.
    #[arg(long, global = true)]
    pub help_full: bool,
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
        about = "Output shell integration script.",
        long_about = "Prints shell wrapper functions for commands like `f new` that need to cd. Add `eval (f shell-init fish)` to your fish config."
    )]
    ShellInit(ShellInitOpts),
    #[command(
        about = "Manage shell integration.",
        long_about = "Helper commands for shell integration like refreshing the current session."
    )]
    Shell(ShellCommand),
    #[command(
        about = "Create a new project from a template.",
        long_about = "Create a new project from ~/new/<template>. Path resolution:\n  f new <template>        → ./<template>\n  f new <template> zerg   → ~/code/zerg\n  f new <template> ./foo  → ./foo\n  f new <template> ~/path → ~/path"
    )]
    New(NewOpts),
    #[command(
        about = "Home setup and config repo management.",
        long_about = "Set up your home environment or clone a GitHub config repo into ~/config, optionally pulling an internal repo into ~/config/i, then applying symlinked configs."
    )]
    Home(HomeCommand),
    #[command(
        about = "Archive the current project to ~/archive/code.",
        long_about = "Copies the current project into ~/archive/code/<project>-<message> so you can keep a snapshot outside git history."
    )]
    Archive(ArchiveOpts),
    #[command(
        about = "Verify required tools and shell integrations.",
        long_about = "Checks for flox (for managed deps), lin (hub helper), and direnv + shell hook presence."
    )]
    Doctor(DoctorOpts),
    #[command(
        about = "Ensure your system matches Flow's expectations.",
        long_about = "Enforces fish shell, installs flow shell integration, and runs doctor checks."
    )]
    Health(HealthOpts),
    #[command(
        about = "Fuzzy search task history or list available tasks.",
        long_about = "Search through previously run tasks (most recent first) or list tasks from flow.toml."
    )]
    Tasks(TasksCommand),
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
    #[command(about = "Show the last fish shell command and output (from fish io-trace).")]
    FishLast,
    #[command(about = "Show full details of the last fish shell command.")]
    FishLastFull,
    #[command(about = "Install traced fish shell (fish fork with always-on I/O tracing).")]
    FishInstall(FishInstallOpts),
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
        about = "Quick traces for AI + task runs from jazz2 state.",
        long_about = "Print recent AI agent events and Flow task runs stored in the shared jazz2 state. Use --follow to stream.",
        alias = "traces"
    )]
    Trace(TraceCommand),
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
        about = "Open the Flow web UI for this project.",
        long_about = "Serves the .ai/web UI and project metadata (including OpenAPI when available), then opens it in your browser."
    )]
    Web(WebOpts),
    #[command(
        about = "Match a natural language query to a task using LM Studio.",
        long_about = "Uses a local LM Studio model to intelligently match your query to an available task. Requires LM Studio running on localhost:1234 (or custom port).",
        alias = "m"
    )]
    Match(MatchOpts),
    #[command(
        about = "Ask the AI server to suggest a task or Flow command.",
        long_about = "Uses the local AI server (zerg/ai) to match your query to a flow.toml task or a Flow CLI command you can run.",
        alias = "a"
    )]
    Ask(AskOpts),
    #[command(
        about = "AI-powered commit with code review and optional GitEdit sync.",
        long_about = "Stages all changes, runs code review for bugs/security, generates commit message, commits, pushes (unless commit queue is enabled), and syncs AI sessions to gitedit.dev when enabled in global config.",
        alias = "c"
    )]
    Commit(CommitOpts),
    #[command(
        about = "Manage the commit review queue.",
        long_about = "List, inspect, approve, or drop queued commits before they push to remote.",
        alias = "cq"
    )]
    CommitQueue(CommitQueueCommand),
    #[command(
        about = "Open queued commits for review in Rise.",
        long_about = "Open the latest queued commit (or a specific one in the future) in Rise's review UI.",
        alias = "rv"
    )]
    Review(ReviewCommand),
    #[command(
        about = "Simple AI commit without code review.",
        long_about = "Stages all changes, uses OpenAI to generate a commit message from the diff, commits, and pushes. No code review.",
        visible_alias = "commitSimple",
        hide = true
    )]
    CommitSimple(CommitOpts),
    #[command(
        about = "AI commit with code review (GitEdit sync honors config).",
        long_about = "Like 'commit' but without forcing gitedit.dev sync; respects the global gitedit setting.",
        alias = "cc",
        visible_alias = "commitWithCheck",
        hide = true
    )]
    CommitWithCheck(CommitOpts),
    #[command(
        about = "Undo the last undoable action (commit, push).",
        long_about = "Reverts the last recorded action. For commits, resets with --soft to keep changes staged. For pushes, force pushes the previous state.",
        alias = "u"
    )]
    Undo(UndoCommand),
    #[command(
        about = "Fix issues in the repo with help from Hive.",
        long_about = "Optionally unroll the last commit, then run a Hive agent to fix the issue (e.g., leaked secrets)."
    )]
    Fix(FixOpts),
    #[command(
        about = "Fix common TOML syntax errors in flow.toml.",
        long_about = "Automatically fixes common issues in flow.toml that can break parsing, such as invalid escape sequences (\\$, \\n in basic strings), unclosed quotes, and other TOML syntax errors."
    )]
    Fixup(FixupOpts),
    #[command(
        about = "Share or apply git diffs without remotes.",
        long_about = "Print the current git diff for sharing or apply a diff string/file to this repo. Useful when git pull/push isn't available."
    )]
    Changes(ChangesCommand),
    #[command(
        about = "Create or unpack a shareable diff bundle.",
        long_about = "Generates a diff against the main branch (including untracked files) plus AI sessions, stores it by hash, or unrolls a stored bundle by hash."
    )]
    Diff(DiffCommand),
    #[command(
        about = "Hash files or sessions with unhash and copy a share link.",
        long_about = "Runs the unhash CLI, then copies unstash./<hash> to clipboard and prints the hash/link."
    )]
    Hash(HashOpts),
    #[command(
        about = "Manage background daemons (start, stop, status).",
        long_about = "Start, stop, and monitor background daemons defined in flow.toml. Daemons are long-running processes like sync servers, API servers, or file watchers.",
        alias = "d"
    )]
    Daemon(DaemonCommand),
    #[command(
        about = "Run the Flow supervisor (daemon manager).",
        long_about = "Starts or checks the Flow supervisor, which manages background daemons via IPC."
    )]
    Supervisor(SupervisorCommand),
    #[command(
        about = "Manage AI coding sessions (Claude Code).",
        long_about = "Track, list, and resume Claude Code sessions for the current project. Sessions are stored in .ai/sessions/claude/ and can be named for easy recall."
    )]
    Ai(AiCommand),
    #[command(about = "Start or continue Codex session.", alias = "cx")]
    Codex {
        #[command(subcommand)]
        action: Option<ProviderAiAction>,
    },
    #[command(about = "Start or continue Claude session.", alias = "cl")]
    Claude {
        #[command(subcommand)]
        action: Option<ProviderAiAction>,
    },
    #[command(
        about = "Manage project env vars and cloud sync.",
        long_about = "With no arguments, lists project env vars for the current environment. Use subcommands to manage env vars via the cloud backend or run the sync workflow."
    )]
    Env(EnvCommand),
    #[command(
        about = "Fetch one-time passwords from 1Password Connect.",
        long_about = "Uses OP_CONNECT_HOST + OP_CONNECT_TOKEN (from env or Flow personal env store) to fetch an item TOTP."
    )]
    Otp(OtpCommand),
    #[command(
        about = "Authenticate Flow AI via myflow.",
        long_about = "Starts a device auth flow for myflow, storing a token for AI-powered CLI features."
    )]
    Auth(AuthOpts),
    #[command(
        about = "Onboard third-party services (Stripe, etc.) with guided env setup.",
        long_about = "Guided setup flows for external services. Prompts for required env vars, stores them in the cloud backend, and can apply them to Cloudflare."
    )]
    Services(ServicesCommand),
    #[command(
        about = "Manage macOS launch agents and daemons.",
        long_about = "List, audit, enable, and disable macOS launchd services. Helps keep your startup clean by identifying bloatware and unwanted background processes."
    )]
    Macos(MacosCommand),
    #[command(
        about = "Manage SSH keys via the cloud backend.",
        long_about = "Generate, store, and unlock SSH keys stored in cloud personal env vars, then wire git to use the Flow SSH agent."
    )]
    Ssh(SshCommand),
    #[command(
        about = "Manage project todos.",
        long_about = "Create, list, edit, and complete lightweight todos stored in .ai/todos/todos.json. With no arguments, opens the per-project Bike outliner stored in .ai/todos/<project>.bike."
    )]
    Todo(TodoCommand),
    #[command(
        about = "Copy an external dependency into ext/ and ignore it.",
        long_about = "Copies a directory into <project>/ext/<name> and adds ext/ to .gitignore."
    )]
    Ext(ExtCommand),
    #[command(
        about = "Manage Codex skills (.ai/skills/).",
        long_about = "Create, list, and manage Codex skills for this project. Skills are stored in .ai/skills/ (gitignored by default) and help Codex understand project-specific workflows."
    )]
    Skills(SkillsCommand),
    #[command(
        about = "Install or update project dependencies.",
        long_about = "Detects the package manager from lockfiles and runs install/update at the project root."
    )]
    Deps(DepsCommand),
    #[command(
        name = "db",
        about = "Manage databases (Jazz, Postgres).",
        long_about = "Provision database backends and run database workflows (Jazz worker accounts, Postgres migrations). Defaults are tuned for Planetscale Postgres."
    )]
    Db(DbCommand),
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
        long_about = "Fuzzy search through git commits, showing attached AI sessions and review metadata. Supports notable commits and quick actions."
    )]
    Commits(CommitsCommand),
    #[command(
        about = "Bootstrap project and run setup task or aliases.",
        long_about = "Bootstraps the project if needed, creates flow.toml when missing, then runs the 'setup' task or prints shell aliases."
    )]
    Setup(SetupOpts),
    #[command(
        about = "Invoke gen AI agents.",
        long_about = "Run gen agents with prompts. Supports project and global agents. Special: flow (flow-aware).",
        alias = "a"
    )]
    Agents(AgentsCommand),
    #[command(
        about = "Manage and run hive agents.",
        long_about = "Hive agents are MoonBit-powered AI agents with tool use. Agents can be project-local (.flow/agents/) or global (~/.hive/agents/).",
        alias = "h"
    )]
    Hive(HiveCommand),
    #[command(
        about = "Sync git repo: pull, upstream merge, push.",
        long_about = "Comprehensive git sync: pulls from origin, merges upstream changes if configured, and pushes. One command to keep your fork in sync."
    )]
    Sync(SyncCommand),
    #[command(
        about = "Jujutsu (jj) workflow helpers.",
        long_about = "Initialize jj, manage workspaces/bookmarks, and sync with git remotes in a safe, structured flow."
    )]
    Jj(JjCommand),
    #[command(
        about = "Repair git state (abort rebase/merge, leave detached HEAD).",
        long_about = "Aborts in-progress git operations (rebase, merge, cherry-pick, revert), resets bisect, and checks out the target branch if HEAD is detached."
    )]
    GitRepair(GitRepairOpts),
    #[command(
        about = "Show project information.",
        long_about = "Display project details including git remotes, upstream configuration, and flow.toml settings.",
        alias = "i"
    )]
    Info,
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
        about = "Deploy to production using flow.toml deploy config.",
        long_about = "Deploys using flow.toml [host], [cloudflare], [railway], or [web] configuration and skips [flow].deploy_task. If a deploy-prod or prod task exists, it will run that task instead.",
        alias = "production"
    )]
    Prod(DeployCommand),
    #[command(
        about = "Publish project to gitedit.dev or GitHub.",
        long_about = "Publish the current project. Without a subcommand, shows a fuzzy picker to choose the target."
    )]
    Publish(PublishCommand),
    #[command(
        about = "Clone repositories into a structured local directory.",
        long_about = "Clone repositories into ~/repos/<owner>/<repo> with SSH URLs and optional upstream setup for forks."
    )]
    Repos(ReposCommand),
    #[command(
        about = "Browse git repos under ~/code.",
        long_about = "Fuzzy search git repositories under ~/code and open the selected path. Also includes helpers to migrate AI sessions when paths move."
    )]
    Code(CodeCommand),
    #[command(
        about = "Move or copy a folder to a new location, preserving symlinks and AI sessions.",
        long_about = "Migrate a project folder to a new location. Usage:\n  f migrate <target>            - move current dir to target\n  f migrate <source> <target>   - move source to target\n  f migrate -c <src> <target>   - copy instead of move\n  f migrate code <relative>     - move current dir to ~/code/<relative>\n  f migrate --copy code <rel>   - copy current dir to ~/code/<relative>\nUpdates ~/bin symlinks (move only). AI sessions are moved or copied based on the mode."
    )]
    Migrate(MigrateCommand),
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
    #[command(
        about = "Upgrade flow to the latest version.",
        long_about = "Download and install the latest version of flow from GitHub releases. Checks for newer versions and replaces the current executable."
    )]
    Upgrade(UpgradeOpts),
    #[command(
        about = "Pull ~/code/flow and rebuild the local flow binary.",
        long_about = "Updates ~/code/flow, runs f deploy in that repo, and reloads the fish shell."
    )]
    Latest,
    #[command(
        about = "Release a project (registry, GitHub, or task).",
        long_about = "Release a project based on flow.toml defaults. Supports Flow registry releases, GitHub releases, or running a release task.",
        alias = "rel"
    )]
    Release(ReleaseCommand),
    #[command(
        about = "Install a binary from the Flow registry.",
        long_about = "Download a binary from a Flow registry and install it into your PATH.",
        alias = "i"
    )]
    Install(InstallCommand),
    #[command(
        about = "Manage the Flow registry (tokens, setup).",
        long_about = "Create registry tokens and wire them into worker secrets and local envs."
    )]
    Registry(RegistryCommand),
    #[command(
        about = "Zero-cost traced reverse proxy for development.",
        long_about = "Start a reverse proxy that traces all HTTP requests with zero overhead. Writes trace-summary.json for AI agents to read.",
        alias = "px"
    )]
    Proxy(ProxyCommand),
}

#[derive(Args, Debug, Clone)]
pub struct TracesOpts {
    /// Max rows per source (default: 40).
    #[arg(short = 'n', long, default_value = "40")]
    pub limit: usize,

    /// Follow and stream new entries.
    #[arg(short = 'f', long)]
    pub follow: bool,

    /// Filter by project path substring.
    #[arg(long)]
    pub project: Option<String>,

    /// Which source to show: all, tasks, ai.
    #[arg(long, value_enum, default_value = "all")]
    pub source: TraceSource,
}

#[derive(Args, Debug, Clone)]
pub struct TraceCommand {
    #[command(subcommand)]
    pub action: Option<TraceAction>,
    #[command(flatten)]
    pub events: TracesOpts,
}

#[derive(Subcommand, Debug, Clone)]
pub enum TraceAction {
    /// Show full history of the last active AI session for a project path.
    Session(TraceSessionOpts),
}

#[derive(Args, Debug, Clone)]
pub struct TraceSessionOpts {
    /// Project path to load the latest session for.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum TraceSource {
    All,
    Tasks,
    Ai,
}

// === Proxy Commands ===

#[derive(Args, Debug, Clone)]
pub struct ProxyCommand {
    #[command(subcommand)]
    pub action: ProxyAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProxyAction {
    /// Start the proxy server (reads [[proxies]] from flow.toml).
    Start(ProxyStartOpts),
    /// View recent request traces.
    #[command(alias = "t")]
    Trace(ProxyTraceOpts),
    /// Show the last request details.
    Last(ProxyLastOpts),
    /// Add a new proxy target.
    Add(ProxyAddOpts),
    /// List configured proxy targets.
    List,
    /// Stop the proxy server.
    Stop,
}

#[derive(Args, Debug, Clone)]
pub struct ProxyStartOpts {
    /// Listen address (e.g., ":8080" or "127.0.0.1:8080").
    #[arg(short, long)]
    pub listen: Option<String>,

    /// Run in foreground (don't daemonize).
    #[arg(short, long)]
    pub foreground: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ProxyTraceOpts {
    /// Number of records to show.
    #[arg(short = 'n', long, default_value = "20")]
    pub count: usize,

    /// Follow trace in real-time.
    #[arg(short, long)]
    pub follow: bool,

    /// Filter by target name.
    #[arg(long)]
    pub target: Option<String>,

    /// Show only errors (status >= 400).
    #[arg(long)]
    pub errors: bool,

    /// Filter by trace ID.
    #[arg(long)]
    pub id: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ProxyLastOpts {
    /// Show only errors.
    #[arg(long)]
    pub errors: bool,

    /// Filter by target name.
    #[arg(long)]
    pub target: Option<String>,

    /// Include request/response body.
    #[arg(long)]
    pub body: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ProxyAddOpts {
    /// Target address (e.g., "localhost:3000").
    pub target: String,

    /// Proxy name (auto-suggested if not provided).
    #[arg(short, long)]
    pub name: Option<String>,

    /// Host-based routing.
    #[arg(long)]
    pub host: Option<String>,

    /// Path prefix routing.
    #[arg(long)]
    pub path: Option<String>,
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
pub struct TasksCommand {
    #[command(subcommand)]
    pub action: Option<TasksAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum TasksAction {
    /// List tasks from the current project flow.toml.
    List(TasksListOpts),
}

#[derive(Args, Debug, Clone)]
pub struct TasksListOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
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
pub struct HealthOpts {}

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
    /// Condense the selected session into a handoff summary (uses Gemini).
    #[arg(long)]
    pub handoff: bool,
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

#[derive(Args, Debug)]
pub struct WebOpts {
    /// Port to serve the web UI on.
    #[arg(long, default_value_t = 9310)]
    pub port: u16,
    /// Host to bind the web UI server to.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
}

#[derive(Args, Debug, Clone)]
pub struct InitOpts {
    /// Where to write the scaffolded flow.toml (defaults to ./flow.toml).
    #[arg(long)]
    pub path: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ShellInitOpts {
    /// Shell to generate init script for (fish, zsh, bash).
    pub shell: String,
}

#[derive(Args, Debug, Clone)]
pub struct ShellCommand {
    #[command(subcommand)]
    pub action: Option<ShellAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ShellAction {
    /// Refresh the current shell session.
    Reset,
    /// Disable fish terminal query to avoid PDA warning.
    #[command(alias = "fix-terminal")]
    FixTerminal,
}

#[derive(Args, Debug, Clone)]
pub struct NewOpts {
    /// Template name (e.g., web, docs). If omitted, shows fuzzy picker.
    pub template: Option<String>,
    /// Destination path. Plain names go to ~/code/ (e.g., "zerg" → ~/code/zerg). Use ./ for cwd.
    pub path: Option<String>,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct HomeCommand {
    #[command(subcommand)]
    pub action: Option<HomeAction>,
    /// GitHub URL or owner/repo for the config repo.
    pub repo: Option<String>,
    /// Optional internal config repo URL (cloned into ~/config/i).
    #[arg(long)]
    pub internal: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum HomeAction {
    /// Guide home setup and validate GitHub access.
    Setup,
}

#[derive(Args, Debug, Clone)]
pub struct ArchiveOpts {
    /// Message to include in the archive folder name.
    pub message: String,
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

    /// Also start the docs hub (Next.js dev server).
    #[arg(long, global = true)]
    pub docs_hub: bool,
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

#[derive(Parser, Debug)]
pub struct OtpCommand {
    #[command(subcommand)]
    pub action: OtpAction,
}

#[derive(Subcommand, Debug)]
pub enum OtpAction {
    #[command(about = "Get a TOTP code from 1Password Connect")]
    Get {
        /// Vault name or id.
        vault: String,
        /// Item title or id.
        item: String,
        /// Optional field label to select when multiple TOTP fields exist.
        #[arg(long)]
        field: Option<String>,
    },
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

    /// Environment name defined in the secrets config.
    #[arg(value_name = "ENV")]
    pub env: String,

    /// Optional override for the secrets hub URL (default myflow.sh).
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
pub struct DbCommand {
    #[command(subcommand)]
    pub action: DbAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DbAction {
    /// Jazz worker accounts and env wiring.
    Jazz(JazzStorageCommand),
    /// Postgres workflows (migrations/generation).
    Postgres(PostgresCommand),
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
    /// Env store worker account (cloud env store).
    EnvStore,
    /// App data worker account (cloud app store).
    AppStore,
}

#[derive(Args, Debug, Clone)]
pub struct PostgresCommand {
    #[command(subcommand)]
    pub action: PostgresAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum PostgresAction {
    /// Generate Drizzle migrations for the configured Postgres project.
    Generate {
        /// Override the Postgres project directory (defaults to ~/org/la/la/server).
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Apply Drizzle migrations for the configured Postgres project.
    Migrate {
        /// Override the Postgres project directory (defaults to ~/org/la/la/server).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Explicit DATABASE_URL (falls back to env/.env/Planetscale env vars).
        #[arg(long)]
        database_url: Option<String>,
        /// Generate migrations before applying them.
        #[arg(long, default_value_t = false)]
        generate: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SecretsFormat {
    Shell,
    Dotenv,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SetupTarget {
    Deploy,
    Release,
    Docs,
}

#[derive(Args, Debug, Clone)]
pub struct SetupOpts {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    /// Optional setup target (e.g., deploy).
    #[arg(value_enum, value_name = "TARGET")]
    pub target: Option<SetupTarget>,
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
pub struct AskOpts {
    /// Natural language query describing the task or command you want to run.
    #[arg(value_name = "QUERY", trailing_var_arg = true, num_args = 1..)]
    pub query: Vec<String>,

    /// AI server model to use (defaults to AI_SERVER_MODEL).
    #[arg(long)]
    pub model: Option<String>,

    /// AI server URL (defaults to AI_SERVER_URL or http://127.0.0.1:7331).
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CommitOpts {
    /// Skip pushing after commit.
    #[arg(long, short = 'n')]
    pub no_push: bool,
    /// Queue the commit for review before pushing.
    #[arg(long)]
    pub queue: bool,
    /// Bypass commit queue and allow pushing immediately.
    #[arg(long, conflicts_with = "queue")]
    pub no_queue: bool,
    /// Force commit without queue (bypass stacked review).
    #[arg(long, conflicts_with = "queue")]
    pub force: bool,
    /// Commit and push immediately (bypass commit queue).
    #[arg(long, conflicts_with = "queue")]
    pub approved: bool,
    /// Open the queued commit in Rise for review after commit.
    #[arg(long)]
    pub review: bool,
    /// Run synchronously (don't delegate to hub).
    #[arg(long, visible_alias = "no-hub")]
    pub sync: bool,
    /// Include AI session context in code review (default: off).
    #[arg(long)]
    pub context: bool,
    /// Include an unhash.sh bundle/link in the commit message (opt-in).
    #[arg(long)]
    pub hashed: bool,
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
    /// Fast commit with optional message (defaults to ".").
    #[arg(long, value_name = "MESSAGE", num_args = 0..=1, default_missing_value = ".")]
    pub fast: Option<String>,
    /// Message to append after the AI-generated subject/body.
    #[arg(value_name = "MESSAGE", allow_hyphen_values = true)]
    pub message_arg: Option<String>,
    /// Max tokens for AI session context (default: 1000).
    #[arg(long, short = 't', default_value = "1000")]
    pub tokens: usize,
}

#[derive(Args, Debug, Clone)]
pub struct CommitQueueCommand {
    #[command(subcommand)]
    pub action: Option<CommitQueueAction>,
}

#[derive(Args, Debug, Clone)]
pub struct ReviewCommand {
    #[command(subcommand)]
    pub action: Option<ReviewAction>,
}

#[derive(Args, Debug, Clone)]
pub struct GitRepairOpts {
    /// Branch to checkout if HEAD is detached (default: main).
    #[arg(long)]
    pub branch: Option<String>,
    /// Dry run - show what would be repaired.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum CommitQueueAction {
    /// List queued commits.
    List,
    /// Show details for a queued commit.
    Show {
        /// Commit hash (short or full).
        hash: String,
    },
    /// Open the queued commit diff in Rise app (multi-file diff UI).
    Open {
        /// Commit hash (short or full).
        hash: String,
    },
    /// Print the full diff for a queued commit to stdout.
    Diff {
        /// Commit hash (short or full).
        hash: String,
    },
    /// Approve a queued commit and push it.
    Approve {
        /// Commit hash (short or full).
        hash: String,
        /// Push even if the commit is not at HEAD.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Approve all queued commits on the current branch (push once).
    ApproveAll {
        /// Push even if the branch is behind its remote.
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Remove a commit from the queue without pushing.
    Drop {
        /// Commit hash (short or full).
        hash: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ReviewAction {
    /// Open the latest queued commit in Rise.
    Latest,
}

#[derive(Args, Debug, Clone)]
pub struct JjCommand {
    #[command(subcommand)]
    pub action: Option<JjAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum JjAction {
    /// Initialize jj in the repo (colocated with git when possible).
    Init {
        /// Optional path to initialize (defaults to current directory).
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Show jj status.
    Status,
    /// Fetch from git remotes.
    Fetch,
    /// Rebase current change onto a destination.
    Rebase(JjRebaseOpts),
    /// Push bookmarks to git.
    Push(JjPushOpts),
    /// Fetch, rebase, then push a bookmark.
    Sync(JjSyncOpts),
    /// Manage workspaces.
    #[command(subcommand)]
    Workspace(JjWorkspaceAction),
    /// Manage bookmarks.
    #[command(subcommand)]
    Bookmark(JjBookmarkAction),
}

#[derive(Args, Debug, Clone)]
pub struct JjRebaseOpts {
    /// Destination to rebase onto (default: jj.default_branch or main/master).
    #[arg(long)]
    pub dest: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct JjPushOpts {
    /// Bookmark to push.
    #[arg(long)]
    pub bookmark: Option<String>,
    /// Push all bookmarks.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug, Clone)]
pub struct JjSyncOpts {
    /// Bookmark to push after rebase (optional).
    #[arg(long)]
    pub bookmark: Option<String>,
    /// Destination to rebase onto (default: jj.default_branch or main/master).
    #[arg(long)]
    pub dest: Option<String>,
    /// Remote to sync with (default: jj.remote or origin).
    #[arg(long)]
    pub remote: Option<String>,
    /// Skip pushing after rebase.
    #[arg(long)]
    pub no_push: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum JjWorkspaceAction {
    /// List workspaces.
    List,
    /// Add a workspace.
    Add {
        /// Workspace name.
        name: String,
        /// Optional path for workspace directory.
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum JjBookmarkAction {
    /// List bookmarks.
    List,
    /// Track a bookmark from a remote.
    Track {
        /// Bookmark name.
        name: String,
        /// Remote name (default: jj.remote or origin).
        #[arg(long)]
        remote: Option<String>,
    },
    /// Create a bookmark at a revision.
    Create {
        /// Bookmark name.
        name: String,
        /// Revision to attach to (default: @).
        #[arg(long)]
        rev: Option<String>,
        /// Whether to track the remote bookmark (default: jj.auto_track).
        #[arg(long)]
        track: Option<bool>,
        /// Remote to track (default: jj.remote or origin).
        #[arg(long)]
        remote: Option<String>,
    },
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
pub struct FixOpts {
    /// Description of what to fix.
    #[arg(value_name = "MESSAGE", trailing_var_arg = true)]
    pub message: Vec<String>,
    /// Skip unrolling the last commit.
    #[arg(long)]
    pub no_unroll: bool,
    /// Stash local changes before unrolling, then restore after.
    #[arg(long)]
    pub stash: bool,
    /// Hive agent name to run (default: shell).
    #[arg(long, default_value = "shell")]
    pub agent: String,
    /// Skip running Hive agent (only unroll).
    #[arg(long)]
    pub no_agent: bool,
}

#[derive(Args, Debug, Clone)]
pub struct UndoCommand {
    #[command(subcommand)]
    pub action: Option<UndoAction>,
    /// Dry run - show what would be undone without doing it.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// Force undo even if it requires force push.
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum UndoAction {
    /// Show the last undoable action.
    Show,
    /// List recent undoable actions.
    List {
        /// Maximum number of actions to show.
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
}

#[derive(Args, Debug, Clone)]
pub struct ChangesCommand {
    #[command(subcommand)]
    pub action: Option<ChangesAction>,
}

#[derive(Args, Debug, Clone)]
pub struct DiffCommand {
    /// Hash to unroll. When omitted, creates a new diff bundle.
    pub hash: Option<String>,
    /// Include specific env vars from local personal env store.
    /// Examples: --env CEREBRAS_API_KEY --env CEREBRAS_MODEL
    ///           --env CEREBRAS_API_KEY,CEREBRAS_MODEL
    ///           --env='[\"CEREBRAS_API_KEY\",\"CEREBRAS_MODEL\"]'
    #[arg(long, value_name = "KEY", action = clap::ArgAction::Append)]
    pub env: Vec<String>,
}

#[derive(Args, Debug, Clone)]
pub struct HashOpts {
    /// Arguments passed to unhash (paths or session flags).
    #[arg(trailing_var_arg = true, required = true)]
    pub args: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ChangesAction {
    #[command(
        about = "Print the current git diff for sharing.",
        long_about = "Outputs git diff (including untracked files) so it can be applied elsewhere."
    )]
    CurrentDiff,
    #[command(
        about = "Apply a diff to the current repo.",
        long_about = "Accepts a diff string, a file path, or '-' to read from stdin."
    )]
    Accept {
        /// Diff content, '-' for stdin, or a path to a diff file.
        diff: Option<String>,
        /// Read diff from a file path.
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
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
    Status {
        /// Optional daemon name to filter status output.
        name: Option<String>,
    },
    /// List available daemons.
    #[command(alias = "ls")]
    List,
}

#[derive(Args, Debug, Clone)]
pub struct SupervisorCommand {
    #[command(subcommand)]
    pub action: Option<SupervisorAction>,
    /// Socket path for supervisor IPC (defaults to ~/.config/flow/supervisor.sock).
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SupervisorAction {
    /// Start the supervisor in the background.
    Start {
        /// Start boot daemons in addition to autostart daemons.
        #[arg(long)]
        boot: bool,
    },
    /// Run the supervisor in the foreground (blocking).
    Run {
        /// Start boot daemons in addition to autostart daemons.
        #[arg(long)]
        boot: bool,
    },
    /// Install a macOS LaunchAgent to keep the supervisor running.
    Install {
        /// Start boot daemons in addition to autostart daemons.
        #[arg(long)]
        boot: bool,
    },
    /// Remove the macOS LaunchAgent for the supervisor.
    Uninstall,
    /// Stop the supervisor if running.
    Stop,
    /// Show supervisor status.
    Status,
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
    /// Claude Code: continue last session or start new one.
    Claude {
        #[command(subcommand)]
        action: Option<ProviderAiAction>,
    },
    /// Codex: continue last session or start new one.
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
    /// Copy last Claude session to clipboard. Optionally search for a session containing text.
    #[command(name = "copy-claude", alias = "cc")]
    CopyClaude {
        /// Search for a session containing this text.
        #[arg(value_name = "SEARCH", trailing_var_arg = true)]
        search: Vec<String>,
    },
    /// Copy last Codex session to clipboard. Optionally search for a session containing text.
    #[command(name = "copy-codex", alias = "cx")]
    CopyCodex {
        /// Search for a session containing this text.
        #[arg(value_name = "SEARCH", trailing_var_arg = true)]
        search: Vec<String>,
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
    /// Start a new session (ignores existing sessions).
    New,
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

#[derive(Args, Debug, Clone)]
pub struct AuthOpts {
    /// Override API base URL for myflow (defaults to https://myflow.sh).
    #[arg(long)]
    pub api_url: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ServicesCommand {
    #[command(subcommand)]
    pub action: Option<ServicesAction>,
}

#[derive(Args, Debug, Clone)]
pub struct SshCommand {
    #[command(subcommand)]
    pub action: Option<SshAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum EnvAction {
    /// Sync project settings and set up autonomous agent workflow.
    Sync,
    /// Unlock env read access (Touch ID on macOS).
    Unlock,
    /// Create a new env token from available templates.
    New,
    /// Authenticate with cloud to fetch env vars.
    Login,
    /// Fetch env vars from cloud and write to .env.
    Pull {
        /// Environment to fetch (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Push local .env to cloud.
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
    /// Apply env vars from cloud to the configured Cloudflare worker.
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
    /// Set a personal env var (default backend).
    Set {
        /// KEY=VALUE pair to set.
        pair: String,
        /// Compatibility flag (ignored; set always targets personal env).
        #[arg(long)]
        personal: bool,
    },
    /// Delete personal env var(s).
    Delete {
        /// Key(s) to delete.
        keys: Vec<String>,
    },
    /// Manage project-scoped env vars.
    Project {
        #[command(subcommand)]
        action: ProjectEnvAction,
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
    /// Run a command with env vars injected from cloud.
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
    /// Manage service tokens for host deployments.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ServicesAction {
    /// Set up Stripe env vars with guided prompts.
    Stripe(StripeServiceOpts),
    /// List available service setup flows.
    #[command(alias = "ls")]
    List,
}

#[derive(Args, Debug, Clone)]
pub struct MacosCommand {
    #[command(subcommand)]
    pub action: Option<MacosAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MacosAction {
    /// List all launchd services.
    #[command(alias = "ls")]
    List(MacosListOpts),
    /// Show running non-Apple services.
    Status,
    /// Audit services with recommendations.
    Audit(MacosAuditOpts),
    /// Show detailed info about a service.
    Info(MacosInfoOpts),
    /// Disable a service.
    Disable(MacosDisableOpts),
    /// Enable a service.
    Enable(MacosEnableOpts),
    /// Disable known bloatware services.
    Clean(MacosCleanOpts),
}

#[derive(Args, Debug, Clone)]
pub struct MacosListOpts {
    /// Only show user agents.
    #[arg(long)]
    pub user: bool,
    /// Only show system agents/daemons.
    #[arg(long)]
    pub system: bool,
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct MacosAuditOpts {
    /// Output as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct MacosInfoOpts {
    /// Service identifier (e.g., com.google.keystone.agent).
    pub service: String,
}

#[derive(Args, Debug, Clone)]
pub struct MacosDisableOpts {
    /// Service identifier to disable.
    pub service: String,
    /// Skip confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct MacosEnableOpts {
    /// Service identifier to enable.
    pub service: String,
}

#[derive(Args, Debug, Clone)]
pub struct MacosCleanOpts {
    /// Only show what would be done.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip confirmation prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct StripeServiceOpts {
    /// Path to the project root (defaults to current directory).
    #[arg(short, long)]
    pub path: Option<PathBuf>,
    /// Environment to store vars in (dev, staging, production).
    #[arg(short, long)]
    pub environment: Option<String>,
    /// Stripe mode (test or live).
    #[arg(long, value_enum, default_value_t = StripeModeArg::Test)]
    pub mode: StripeModeArg,
    /// Prompt even if keys are already set.
    #[arg(long)]
    pub force: bool,
    /// Apply env vars to Cloudflare after setting them.
    #[arg(long, conflicts_with = "no_apply")]
    pub apply: bool,
    /// Skip applying env vars to Cloudflare.
    #[arg(long, conflicts_with = "apply")]
    pub no_apply: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum StripeModeArg {
    Test,
    Live,
}

#[derive(Subcommand, Debug, Clone)]
pub enum SshAction {
    /// Generate a new SSH keypair and store it in cloud personal env vars.
    Setup {
        /// Optional key name (default: "default").
        #[arg(long, default_value = "default")]
        name: String,
        /// Skip automatically unlocking the key after setup.
        #[arg(long)]
        no_unlock: bool,
    },
    /// Unlock the SSH key from cloud and load it into the Flow SSH agent.
    Unlock {
        /// Optional key name (default: "default").
        #[arg(long, default_value = "default")]
        name: String,
        /// TTL for ssh-agent in hours (default: 24).
        #[arg(long, default_value = "24")]
        ttl_hours: u64,
    },
    /// Show whether the Flow SSH agent and key are available.
    Status {
        /// Optional key name (default: "default").
        #[arg(long, default_value = "default")]
        name: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum TokenAction {
    /// Create a new service token for a project.
    Create {
        /// Token name (e.g., "pulse-production").
        #[arg(short, long)]
        name: Option<String>,
        /// Permissions: read, write, or admin.
        #[arg(short, long, default_value = "read")]
        permissions: String,
    },
    /// List service tokens.
    #[command(alias = "ls")]
    List,
    /// Revoke a service token.
    Revoke {
        /// Token name to revoke.
        name: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ProjectEnvAction {
    /// Set a project-scoped env var.
    Set {
        /// KEY=VALUE pair to set.
        pair: String,
        /// Environment (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// Delete project-scoped env var(s).
    Delete {
        /// Key(s) to delete.
        keys: Vec<String>,
        /// Environment (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
    /// List project env vars.
    #[command(alias = "ls")]
    List {
        /// Environment (dev, staging, production).
        #[arg(short, long, default_value = "production")]
        environment: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct TodoCommand {
    #[command(subcommand)]
    pub action: Option<TodoAction>,
}

#[derive(Args, Debug, Clone)]
pub struct ExtCommand {
    /// Path to the external directory to move.
    pub path: String,
}

#[derive(Subcommand, Debug, Clone)]
pub enum TodoAction {
    /// Open the project Bike file.
    Bike,
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
    /// Copy agent instructions to clipboard (fuzzy select).
    #[command(alias = "cp")]
    Copy {
        /// Optional agent name (fuzzy select if not provided).
        agent: Option<String>,
    },
    /// Switch agents.md profile (fuzzy select if not provided).
    Rules {
        /// Optional profile name (e.g., light).
        profile: Option<String>,
        /// Optional repo path (defaults to cwd).
        repo: Option<String>,
    },
}

/// Hive agent management.
#[derive(Args, Debug, Clone)]
pub struct HiveCommand {
    #[command(subcommand)]
    pub action: Option<HiveAction>,
    /// Run an agent directly (e.g., `f hive fish "wrap ls"`).
    #[arg(trailing_var_arg = true)]
    pub agent: Vec<String>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum HiveAction {
    /// List available hive agents.
    #[command(alias = "ls")]
    List,
    /// Run a hive agent with a prompt.
    Run {
        /// Agent name.
        agent: String,
        /// Prompt for the agent.
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
    },
    /// Create a new agent spec.
    New {
        /// Agent name.
        name: String,
        /// Create as global agent (default: project-local).
        #[arg(short, long)]
        global: bool,
    },
    /// Edit an agent spec file.
    Edit {
        /// Agent name (fuzzy select if not provided).
        agent: Option<String>,
    },
    /// Show an agent's spec.
    Show {
        /// Agent name.
        agent: String,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct PublishOpts {
    /// GitHub repository URL (e.g., https://github.com/org/repo or git@github.com:org/repo.git).
    #[arg(value_name = "URL")]
    pub url: Option<String>,
    /// Repository name (defaults to current folder name).
    #[arg(short, long)]
    pub name: Option<String>,
    /// Repository owner/org (GitHub) or owner (gitedit.dev).
    #[arg(long)]
    pub owner: Option<String>,
    /// Update existing origin remote to match the target repo (GitHub).
    #[arg(long)]
    pub set_origin: bool,
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
pub struct PublishCommand {
    #[command(subcommand)]
    pub action: Option<PublishAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum PublishAction {
    /// Publish to gitedit.dev.
    Gitedit(PublishOpts),
    /// Publish to GitHub.
    Github(PublishOpts),
}

#[derive(Args, Debug, Clone)]
pub struct ReposCommand {
    #[command(subcommand)]
    pub action: Option<ReposAction>,
}

#[derive(Args, Debug, Clone)]
pub struct CodeCommand {
    #[command(subcommand)]
    pub action: Option<CodeAction>,
    /// Root directory to scan (default: ~/code).
    #[arg(long, default_value = "~/code")]
    pub root: String,
}

#[derive(Subcommand, Debug, Clone)]
pub enum CodeAction {
    /// List git repos under ~/code.
    List,
    /// Create a new project from a template in ~/new/<name>.
    New(CodeNewOpts),
    /// Move a folder into ~/code/<relative-path> and migrate AI sessions.
    Migrate(CodeMigrateOpts),
    /// Move AI sessions when a project path changes.
    MoveSessions(CodeMoveSessionsOpts),
}

#[derive(Args, Debug, Clone)]
pub struct CodeNewOpts {
    /// Template name under ~/new (e.g., "docs").
    pub template: String,
    /// Destination folder name or relative path under the code root.
    pub name: String,
    /// Add the new path to .gitignore in the containing repo.
    #[arg(long)]
    pub ignored: bool,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CodeMigrateOpts {
    /// Source folder to migrate.
    pub from: String,
    /// Relative path under the code root (e.g., "flow/myflow").
    pub relative: String,
    /// Copy instead of move (keeps original).
    #[arg(long, short)]
    pub copy: bool,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip migrating Claude sessions.
    #[arg(long)]
    pub skip_claude: bool,
    /// Skip migrating Codex sessions.
    #[arg(long)]
    pub skip_codex: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CodeMoveSessionsOpts {
    /// Old project path.
    #[arg(long)]
    pub from: String,
    /// New project path.
    #[arg(long)]
    pub to: String,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip migrating Claude sessions.
    #[arg(long)]
    pub skip_claude: bool,
    /// Skip migrating Codex sessions.
    #[arg(long)]
    pub skip_codex: bool,
}

#[derive(Args, Debug, Clone)]
pub struct MigrateCommand {
    #[command(subcommand)]
    pub action: Option<MigrateAction>,
    /// Source path (defaults to current directory if only one path given).
    pub source: Option<String>,
    /// Target path (if source is given, this is the destination).
    pub target: Option<String>,
    /// Copy instead of move (keeps original).
    #[arg(long, short)]
    pub copy: bool,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip migrating Claude sessions.
    #[arg(long)]
    pub skip_claude: bool,
    /// Skip migrating Codex sessions.
    #[arg(long)]
    pub skip_codex: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum MigrateAction {
    /// Move or copy current folder to ~/code/<relative-path>.
    Code(MigrateCodeOpts),
}

#[derive(Args, Debug, Clone)]
pub struct MigrateCodeOpts {
    /// Relative path under ~/code (e.g., "flow/myflow").
    pub relative: String,
    /// Copy instead of move (keeps original).
    #[arg(long, short)]
    pub copy: bool,
    /// Show what would change without writing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip migrating Claude sessions.
    #[arg(long)]
    pub skip_claude: bool,
    /// Skip migrating Codex sessions.
    #[arg(long)]
    pub skip_codex: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ReposAction {
    /// Clone a repository into ~/repos/<owner>/<repo>.
    Clone(ReposCloneOpts),
    /// Create a GitHub repository from the current folder and push it.
    Create(PublishOpts),
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
pub struct SyncCommand {
    /// Use rebase instead of merge when pulling.
    #[arg(long, short)]
    pub rebase: bool,
    /// Skip pushing to origin.
    #[arg(long)]
    pub no_push: bool,
    /// Auto-stash uncommitted changes (default: true).
    #[arg(long, short, default_value = "true")]
    pub stash: bool,
    /// Stash local JJ commits to a bookmark before syncing (JJ-only).
    #[arg(long, default_value = "false")]
    pub stash_commits: bool,
    /// Allow sync/rebase even when commit queue is non-empty.
    #[arg(long)]
    pub allow_queue: bool,
    /// Create origin repo on GitHub if it doesn't exist.
    #[arg(long)]
    pub create_repo: bool,
    /// Auto-fix conflicts and errors using Claude (default: true).
    #[arg(long, short, default_value = "true", action = clap::ArgAction::Set)]
    pub fix: bool,
    /// Disable auto-fix (same as --fix=false).
    #[arg(long, overrides_with = "fix")]
    pub no_fix: bool,
    /// Maximum fix attempts before giving up.
    #[arg(long, default_value = "3")]
    pub max_fix_attempts: u32,
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
    /// Checkout local 'upstream' branch synced to upstream.
    Check,
    /// Full sync: pull upstream, merge to dev/main, push to origin.
    Sync {
        /// Skip pushing to origin.
        #[arg(long)]
        no_push: bool,
        /// Create origin repo on GitHub if it doesn't exist.
        #[arg(long)]
        create_repo: bool,
    },
    /// Open upstream repository URL in browser.
    Open,
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

#[derive(Args, Debug, Clone)]
pub struct CommitsCommand {
    #[command(subcommand)]
    pub action: Option<CommitsAction>,
    #[command(flatten)]
    pub opts: CommitsOpts,
}

#[derive(Subcommand, Debug, Clone)]
pub enum CommitsAction {
    /// List notable commits.
    Top,
    /// Mark a commit as notable.
    Mark {
        /// Commit hash (short or full).
        hash: String,
    },
    /// Remove a commit from notable list.
    Unmark {
        /// Commit hash (short or full).
        hash: String,
    },
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

#[derive(Args, Debug, Clone)]
pub struct ReleaseCommand {
    /// Path to the project flow config (flow.toml).
    #[arg(long, default_value = "flow.toml")]
    pub config: PathBuf,
    #[command(subcommand)]
    pub action: Option<ReleaseAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ReleaseAction {
    /// Run the configured release task.
    Task(ReleaseTaskOpts),
    /// Publish a release to a Flow registry.
    Registry(RegistryReleaseOpts),
    /// Manage GitHub releases.
    #[command(alias = "gh")]
    Github(GhReleaseCommand),
}

#[derive(Args, Debug, Clone, Default)]
pub struct ReleaseTaskOpts {
    /// Additional arguments passed to the release task command.
    #[arg(value_name = "ARGS", trailing_var_arg = true)]
    pub args: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct RegistryReleaseOpts {
    /// Version to publish (auto-detected if omitted).
    #[arg(long, short)]
    pub version: Option<String>,
    /// Registry base URL (overrides flow.toml).
    #[arg(long)]
    pub registry: Option<String>,
    /// Override package name for the registry.
    #[arg(long)]
    pub package: Option<String>,
    /// Override the binary name(s) to upload.
    #[arg(long, value_name = "BIN")]
    pub bin: Vec<String>,
    /// Skip building binaries before publishing.
    #[arg(long)]
    pub no_build: bool,
    /// Mark this version as latest in the registry.
    #[arg(long, conflicts_with = "no_latest")]
    pub latest: bool,
    /// Skip updating the latest pointer.
    #[arg(long, conflicts_with = "latest")]
    pub no_latest: bool,
    /// Dry run: show what would be published without publishing.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
}

#[derive(Args, Debug, Clone)]
pub struct InstallOpts {
    /// Package name to install (leave blank to search).
    pub name: Option<String>,
    /// Registry base URL (defaults to FLOW_REGISTRY_URL).
    #[arg(long)]
    pub registry: Option<String>,
    /// Install backend (auto tries registry, falls back to flox).
    #[arg(long, value_enum, default_value = "auto")]
    pub backend: InstallBackend,
    /// Version to install (defaults to latest).
    #[arg(long, short)]
    pub version: Option<String>,
    /// Binary name to install (defaults to the package name or manifest default).
    #[arg(long)]
    pub bin: Option<String>,
    /// Install directory (defaults to ~/bin).
    #[arg(long)]
    pub bin_dir: Option<PathBuf>,
    /// Skip checksum verification.
    #[arg(long)]
    pub no_verify: bool,
    /// Overwrite existing binary if present.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct InstallCommand {
    #[command(subcommand)]
    pub action: Option<InstallAction>,

    #[command(flatten)]
    pub opts: InstallOpts,
}

#[derive(Subcommand, Debug, Clone)]
pub enum InstallAction {
    /// Index flox packages into Typesense.
    Index(InstallIndexOpts),
}

#[derive(Args, Debug, Clone)]
pub struct InstallIndexOpts {
    /// Search term to index (defaults to prompt).
    pub query: Option<String>,
    /// File with newline-separated search terms.
    #[arg(long)]
    pub queries: Option<PathBuf>,
    /// Typesense base URL (overrides FLOW_TYPESENSE_URL).
    #[arg(long)]
    pub url: Option<String>,
    /// Typesense API key (overrides FLOW_TYPESENSE_API_KEY).
    #[arg(long)]
    pub api_key: Option<String>,
    /// Typesense collection name (overrides FLOW_TYPESENSE_COLLECTION).
    #[arg(long, default_value = "flox-packages")]
    pub collection: String,
    /// Index server URL (defaults to local base server).
    #[arg(long, default_value = "http://127.0.0.1:9417")]
    pub server: String,
    /// Skip index server and write directly to Typesense.
    #[arg(long)]
    pub direct: bool,
    /// Max results per search term.
    #[arg(long, default_value_t = 200)]
    pub per_page: usize,
    /// Dry run (do not write to Typesense).
    #[arg(long, short = 'n')]
    pub dry_run: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum InstallBackend {
    Auto,
    Registry,
    Flox,
}

#[derive(Args, Debug, Clone)]
pub struct FishInstallOpts {
    /// Path to fish-shell source repo (auto-detected if not set).
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Install directory for the fish binary (defaults to ~/.local/bin).
    #[arg(long)]
    pub bin_dir: Option<PathBuf>,
    /// Force reinstall even if already installed.
    #[arg(long)]
    pub force: bool,
    /// Skip confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct RegistryCommand {
    #[command(subcommand)]
    pub action: Option<RegistryAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum RegistryAction {
    /// Create a registry token and configure worker + env.
    Init(RegistryInitOpts),
}

#[derive(Args, Debug, Clone)]
pub struct RegistryInitOpts {
    /// Path to the worker project (defaults to packages/worker).
    #[arg(long, short)]
    pub worker: Option<PathBuf>,
    /// Registry base URL (overrides flow.toml or FLOW_REGISTRY_URL).
    #[arg(long)]
    pub registry: Option<String>,
    /// Env var name for the registry token.
    #[arg(long)]
    pub token_env: Option<String>,
    /// Provide an explicit token instead of generating one.
    #[arg(long)]
    pub token: Option<String>,
    /// Skip updating the worker secret.
    #[arg(long)]
    pub no_worker: bool,
    /// Print the generated token to stdout.
    #[arg(long)]
    pub show_token: bool,
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
    /// Deploy the web site (Cloudflare).
    Web,
    /// Interactive deploy setup (Cloudflare Workers for now).
    Setup,
    /// Deploy to Railway.
    Railway,
    /// Configure deployment defaults (Linux host).
    Config,
    /// Run the project's release task.
    Release(ReleaseOpts),
    /// Show deployment status.
    Status,
    /// View deployment logs.
    Logs {
        /// Follow logs in real-time.
        #[arg(long, short)]
        follow: bool,
        /// Show logs since the last successful deploy (default).
        #[arg(long, default_value_t = true)]
        since_deploy: bool,
        /// Show full log history (ignores --since-deploy).
        #[arg(long)]
        all: bool,
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

#[derive(Args, Debug, Clone)]
pub struct UpgradeOpts {
    /// Upgrade to a specific version (e.g., "0.2.0" or "v0.2.0").
    #[arg(value_name = "VERSION")]
    pub version: Option<String>,
    /// Print what would happen without making changes.
    #[arg(long, short = 'n')]
    pub dry_run: bool,
    /// Force upgrade even if already on the latest version.
    #[arg(long, short)]
    pub force: bool,
    /// Download to a specific path instead of replacing the current executable.
    #[arg(long, short)]
    pub output: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct GhReleaseCommand {
    #[command(subcommand)]
    pub action: Option<GhReleaseAction>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum GhReleaseAction {
    /// Create a new GitHub release.
    Create(GhReleaseCreateOpts),
    /// List recent releases.
    #[command(alias = "ls")]
    List {
        /// Number of releases to show.
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
    /// Delete a release.
    Delete {
        /// Release tag to delete.
        tag: String,
        /// Skip confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Download release assets.
    Download {
        /// Release tag (defaults to latest).
        #[arg(short, long)]
        tag: Option<String>,
        /// Output directory.
        #[arg(short, long, default_value = ".")]
        output: String,
    },
}

#[derive(Args, Debug, Clone)]
pub struct GhReleaseCreateOpts {
    /// Version tag (e.g., "v0.1.0"). Auto-detected from Cargo.toml if not provided.
    #[arg(value_name = "TAG")]
    pub tag: Option<String>,
    /// Release title (defaults to tag name).
    #[arg(short, long)]
    pub title: Option<String>,
    /// Release notes (reads from stdin or file if not provided).
    #[arg(short, long)]
    pub notes: Option<String>,
    /// Read release notes from a file.
    #[arg(long)]
    pub notes_file: Option<String>,
    /// Generate release notes automatically from commits.
    #[arg(long)]
    pub generate_notes: bool,
    /// Create as draft release.
    #[arg(long)]
    pub draft: bool,
    /// Mark as prerelease.
    #[arg(long)]
    pub prerelease: bool,
    /// Asset files to upload (can be specified multiple times).
    #[arg(short, long, value_name = "FILE")]
    pub asset: Vec<String>,
    /// Target commit/branch for the release tag.
    #[arg(long)]
    pub target: Option<String>,
    /// Skip confirmation prompts.
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum DocsAction {
    /// Create a docs/ folder with starter markdown files.
    New(DocsNewOpts),
    /// Run the docs hub that aggregates docs from ~/code and ~/org.
    Hub(DocsHubOpts),
    /// Deploy the docs hub to Cloudflare Pages.
    Deploy(DocsDeployOpts),
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

#[derive(Args, Debug, Clone)]
pub struct DocsNewOpts {
    /// Path to create docs in (defaults to current directory).
    #[arg(long)]
    pub path: Option<PathBuf>,
    /// Overwrite if docs/ already exists.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DocsDeployOpts {
    /// Cloudflare Pages project name (defaults to flow.toml name).
    #[arg(long)]
    pub project: Option<String>,
    /// Custom domain to attach (optional).
    #[arg(long)]
    pub domain: Option<String>,
    /// Skip confirmation prompts.
    #[arg(short, long)]
    pub yes: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DocsHubOpts {
    /// Host to bind the docs hub to.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Port for the docs hub.
    #[arg(long, default_value_t = 4410)]
    pub port: u16,
    /// Docs hub root (defaults to ~/.config/flow/docs-hub).
    #[arg(long, default_value = "~/.config/flow/docs-hub")]
    pub hub_root: String,
    /// Template root (defaults to ~/new/docs).
    #[arg(long, default_value = "~/new/docs")]
    pub template_root: String,
    /// Code root to scan for docs (defaults to ~/code).
    #[arg(long, default_value = "~/code")]
    pub code_root: String,
    /// Org root to scan for docs (defaults to ~/org).
    #[arg(long, default_value = "~/org")]
    pub org_root: String,
    /// Skip scanning for .ai/docs.
    #[arg(long)]
    pub no_ai: bool,
    /// Skip opening the browser.
    #[arg(long)]
    pub no_open: bool,
    /// Sync content and exit without running the dev server.
    #[arg(long)]
    pub sync_only: bool,
}
