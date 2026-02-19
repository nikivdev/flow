use std::{
    collections::{BTreeMap, hash_map::DefaultHasher},
    env,
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{IsTerminal, Read, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::json;
use shell_words;
use which::which;

use crate::{
    ai_taskd, ai_tasks,
    cli::{
        FastRunOpts, GlobalAction, GlobalCommand, HubAction, HubCommand, HubOpts, TaskActivateOpts,
        TaskRunOpts, TasksAction, TasksBuildAiOpts, TasksCommand, TasksDaemonAction,
        TasksDaemonCommand, TasksDupesOpts, TasksInitAiOpts, TasksListOpts, TasksOpts,
        TasksRunAiOpts,
    },
    config::{self, Config, FloxInstallSpec, TaskConfig},
    discover,
    flox::{self, FloxEnv},
    history::{self, InvocationRecord},
    hub, init, jazz_state, projects,
    running::{self, RunningProcess},
    secret_redact, task_failure_agents, task_match,
};

/// Fire-and-forget log ingester that batches output lines and POSTs them to the
/// Flow daemon's `/logs/ingest` endpoint on a background thread.
struct LogIngester {
    tx: std::sync::mpsc::Sender<String>,
}

impl LogIngester {
    fn new(project: &str, service: &str) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let project = project.to_string();
        let service = service.to_string();
        thread::spawn(move || {
            let client = match Client::builder().timeout(Duration::from_secs(2)).build() {
                Ok(c) => c,
                Err(_) => return,
            };
            let mut batch: Vec<serde_json::Value> = Vec::new();
            let flush_interval = Duration::from_millis(500);
            let mut last_flush = Instant::now();

            loop {
                match rx.recv_timeout(flush_interval) {
                    Ok(line) => {
                        batch.push(json!({
                            "project": project,
                            "content": line,
                            "timestamp": running::now_ms() as i64,
                            "type": "log",
                            "service": service,
                            "format": "text",
                        }));
                        // Flush if batch is large enough or interval has passed
                        if batch.len() >= 50 || last_flush.elapsed() >= flush_interval {
                            let _ = client
                                .post("http://127.0.0.1:9050/logs/ingest")
                                .json(&batch)
                                .send();
                            batch.clear();
                            last_flush = Instant::now();
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if !batch.is_empty() {
                            let _ = client
                                .post("http://127.0.0.1:9050/logs/ingest")
                                .json(&batch)
                                .send();
                            batch.clear();
                            last_flush = Instant::now();
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        if !batch.is_empty() {
                            let _ = client
                                .post("http://127.0.0.1:9050/logs/ingest")
                                .json(&batch)
                                .send();
                        }
                        break;
                    }
                }
            }
        });
        Self { tx }
    }

    fn send(&self, line: &str) {
        let _ = self.tx.send(secret_redact::redact_text(line));
    }
}

/// Global state for cancel cleanup handler.
static CANCEL_HANDLER_SET: AtomicBool = AtomicBool::new(false);
static FISHX_WARNED: AtomicBool = AtomicBool::new(false);

/// Cleanup state shared with the signal handler.
struct CleanupState {
    command: Option<String>,
    workdir: PathBuf,
    pid: Option<u32>,
    pgid: Option<u32>,
}

static CLEANUP_STATE: std::sync::OnceLock<Mutex<CleanupState>> = std::sync::OnceLock::new();

/// Run the cleanup command if one is set.
fn run_cleanup() {
    let state = CLEANUP_STATE.get_or_init(|| {
        Mutex::new(CleanupState {
            command: None,
            workdir: PathBuf::from("."),
            pid: None,
            pgid: None,
        })
    });

    if let Ok(guard) = state.lock() {
        terminate_tracked_process(&guard);
        if let Some(ref cmd) = guard.command {
            eprintln!("\nRunning cleanup: {}", cmd);
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(&guard.workdir)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status();
        }
    }
}

/// Set up the cleanup handler for Ctrl+C.
fn setup_cancel_handler(on_cancel: Option<&str>, workdir: &Path) {
    let state = CLEANUP_STATE.get_or_init(|| {
        Mutex::new(CleanupState {
            command: None,
            workdir: PathBuf::from("."),
            pid: None,
            pgid: None,
        })
    });

    // Update the cleanup state
    if let Ok(mut guard) = state.lock() {
        guard.command = on_cancel.map(|s| s.to_string());
        guard.workdir = workdir.to_path_buf();
        guard.pid = None;
        guard.pgid = None;
    }

    // Only set up the handler once
    if !CANCEL_HANDLER_SET.swap(true, Ordering::SeqCst) {
        let _ = ctrlc::set_handler(move || {
            run_cleanup();
            std::process::exit(130); // 128 + SIGINT (2)
        });
    }
}

/// Clear the cleanup handler (called after task completes normally).
fn clear_cancel_handler() {
    let state = CLEANUP_STATE.get_or_init(|| {
        Mutex::new(CleanupState {
            command: None,
            workdir: PathBuf::from("."),
            pid: None,
            pgid: None,
        })
    });

    if let Ok(mut guard) = state.lock() {
        guard.command = None;
        guard.pid = None;
        guard.pgid = None;
    }
}

fn set_cleanup_process(pid: u32, pgid: u32) {
    let state = CLEANUP_STATE.get_or_init(|| {
        Mutex::new(CleanupState {
            command: None,
            workdir: PathBuf::from("."),
            pid: None,
            pgid: None,
        })
    });

    if let Ok(mut guard) = state.lock() {
        guard.pid = Some(pid);
        guard.pgid = Some(pgid);
    }
}

fn terminate_tracked_process(state: &CleanupState) {
    #[cfg(unix)]
    {
        let self_pgid = running::get_pgid(std::process::id()).unwrap_or(0);
        if let Some(pgid) = state.pgid {
            if pgid != 0 && pgid != self_pgid {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(format!("-{}", pgid))
                    .status();
                return;
            }
        }

        if let Some(pid) = state.pid {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .status();
        }
    }

    #[cfg(windows)]
    {
        if let Some(pid) = state.pid {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .status();
        }
    }
}

/// Context for registering a running task process
#[derive(Debug, Clone)]
pub struct TaskContext {
    pub task_name: String,
    pub command: String,
    pub config_path: PathBuf,
    pub project_root: PathBuf,
    pub used_flox: bool,
    pub project_name: Option<String>,
    pub log_path: Option<PathBuf>,
    pub interactive: bool,
}

/// Check if a command needs interactive mode (TTY passthrough).
/// Auto-detects commands that typically require user input.
fn needs_interactive_mode(command: &str) -> bool {
    // Check each line of the command (for multi-line scripts)
    for line in command.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Commands that need interactive mode when they start a line
        let interactive_prefixes = [
            "sudo ",
            "sudo\t",
            "su ",
            "ssh ",
            "docker run -it",
            "docker run -ti",
            "docker exec -it",
            "docker exec -ti",
            "kubectl exec -it",
            "kubectl exec -ti",
        ];

        for prefix in &interactive_prefixes {
            if line.starts_with(prefix) {
                return true;
            }
        }

        // Also check if line is exactly "sudo" followed by something
        if line == "sudo" || line.starts_with("sudo ") {
            return true;
        }
    }

    // Check for sudo anywhere in piped/chained commands
    if command.contains("| sudo") || command.contains("&& sudo") || command.contains("; sudo") {
        return true;
    }

    // Standalone interactive commands (check first line's first word)
    let interactive_commands = [
        "vim",
        "nvim",
        "nano",
        "emacs",
        "htop",
        "top",
        "btop",
        "less",
        "more",
        "psql",
        "mysql",
        "sqlite3",
        "node",
        "python",
        "python3",
        "irb",
        "ghci",
        "lazygit",
        "lazydocker",
        // Package managers can have interactive prompts (corepack, license confirmations, etc.)
        "pnpm",
        "npm",
        "yarn",
        "bun",
    ];

    let first_line = command.lines().next().unwrap_or("").trim();
    let first_word = first_line.split_whitespace().next().unwrap_or("");
    let base_cmd = first_word.rsplit('/').next().unwrap_or(first_word);

    interactive_commands.contains(&base_cmd)
}

/// Handle `f tasks` command: fuzzy search history or list tasks.
pub fn run_tasks_command(cmd: TasksCommand) -> Result<()> {
    match cmd.action {
        Some(TasksAction::List(opts)) => list_tasks(opts),
        Some(TasksAction::Dupes(opts)) => list_task_duplicates(opts),
        Some(TasksAction::InitAi(opts)) => init_ai_tasks(opts),
        Some(TasksAction::BuildAi(opts)) => build_ai_task(opts),
        Some(TasksAction::RunAi(opts)) => run_ai_task(opts),
        Some(TasksAction::Daemon(cmd)) => run_ai_task_daemon_command(cmd),
        None => fuzzy_search_task_history(),
    }
}

pub fn run_fast(opts: FastRunOpts) -> Result<()> {
    let root = resolve_ai_root(&opts.root)?;
    let selector = opts.name.trim();
    if !selector.to_ascii_lowercase().starts_with("ai:") {
        bail!(
            "f fast expects an AI task selector (for example: ai:flow/dev-check), got '{}'",
            opts.name
        );
    }

    if let Some(()) = run_via_fast_client(&root, selector, &opts.args, opts.no_cache)? {
        return Ok(());
    }

    run_via_daemon_with_lazy_start(&root, selector, &opts.args, opts.no_cache)
}

fn run_ai_task_daemon_command(cmd: TasksDaemonCommand) -> Result<()> {
    match cmd.action {
        TasksDaemonAction::Start => ai_taskd::start(),
        TasksDaemonAction::Stop => ai_taskd::stop(),
        TasksDaemonAction::Status => ai_taskd::status(),
        TasksDaemonAction::Serve => ai_taskd::serve(),
    }
}

fn build_ai_task(opts: TasksBuildAiOpts) -> Result<()> {
    let root = resolve_ai_root(&opts.root)?;
    let ai_discovery = ai_tasks::discover_tasks(&root)?;
    let task = ai_tasks::select_task(&ai_discovery, &opts.name)?
        .with_context(|| format!("AI task '{}' not found in {}", opts.name, root.display()))?;
    let artifact = ai_tasks::build_task_cached(task, &root, opts.force)?;
    println!(
        "ai task cached: {}\n  key: {}\n  binary: {}\n  rebuilt: {}",
        task.id,
        artifact.cache_key,
        artifact.binary_path.display(),
        artifact.rebuilt
    );
    Ok(())
}

fn run_ai_task(opts: TasksRunAiOpts) -> Result<()> {
    let root = resolve_ai_root(&opts.root)?;
    let mut policy = AiTaskExecutionPolicy::from_env();
    if opts.daemon {
        policy.use_daemon = true;
    }
    if opts.no_cache {
        policy.no_cache = true;
    }
    if !execute_ai_task_by_selector(&root, &opts.name, &opts.args, &policy)? {
        bail!("AI task '{}' not found in {}", opts.name, root.display());
    }
    Ok(())
}

fn resolve_ai_root(root: &Path) -> Result<PathBuf> {
    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()?.join(root)
    };
    Ok(root.canonicalize().unwrap_or(root))
}

#[derive(Debug, Clone, Copy)]
struct AiTaskExecutionPolicy {
    use_daemon: bool,
    no_cache: bool,
}

impl AiTaskExecutionPolicy {
    fn from_env() -> Self {
        let runtime = std::env::var("FLOW_AI_TASK_RUNTIME")
            .ok()
            .unwrap_or_default()
            .to_ascii_lowercase();
        Self {
            use_daemon: env_flag_is_true("FLOW_AI_TASK_DAEMON"),
            no_cache: runtime == "moon-run" || runtime == "moon",
        }
    }
}

fn env_flag_is_true(name: &str) -> bool {
    match std::env::var(name) {
        Ok(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn should_prefer_fast_client(root: &Path, selector: &str) -> bool {
    if !env_flag_is_true("FLOW_AI_TASK_FAST_CLIENT") {
        return false;
    }
    if !selector.trim().to_ascii_lowercase().starts_with("ai:") {
        return false;
    }

    if let Ok(raw) = std::env::var("FLOW_AI_TASK_FAST_SELECTORS")
        && selector_matches_patterns(selector, &raw)
    {
        return true;
    }

    if let Ok(Some(task)) = ai_tasks::resolve_task_fast(root, selector) {
        return task.tags.iter().any(|tag| {
            matches!(
                tag.trim().to_ascii_lowercase().as_str(),
                "fast" | "latency" | "hot" | "hotkey"
            )
        });
    }

    false
}

fn selector_matches_patterns(selector: &str, patterns_csv: &str) -> bool {
    let selector = selector.trim();
    for raw in patterns_csv.split(',') {
        let p = raw.trim();
        if p.is_empty() {
            continue;
        }
        if p == "*" {
            return true;
        }
        if p.starts_with('*') && p.ends_with('*') && p.len() >= 3 {
            let needle = &p[1..p.len() - 1];
            if selector.contains(needle) {
                return true;
            }
            continue;
        }
        if let Some(prefix) = p.strip_suffix('*') {
            if selector.starts_with(prefix) {
                return true;
            }
            continue;
        }
        if let Some(suffix) = p.strip_prefix('*') {
            if selector.ends_with(suffix) {
                return true;
            }
            continue;
        }
        if selector.eq_ignore_ascii_case(p) {
            return true;
        }
    }
    false
}

fn fast_client_binary_path(root: &Path) -> Option<PathBuf> {
    if let Ok(raw) = std::env::var("FLOW_AI_TASK_FAST_CLIENT_BIN") {
        let p = PathBuf::from(raw.trim());
        if p.is_file() {
            return Some(p);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let fai = home.join(".local").join("bin").join("fai");
        if fai.is_file() {
            return Some(fai);
        }
    }

    let release_local = root.join("target").join("release").join("ai-taskd-client");
    if release_local.is_file() {
        return Some(release_local);
    }

    let debug_local = root.join("target").join("debug").join("ai-taskd-client");
    if debug_local.is_file() {
        return Some(debug_local);
    }

    which("ai-taskd-client").ok()
}

fn run_via_fast_client(
    root: &Path,
    selector: &str,
    args: &[String],
    no_cache: bool,
) -> Result<Option<()>> {
    let Some(bin) = fast_client_binary_path(root) else {
        return Ok(None);
    };

    fn invoke(
        bin: &Path,
        root: &Path,
        selector: &str,
        args: &[String],
        no_cache: bool,
    ) -> Result<std::process::Output> {
        let mut cmd = Command::new(bin);
        cmd.arg("--root").arg(root);
        if no_cache {
            cmd.arg("--no-cache");
        }
        cmd.arg(selector);
        if !args.is_empty() {
            cmd.arg("--");
            cmd.args(args);
        }
        cmd.output().with_context(|| {
            format!(
                "failed to run fast ai client '{}' for selector '{}'",
                bin.display(),
                selector
            )
        })
    }

    let mut output = invoke(&bin, root, selector, args, no_cache)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        let unavailable = stderr.contains("failed to connect")
            || stderr.contains("connection refused")
            || stderr.contains("no such file or directory");
        if unavailable {
            ai_taskd::start()?;
            output = invoke(&bin, root, selector, args, no_cache)?;
        }
    }

    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }

    if output.status.success() {
        Ok(Some(()))
    } else {
        let code = output.status.code().unwrap_or(1);
        bail!("fast ai client failed for '{}': exit {}", selector, code);
    }
}

fn execute_ai_task_by_selector(
    root: &Path,
    selector: &str,
    args: &[String],
    policy: &AiTaskExecutionPolicy,
) -> Result<bool> {
    if policy.use_daemon {
        if should_prefer_fast_client(root, selector)
            && run_via_fast_client(root, selector, args, policy.no_cache)?.is_some()
        {
            return Ok(true);
        }

        match run_via_daemon_with_lazy_start(root, selector, args, policy.no_cache) {
            Ok(()) => return Ok(true),
            Err(error) => {
                let msg = format!("{error:#}").to_ascii_lowercase();
                if msg.contains("not found") {
                    return Ok(false);
                }
                return Err(error);
            }
        }
    }

    if let Some(ai_task) = ai_tasks::resolve_task_fast(root, selector)? {
        execute_ai_task(root, &ai_task.id, &ai_task, args, policy)?;
        return Ok(true);
    }

    let ai_discovery = ai_tasks::discover_tasks(root)?;
    let Some(ai_task) = ai_tasks::select_task(&ai_discovery, selector)? else {
        return Ok(false);
    };

    execute_ai_task(root, &ai_task.id, ai_task, args, policy)?;
    Ok(true)
}

fn execute_ai_task(
    root: &Path,
    selector: &str,
    task: &ai_tasks::DiscoveredAiTask,
    args: &[String],
    policy: &AiTaskExecutionPolicy,
) -> Result<()> {
    if policy.use_daemon {
        return run_via_daemon_with_lazy_start(root, selector, args, policy.no_cache);
    }

    if policy.no_cache {
        ai_tasks::run_task_via_moon(task, root, args)
    } else {
        // Auto runtime policy currently resolves to cache-first with safe moon-run fallback.
        ai_tasks::run_task(task, root, args)
    }
}

fn run_via_daemon_with_lazy_start(
    root: &Path,
    selector: &str,
    args: &[String],
    no_cache: bool,
) -> Result<()> {
    match ai_taskd::run_via_daemon(root, selector, args, no_cache) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            let msg = format!("{first_error:#}").to_ascii_lowercase();
            let daemon_unavailable = msg.contains("failed to connect to ai-taskd")
                || msg.contains("connection refused")
                || msg.contains("no such file or directory");
            if !daemon_unavailable {
                return Err(first_error);
            }
            ai_taskd::start()?;
            ai_taskd::run_via_daemon(root, selector, args, no_cache)
        }
    }
}

/// Fuzzy search through task history (most recent first).
fn fuzzy_search_task_history() -> Result<()> {
    let records = history::load_all_records()?;

    if records.is_empty() {
        println!("No task history found.");
        return Ok(());
    }

    // Dedupe by task_name + project_root, keeping most recent
    let mut seen = std::collections::HashSet::new();
    let mut unique_records = Vec::new();
    for rec in records {
        let key = format!("{}:{}", rec.project_root, rec.task_name);
        if seen.insert(key) {
            unique_records.push(rec);
        }
    }

    if unique_records.is_empty() {
        println!("No task history found.");
        return Ok(());
    }

    // Format for fzf: "task_name  project_path"
    let lines: Vec<String> = unique_records
        .iter()
        .map(|r| {
            let project = r
                .project_root
                .strip_prefix(
                    &dirs::home_dir()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                )
                .map(|p| format!("~{}", p))
                .unwrap_or_else(|| r.project_root.clone());
            format!("{}\t{}", r.task_name, project)
        })
        .collect();

    let input = lines.join("\n");

    // Run fzf
    let mut fzf = Command::new("fzf")
        .args([
            "--height=50%",
            "--reverse",
            "--prompt=Task: ",
            "--delimiter=\t",
            "--with-nth=1,2",
            "--tabstop=4",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    fzf.stdin.as_mut().unwrap().write_all(input.as_bytes())?;

    let output = fzf.wait_with_output()?;
    if !output.status.success() {
        return Ok(()); // User cancelled
    }

    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        return Ok(());
    }

    // Parse selection: "task_name\tproject_path"
    let parts: Vec<&str> = selected.split('\t').collect();
    if parts.is_empty() {
        return Ok(());
    }

    let task_name = parts[0].trim();
    let project_path = if parts.len() > 1 {
        let p = parts[1].trim();
        if p.starts_with("~/") {
            dirs::home_dir()
                .unwrap_or_default()
                .join(&p[2..])
                .to_string_lossy()
                .to_string()
        } else {
            p.to_string()
        }
    } else {
        std::env::current_dir()?.to_string_lossy().to_string()
    };

    // Run the task in that project
    println!("Running '{}' in {}", task_name, project_path);
    let project_root = PathBuf::from(&project_path);
    let config_path = project_root.join("flow.toml");

    run(TaskRunOpts {
        config: config_path,
        delegate_to_hub: false,
        hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
        hub_port: 9050,
        name: task_name.to_string(),
        args: vec![],
    })
}

/// List tasks from flow.toml (moved from `f tasks` to `f tasks list`).
fn list_tasks(opts: TasksListOpts) -> Result<()> {
    // Determine root directory for discovery
    let mut root = if opts.config.is_absolute() {
        opts.config
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    if is_default_flow_config(&opts.config) && !root.join("flow.toml").exists() {
        if let Some(found) = find_flow_toml_upwards(&root) {
            root = found.parent().unwrap_or(&root).to_path_buf();
        }
    }

    let discovery = discover::discover_tasks(&root)?;
    let ai_discovery = ai_tasks::discover_tasks(&root)?;
    if opts.dupes {
        return print_duplicate_tasks(&discovery.tasks);
    }

    if discovery.tasks.is_empty() && ai_discovery.is_empty() {
        println!("No tasks defined in {} or subdirectories", root.display());
        return Ok(());
    }

    println!("Tasks (root: {}):", root.display());
    for line in format_discovered_task_lines(&discovery.tasks, &ai_discovery) {
        println!("{line}");
    }

    Ok(())
}

fn list_task_duplicates(opts: TasksDupesOpts) -> Result<()> {
    let mut root = if opts.config.is_absolute() {
        opts.config
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    if is_default_flow_config(&opts.config) && !root.join("flow.toml").exists() {
        if let Some(found) = find_flow_toml_upwards(&root) {
            root = found.parent().unwrap_or(&root).to_path_buf();
        }
    }
    let discovery = discover::discover_tasks(&root)?;
    print_duplicate_tasks(&discovery.tasks)
}

fn init_ai_tasks(opts: TasksInitAiOpts) -> Result<()> {
    let root = if opts.root.is_absolute() {
        opts.root
    } else {
        std::env::current_dir()?.join(opts.root)
    };
    let task_dir = root.join(".ai").join("tasks");
    std::fs::create_dir_all(&task_dir)
        .with_context(|| format!("failed to create {}", task_dir.display()))?;

    let starter_path = task_dir.join("starter.mbt");
    if starter_path.exists() && !opts.force {
        println!("AI task starter already exists: {}", starter_path.display());
        println!("Use --force to overwrite.");
        return Ok(());
    }

    std::fs::write(&starter_path, AI_TASK_STARTER)
        .with_context(|| format!("failed to write {}", starter_path.display()))?;
    println!("Created AI task starter: {}", starter_path.display());
    println!("Run it with: f starter");
    Ok(())
}

pub fn list(opts: TasksOpts) -> Result<()> {
    // Determine root directory for discovery
    let mut root = if opts.config.is_absolute() {
        opts.config
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };
    if is_default_flow_config(&opts.config) && !root.join("flow.toml").exists() {
        if let Some(found) = find_flow_toml_upwards(&root) {
            root = found.parent().unwrap_or(&root).to_path_buf();
        }
    }

    let discovery = discover::discover_tasks(&root)?;
    let ai_discovery = ai_tasks::discover_tasks(&root)?;

    if discovery.tasks.is_empty() && ai_discovery.is_empty() {
        println!("No tasks defined in {} or subdirectories", root.display());
        return Ok(());
    }

    println!("Tasks (root: {}):", root.display());
    for line in format_discovered_task_lines(&discovery.tasks, &ai_discovery) {
        println!("{line}");
    }

    Ok(())
}

/// Run tasks from the global flow config (~/.config/flow/flow.toml).
pub fn run_global(opts: GlobalCommand) -> Result<()> {
    let config_path = config::default_config_path();
    if !config_path.exists() {
        bail!("global flow config not found at {}", config_path.display());
    }

    if let Some(action) = opts.action {
        match action {
            GlobalAction::List => {
                return list(TasksOpts {
                    config: config_path,
                });
            }
            GlobalAction::Run { task, args } => {
                return run(TaskRunOpts {
                    config: config_path,
                    delegate_to_hub: false,
                    hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
                    hub_port: 9050,
                    name: task,
                    args,
                });
            }
            GlobalAction::Match(opts) => {
                return task_match::run_global(task_match::MatchOpts {
                    args: opts.query,
                    model: opts.model,
                    port: Some(opts.port),
                    execute: !opts.dry_run,
                });
            }
        }
    }

    if opts.list {
        return list(TasksOpts {
            config: config_path,
        });
    }

    if let Some(task) = opts.task {
        return run(TaskRunOpts {
            config: config_path,
            delegate_to_hub: false,
            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
            hub_port: 9050,
            name: task,
            args: opts.args,
        });
    }

    list(TasksOpts {
        config: config_path,
    })
}

/// Run a task, searching nested flow.toml files if not found in root.
pub fn run_with_discovery(task_name: &str, args: Vec<String>) -> Result<()> {
    let mut root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if !root.join("flow.toml").exists() {
        if let Some(found) = find_flow_toml_upwards(&root) {
            root = found.parent().unwrap_or(&root).to_path_buf();
        }
    }
    let discovery = discover::discover_tasks(&root)?;
    let ai_discovery = ai_tasks::discover_tasks(&root)?;
    if discovery.tasks.is_empty() && ai_discovery.is_empty() {
        bail!("No tasks defined in {} or subdirectories", root.display());
    }

    let discovered = select_discovered_task(&discovery, task_name)?;
    if let Some(discovered) = discovered {
        return run(TaskRunOpts {
            config: discovered.config_path.clone(),
            delegate_to_hub: false,
            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
            hub_port: 9050,
            name: discovered.task.name.clone(),
            args,
        });
    }

    let ai_policy = AiTaskExecutionPolicy::from_env();
    if execute_ai_task_by_selector(&root, task_name, &args, &ai_policy)? {
        return Ok(());
    }

    // List available tasks in error message
    let available: Vec<_> = discovery.tasks.iter().map(task_reference).collect();
    let mut available_all = available;
    available_all.extend(ai_discovery.iter().map(ai_tasks::task_reference));
    bail!(
        "task '{}' not found.\nAvailable tasks: {}",
        task_name,
        available_all.join(", ")
    );
}

fn select_discovered_task<'a>(
    discovery: &'a discover::DiscoveryResult,
    task_name: &str,
) -> Result<Option<&'a discover::DiscoveredTask>> {
    let mut scoped_not_found: Option<(String, String, Vec<String>)> = None;
    if let Some((scope, scoped_task)) = parse_scoped_selector(task_name) {
        let scope_exists = discovery.tasks.iter().any(|d| d.matches_scope(&scope));
        if scope_exists {
            let scoped_matches: Vec<&discover::DiscoveredTask> = discovery
                .tasks
                .iter()
                .filter(|d| d.matches_scope(&scope))
                .filter(|d| task_matches_selector(d, &scoped_task))
                .collect();

            let selected = if scoped_matches.is_empty() {
                let needle = scoped_task.to_ascii_lowercase();
                if needle.len() < 2 {
                    None
                } else {
                    let mut matches = discovery.tasks.iter().filter(|d| {
                        d.matches_scope(&scope)
                            && generate_abbreviation(&d.task.name)
                                .map(|abbr| abbr == needle)
                                .unwrap_or(false)
                    });
                    let first = matches.next();
                    if first.is_some() && matches.next().is_none() {
                        first
                    } else {
                        None
                    }
                }
            } else if scoped_matches.len() == 1 {
                Some(scoped_matches[0])
            } else {
                return Err(ambiguous_task_error(task_name, &scoped_matches));
            };

            if let Some(discovered) = selected {
                return Ok(Some(discovered));
            }

            let scoped_available: Vec<String> = discovery
                .tasks
                .iter()
                .filter(|d| d.matches_scope(&scope))
                .map(task_reference)
                .collect();
            scoped_not_found = Some((scope, scoped_task, scoped_available));
        }
    }

    let exact_matches: Vec<&discover::DiscoveredTask> = discovery
        .tasks
        .iter()
        .filter(|d| task_matches_selector(d, task_name))
        .collect();

    let discovered = if exact_matches.is_empty() {
        let needle = task_name.to_ascii_lowercase();
        if needle.len() < 2 {
            None
        } else {
            let mut matches = discovery.tasks.iter().filter(|d| {
                generate_abbreviation(&d.task.name)
                    .map(|abbr| abbr == needle)
                    .unwrap_or(false)
            });
            if let Some(first) = matches.next() {
                if matches.next().is_some() {
                    None
                } else {
                    Some(first)
                }
            } else {
                None
            }
        }
    } else if exact_matches.len() == 1 {
        Some(exact_matches[0])
    } else {
        Some(resolve_ambiguous_task_match(
            task_name,
            &exact_matches,
            discovery.root_cfg.as_ref(),
        )?)
    };

    if let Some(discovered) = discovered {
        return Ok(Some(discovered));
    }

    if let Some((scope, scoped_task, scoped_available)) = scoped_not_found {
        bail!(
            "task '{}' not found in scope '{}'.\nAvailable in scope: {}",
            scoped_task,
            scope,
            if scoped_available.is_empty() {
                "(none)".to_string()
            } else {
                scoped_available.join(", ")
            }
        );
    }

    Ok(None)
}

fn parse_scoped_selector(selector: &str) -> Option<(String, String)> {
    let trimmed = selector.trim();
    if let Some((scope, task)) = trimmed.split_once(':') {
        let scope = scope.trim();
        let task = task.trim();
        if !scope.is_empty() && !task.is_empty() {
            return Some((scope.to_string(), task.to_string()));
        }
    }
    if let Some((scope, task)) = trimmed.split_once('/') {
        let scope = scope.trim();
        let task = task.trim();
        if !scope.is_empty() && !task.is_empty() {
            return Some((scope.to_string(), task.to_string()));
        }
    }
    None
}

fn task_matches_selector(task: &discover::DiscoveredTask, needle: &str) -> bool {
    task.task.name.eq_ignore_ascii_case(needle)
        || task
            .task
            .shortcuts
            .iter()
            .any(|s| s.eq_ignore_ascii_case(needle))
}

fn task_reference(task: &discover::DiscoveredTask) -> String {
    let mut out = format!("{}:{}", task.scope, task.task.name);
    if !task.relative_dir.is_empty() {
        out.push_str(&format!(" ({})", task.relative_dir));
    }
    out
}

fn ambiguous_task_error(task_name: &str, matches: &[&discover::DiscoveredTask]) -> anyhow::Error {
    let mut msg = String::new();
    msg.push_str(&format!("task '{}' is ambiguous.\n", task_name));
    msg.push_str("Discovered matches:\n");
    for task in matches {
        msg.push_str(&format!("  - {}\n", task_reference(task)));
    }
    msg.push_str("Try one of:\n");
    for task in matches {
        msg.push_str(&format!(
            "  f {}:{}\n  f run --config {} {}\n",
            task.scope,
            task.task.name,
            task.config_path.display(),
            task.task.name
        ));
    }
    anyhow::anyhow!(msg.trim_end().to_string())
}

fn resolve_ambiguous_task_match<'a>(
    query: &str,
    matches: &[&'a discover::DiscoveredTask],
    root_cfg: Option<&Config>,
) -> Result<&'a discover::DiscoveredTask> {
    let Some(policy) = root_cfg.and_then(|cfg| cfg.task_resolution.as_ref()) else {
        return Err(ambiguous_task_error(query, matches));
    };

    let mut route_scope: Option<&str> = None;
    for (task, scope) in &policy.routes {
        if task.eq_ignore_ascii_case(query)
            || matches
                .iter()
                .any(|m| m.task.name.eq_ignore_ascii_case(task))
        {
            route_scope = Some(scope.as_str());
            break;
        }
    }
    if let Some(scope) = route_scope {
        let routed: Vec<&discover::DiscoveredTask> = matches
            .iter()
            .copied()
            .filter(|m| m.matches_scope(scope))
            .collect();
        if routed.len() == 1 {
            if policy.warn_on_implicit_scope.unwrap_or(false) {
                eprintln!(
                    "note: routed '{}' to scope '{}' via [task_resolution.routes].",
                    query, scope
                );
            }
            return Ok(routed[0]);
        }
        if routed.len() > 1 {
            return Err(ambiguous_task_error(query, &routed));
        }
    }

    for scope in &policy.preferred_scopes {
        let preferred: Vec<&discover::DiscoveredTask> = matches
            .iter()
            .copied()
            .filter(|m| m.matches_scope(scope))
            .collect();
        if preferred.len() == 1 {
            if policy.warn_on_implicit_scope.unwrap_or(false) {
                eprintln!(
                    "note: selected '{}' from preferred scope '{}'.",
                    query, scope
                );
            }
            return Ok(preferred[0]);
        }
        if preferred.len() > 1 {
            return Err(ambiguous_task_error(query, &preferred));
        }
    }

    Err(ambiguous_task_error(query, matches))
}

pub fn run(opts: TaskRunOpts) -> Result<()> {
    let config_path_for_deps = opts.config.clone();
    let (config_path, cfg) = load_project_config(opts.config)?;
    let project_name = cfg.project_name.clone();
    let workdir = config_path.parent().unwrap_or(Path::new("."));

    maybe_warn_non_fishx();

    // Set active project when running a task
    if let Some(ref name) = project_name {
        let _ = projects::set_active_project(name);
    }

    let ai_policy = AiTaskExecutionPolicy::from_env();
    let task = if let Some(task) = find_task(&cfg, &opts.name) {
        task
    } else {
        if execute_ai_task_by_selector(workdir, &opts.name, &opts.args, &ai_policy)? {
            return Ok(());
        }
        bail!(
            "task '{}' not found in {}",
            opts.name,
            config_path.display()
        );
    };

    // Build user_input early so we can record failures
    let quoted_args: Vec<String> = opts
        .args
        .iter()
        .map(|arg| shell_words::quote(arg).into_owned())
        .collect();
    let user_input = if opts.args.is_empty() {
        task.name.clone()
    } else {
        format!("{} {}", task.name, quoted_args.join(" "))
    };
    let base_command = task.command.trim().to_string();
    let display_command = if opts.args.is_empty() {
        base_command.clone()
    } else {
        format!("{} {}", base_command, quoted_args.join(" "))
    };

    // Helper to record a failed invocation
    let record_failure = |error_msg: &str| {
        let mut record = InvocationRecord::new(
            workdir.display().to_string(),
            config_path.display().to_string(),
            project_name.as_deref(),
            &task.name,
            &display_command,
            &user_input,
            false,
        );
        record.success = false;
        record.status = Some(1);
        record.output = error_msg.to_string();
        if let Err(err) = history::record(record) {
            tracing::warn!(?err, "failed to write task history");
        }
    };

    // Resolve dependencies and record failure if it fails
    let resolved = match resolve_task_dependencies(task, &cfg) {
        Ok(r) => r,
        Err(err) => {
            record_failure(&err.to_string());
            return Err(err);
        }
    };

    // Run task dependencies first (tasks that must complete before this one)
    if !resolved.task_deps.is_empty() {
        for dep_task_name in &resolved.task_deps {
            println!("Running dependency task '{}'...", dep_task_name);
            let dep_opts = TaskRunOpts {
                config: config_path_for_deps.clone(),
                delegate_to_hub: false,
                hub_host: opts.hub_host,
                hub_port: opts.hub_port,
                name: dep_task_name.clone(),
                args: vec![],
            };
            if let Err(err) = run(dep_opts) {
                record_failure(&format!(
                    "dependency task '{}' failed: {}",
                    dep_task_name, err
                ));
                bail!("dependency task '{}' failed: {}", dep_task_name, err);
            }
            println!();
        }
    }

    let should_delegate = opts.delegate_to_hub || task.delegate_to_hub;
    if should_delegate {
        match delegate_task_to_hub(
            task,
            &resolved,
            workdir,
            opts.hub_host,
            opts.hub_port,
            &display_command,
        ) {
            Ok(()) => {
                let mut record = InvocationRecord::new(
                    workdir.display().to_string(),
                    config_path.display().to_string(),
                    project_name.as_deref(),
                    &task.name,
                    &display_command,
                    &user_input,
                    false,
                );
                record.success = true;
                record.status = Some(0);
                record.output = format!("delegated to hub at {}:{}", opts.hub_host, opts.hub_port);
                if let Err(err) = history::record(record) {
                    tracing::warn!(?err, "failed to write task history");
                }
                return Ok(());
            }
            Err(err) => {
                println!(
                    "⚠️  Failed to delegate task '{}' to hub ({}); falling back to local execution.",
                    task.name, err
                );
            }
        }
    }

    let flox_pkgs = collect_flox_packages(&cfg, &resolved.flox);
    let mut preamble = String::new();
    let flox_disabled_env = std::env::var_os("FLOW_DISABLE_FLOX").is_some();
    let flox_disabled_marker = flox_disabled_marker(workdir).exists();
    let flox_enabled = !flox_pkgs.is_empty() && !flox_disabled_env && !flox_disabled_marker;

    if flox_enabled {
        log_and_capture(
            &mut preamble,
            &format!(
                "Skipping host PATH checks; using managed deps [{}]",
                flox_pkgs
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    } else {
        if flox_disabled_env {
            log_and_capture(
                &mut preamble,
                "FLOW_DISABLE_FLOX is set; running on host PATH",
            );
        }
        if let Err(err) = ensure_command_dependencies_available(&resolved.commands) {
            record_failure(&err.to_string());
            return Err(err);
        }
    }
    execute_task(
        task,
        &config_path,
        workdir,
        preamble,
        project_name.as_deref(),
        &flox_pkgs,
        flox_enabled,
        &base_command,
        &opts.args,
        &user_input,
    )
}

pub fn activate(opts: TaskActivateOpts) -> Result<()> {
    let (config_path, cfg) = load_project_config(opts.config)?;
    let workdir = config_path.parent().unwrap_or(Path::new("."));
    let project_name = cfg.project_name.clone();

    let tasks: Vec<&TaskConfig> = cfg
        .tasks
        .iter()
        .filter(|task| task.activate_on_cd_to_root)
        .collect();

    if tasks.is_empty() {
        return Ok(());
    }

    let mut combined = ResolvedDependencies::default();
    for task in &tasks {
        let resolved = resolve_task_dependencies(task, &cfg)?;
        combined.commands.extend(resolved.commands);
        combined.flox.extend(resolved.flox);
    }

    let flox_pkgs = collect_flox_packages(&cfg, &combined.flox);
    let mut preamble = String::new();
    if flox_pkgs.is_empty() {
        ensure_command_dependencies_available(&combined.commands)?;
    } else {
        log_and_capture(
            &mut preamble,
            &format!(
                "Skipping host PATH checks; using managed deps [{}]",
                flox_pkgs
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }
    for task in tasks {
        let flox_disabled_env = std::env::var_os("FLOW_DISABLE_FLOX").is_some();
        let flox_disabled_marker = flox_disabled_marker(workdir).exists();
        let flox_enabled = !flox_pkgs.is_empty() && !flox_disabled_env && !flox_disabled_marker;
        let command = task.command.trim().to_string();
        let empty_args: Vec<String> = Vec::new();
        execute_task(
            task,
            &config_path,
            workdir,
            preamble.clone(),
            project_name.as_deref(),
            &flox_pkgs,
            flox_enabled,
            &command,
            &empty_args,
            &task.name,
        )?;
    }

    Ok(())
}

pub(crate) fn load_project_config(path: PathBuf) -> Result<(PathBuf, Config)> {
    let mut config_path = resolve_path(path)?;
    if !config_path.exists() {
        let is_default = is_default_flow_config(&config_path);
        if is_default {
            if let Some(found) =
                find_flow_toml_upwards(config_path.parent().unwrap_or_else(|| Path::new(".")))
            {
                config_path = found;
            } else {
                init::write_template(&config_path)?;
                println!("Created starter flow.toml at {}", config_path.display());
            }
        }
    }
    let cfg = config::load(&config_path).with_context(|| {
        format!(
            "failed to load flow tasks configuration at {}",
            config_path.display()
        )
    })?;
    if let Some(name) = cfg.project_name.as_deref() {
        if let Err(err) = projects::register_project(name, &config_path) {
            tracing::debug!(?err, "failed to register project name");
        }
    }
    Ok((config_path, cfg))
}

fn resolve_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn is_default_flow_config(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("flow.toml")
}

fn find_flow_toml_upwards(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn log_and_capture(buf: &mut String, msg: &str) {
    println!("{msg}");
    buf.push_str(msg);
    if !msg.ends_with('\n') {
        buf.push('\n');
    }
}

fn log_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/logs")
}

fn sanitize_component(raw: &str) -> String {
    let mut s = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            s.push(ch);
        } else {
            s.push('-');
        }
    }
    s.trim_matches('-').to_lowercase()
}

fn short_hash(input: &str) -> String {
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn task_log_path(ctx: &TaskContext) -> Option<PathBuf> {
    let base = log_dir();
    let slug = if let Some(name) = ctx.project_name.as_deref() {
        let clean = sanitize_component(name);
        if clean.is_empty() {
            format!(
                "proj-{}",
                short_hash(&ctx.project_root.display().to_string())
            )
        } else {
            format!(
                "{clean}-{}",
                short_hash(&ctx.project_root.display().to_string())
            )
        }
    } else {
        format!(
            "proj-{}",
            short_hash(&ctx.project_root.display().to_string())
        )
    };

    let task = {
        let clean = sanitize_component(&ctx.task_name);
        if clean.is_empty() {
            "task".to_string()
        } else {
            clean
        }
    };

    Some(base.join(slug).join(format!("{task}.log")))
}

fn task_output_path(raw: &str, workdir: &Path) -> PathBuf {
    let expanded = config::expand_path(raw);
    if expanded.is_absolute() {
        expanded
    } else {
        workdir.join(expanded)
    }
}

fn execute_task(
    task: &TaskConfig,
    config_path: &Path,
    workdir: &Path,
    mut preamble: String,
    project_name: Option<&str>,
    flox_pkgs: &[(String, FloxInstallSpec)],
    flox_enabled: bool,
    command: &str,
    args: &[String],
    user_input: &str,
) -> Result<()> {
    if command.is_empty() {
        bail!("task '{}' has an empty command", task.name);
    }

    log_and_capture(
        &mut preamble,
        &format!("Running task '{}': {}", task.name, command),
    );

    // Create context for PID tracking
    let canonical_config = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let canonical_workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());

    // Auto-detect interactive mode if not explicitly set
    let interactive = task.interactive || needs_interactive_mode(command);

    let task_ctx = TaskContext {
        task_name: task.name.clone(),
        command: command.to_string(),
        config_path: canonical_config,
        project_root: canonical_workdir.clone(),
        used_flox: flox_enabled && !flox_pkgs.is_empty(),
        project_name: project_name.map(|s| s.to_string()),
        log_path: None,
        interactive,
    };

    // Set up cancel handler if on_cancel is defined
    setup_cancel_handler(task.on_cancel.as_deref(), workdir);

    let mut record = InvocationRecord::new(
        workdir.display().to_string(),
        config_path.display().to_string(),
        project_name,
        &task.name,
        command,
        user_input,
        !flox_pkgs.is_empty(),
    );
    let started = Instant::now();
    let mut combined_output = preamble;
    let status: ExitStatus;

    let flox_disabled = flox_disabled_marker(workdir).exists();

    if flox_pkgs.is_empty() || flox_disabled || !flox_enabled {
        let (st, out) = run_host_command(workdir, command, args, Some(task_ctx.clone()))?;
        status = st;
        combined_output.push_str(&out);
    } else {
        log_and_capture(
            &mut combined_output,
            &format!(
                "Skipping host PATH checks; using managed deps [{}]",
                flox_pkgs
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        match flox_health_check(workdir, flox_pkgs) {
            Ok(true) => {
                match run_flox_with_reset(flox_pkgs, workdir, command, args, Some(task_ctx.clone()))
                {
                    Ok(Some((st, out))) => {
                        combined_output.push_str(&out);
                        if st.success() {
                            status = st;
                        } else {
                            log_and_capture(
                                &mut combined_output,
                                &format!(
                                    "flox activate failed (status {:?}); retrying on host PATH",
                                    st.code()
                                ),
                            );
                            let (host_status, host_out) =
                                run_host_command(workdir, command, args, Some(task_ctx.clone()))?;
                            combined_output
                                .push_str("\n[flox activate failed; retried on host PATH]\n");
                            combined_output.push_str(&host_out);
                            status = host_status;
                        }
                    }
                    Ok(None) => {
                        log_and_capture(
                            &mut combined_output,
                            "flox disabled after repeated errors; using host PATH",
                        );
                        combined_output.push_str("[flox disabled after errors]\n");
                        let (host_status, host_out) =
                            run_host_command(workdir, command, args, Some(task_ctx.clone()))?;
                        combined_output.push_str(&host_out);
                        status = host_status;
                    }
                    Err(err) => {
                        log_and_capture(
                            &mut combined_output,
                            &format!("flox activate failed ({err}); retrying on host PATH"),
                        );
                        let (host_status, host_out) =
                            run_host_command(workdir, command, args, Some(task_ctx.clone()))?;
                        combined_output
                            .push_str("\n[flox activate failed; retried on host PATH]\n");
                        combined_output.push_str(&host_out);
                        status = host_status;
                    }
                }
            }
            Ok(false) => {
                log_and_capture(
                    &mut combined_output,
                    "flox disabled after health check; using host PATH",
                );
                combined_output.push_str("[flox disabled after health check]\n");
                let (host_status, host_out) =
                    run_host_command(workdir, command, args, Some(task_ctx.clone()))?;
                combined_output.push_str(&host_out);
                status = host_status;
            }
            Err(err) => {
                log_and_capture(
                    &mut combined_output,
                    &format!("flox health check failed ({err}); using host PATH"),
                );
                combined_output.push_str("[flox health check failed; using host PATH]\n");
                let (host_status, host_out) =
                    run_host_command(workdir, command, args, Some(task_ctx))?;
                combined_output.push_str(&host_out);
                status = host_status;
            }
        }
    }

    record.duration_ms = started.elapsed().as_millis();
    record.status = status.code();
    record.success = status.success();
    record.output = combined_output;
    let output = record.output.clone();

    if let Some(output_file) = task.output_file.as_deref() {
        let path = task_output_path(output_file, workdir);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(err) = fs::write(&path, record.output.as_bytes()) {
            tracing::warn!(?err, path = %path.display(), "failed to write task output file");
        }
    }

    // Record to jazz2 first (borrows), then history (takes ownership)
    if let Err(err) = jazz_state::record_task_run(&record) {
        tracing::warn!(?err, "failed to write jazz2 task run");
    }
    if let Err(err) = history::record(record) {
        tracing::warn!(?err, "failed to write task history");
    }

    // Clear cancel handler since task completed normally
    clear_cancel_handler();

    if status.success() {
        Ok(())
    } else {
        write_failure_bundle(
            &task.name,
            command,
            workdir,
            config_path,
            project_name,
            &output,
            status.code(),
        );
        task_failure_agents::maybe_run_task_failure_agents(
            &task.name,
            command,
            workdir,
            &output,
            status.code(),
        );
        maybe_run_task_failure_hook(&task.name, command, workdir, &output, status.code());
        bail!(
            "task '{}' exited with status {}",
            task.name,
            status.code().unwrap_or(-1)
        );
    }
}

#[cfg(test)]
fn format_task_lines(tasks: &[TaskConfig]) -> Vec<String> {
    let mut lines = Vec::new();
    for (idx, task) in tasks.iter().enumerate() {
        let shortcut_display = if task.shortcuts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", task.shortcuts.join(", "))
        };
        lines.push(format!(
            "{:>2}. {}{} – {}",
            idx + 1,
            task.name,
            shortcut_display,
            task.command
        ));
        if let Some(desc) = &task.description {
            lines.push(format!("    {desc}"));
        }
    }
    lines
}

fn format_discovered_task_lines(
    tasks: &[discover::DiscoveredTask],
    ai_tasks_list: &[ai_tasks::DiscoveredAiTask],
) -> Vec<String> {
    let mut lines = Vec::new();
    for (idx, discovered) in tasks.iter().enumerate() {
        let task = &discovered.task;
        let shortcut_display = if task.shortcuts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", task.shortcuts.join(", "))
        };

        // Keep relative path visible for debugging where each selector resolves.
        let path_suffix = if let Some(path_label) = discovered.path_label() {
            format!(" ({})", path_label)
        } else {
            String::new()
        };

        lines.push(format!(
            "{:>2}. {}:{}{}{} – {}",
            idx + 1,
            discovered.scope,
            task.name,
            shortcut_display,
            path_suffix,
            task.command
        ));
        if let Some(desc) = &task.description {
            lines.push(format!("    {desc}"));
        }
    }

    let base = lines.len();
    for (idx, task) in ai_tasks_list.iter().enumerate() {
        let tags = if task.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", task.tags.join(","))
        };
        lines.push(format!(
            "{:>2}. {}{} ({}) – moon run {}",
            base + idx + 1,
            task.id,
            tags,
            task.relative_path,
            task.path.display()
        ));
        if !task.description.trim().is_empty() {
            lines.push(format!("    {}", task.description.trim()));
        }
    }

    lines
}

fn print_duplicate_tasks(tasks: &[discover::DiscoveredTask]) -> Result<()> {
    let mut by_name: BTreeMap<String, Vec<&discover::DiscoveredTask>> = BTreeMap::new();
    for task in tasks {
        by_name
            .entry(task.task.name.to_ascii_lowercase())
            .or_default()
            .push(task);
    }

    let mut duplicates: Vec<(String, Vec<&discover::DiscoveredTask>)> = by_name
        .into_iter()
        .filter_map(|(name, mut entries)| {
            if entries.len() < 2 {
                return None;
            }
            entries.sort_by(|a, b| {
                a.scope
                    .cmp(&b.scope)
                    .then_with(|| a.relative_dir.cmp(&b.relative_dir))
            });
            Some((name, entries))
        })
        .collect();
    duplicates.sort_by(|a, b| a.0.cmp(&b.0));

    if duplicates.is_empty() {
        println!("No duplicate task names found.");
        return Ok(());
    }

    println!("Duplicate task names:");
    for (name, entries) in duplicates {
        println!();
        println!("  {} ({})", name, entries.len());
        for entry in entries {
            println!(
                "    - {}:{}  [{}]",
                entry.scope,
                entry.task.name,
                entry.config_path.display()
            );
        }
    }
    Ok(())
}

const AI_TASK_STARTER: &str = r#"// title: Starter AI Task
// description: Example MoonBit task under .ai/tasks.
// tags: [ai, moonbit, task]
//
// Run with:
//   f starter
// or:
//   f ai:starter

fn main {
  println("starter ai task: ok")
}
"#;

pub(crate) fn find_task<'a>(cfg: &'a Config, needle: &str) -> Option<&'a TaskConfig> {
    let normalized = needle.trim();
    if normalized.is_empty() {
        return None;
    }

    if let Some(task) = cfg
        .tasks
        .iter()
        .find(|task| task.name.eq_ignore_ascii_case(normalized))
    {
        return Some(task);
    }

    if let Some(task) = cfg.tasks.iter().find(|task| {
        task.shortcuts
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(normalized))
    }) {
        return Some(task);
    }

    resolve_by_abbreviation(&cfg.tasks, normalized)
}

fn resolve_by_abbreviation<'a>(tasks: &'a [TaskConfig], alias: &str) -> Option<&'a TaskConfig> {
    let alias = alias.trim().to_ascii_lowercase();
    if alias.len() < 2 {
        return None;
    }

    let mut matches = tasks.iter().filter(|task| {
        generate_abbreviation(&task.name)
            .map(|abbr| abbr == alias)
            .unwrap_or(false)
    });

    let first = matches.next()?;
    if matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

fn generate_abbreviation(name: &str) -> Option<String> {
    let mut abbr = String::new();
    let mut new_segment = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if new_segment {
                abbr.push(ch.to_ascii_lowercase());
                new_segment = false;
            }
        } else {
            new_segment = true;
        }
    }

    if abbr.len() >= 2 { Some(abbr) } else { None }
}

/// Check if command already references shell positional args ($@, $*, $1, etc.)
fn command_references_args(command: &str) -> bool {
    // Look for $@, $*, $1-$9, ${1}, ${@}, etc.
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            match chars.peek() {
                Some('@') | Some('*') | Some('1'..='9') => return true,
                Some('{') => {
                    // Check for ${1}, ${@}, ${*}, etc.
                    chars.next();
                    match chars.peek() {
                        Some('@') | Some('*') | Some('1'..='9') => return true,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn has_tty_access() -> bool {
    if std::io::stdin().is_terminal() {
        return true;
    }
    #[cfg(unix)]
    {
        std::fs::File::open("/dev/tty").is_ok()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn fishx_enabled() -> bool {
    match env::var("FISHX") {
        Ok(value) => {
            let value = value.trim().to_lowercase();
            value == "1" || value == "true" || value == "yes" || value == "on"
        }
        Err(_) => false,
    }
}

fn maybe_warn_non_fishx() {
    if !std::io::stdin().is_terminal() {
        return;
    }
    if fishx_enabled() {
        return;
    }
    if env::var_os("FLOW_ALLOW_NON_FISHX").is_some() {
        return;
    }
    if FISHX_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    // Only warn if fishx is installed but not active — contributors who
    // never installed fishx shouldn't see a confusing warning.
    if which::which("fishx").is_err() {
        return;
    }
    eprintln!(
        "⚠️  fishx is installed but not active. Flow runs best under fishx for error capture and AI hints.\n\
   Tip: run `f deploy-login` in the fishx repo or set FLOW_ALLOW_NON_FISHX=1 to hide this warning."
    );
}

fn failure_bundle_path() -> Option<PathBuf> {
    if let Ok(path) = env::var("FISHX_FAILURE_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    if let Ok(path) = env::var("FLOW_FAILURE_BUNDLE_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    dirs::cache_dir().map(|dir| dir.join("flow").join("last-task-failure.json"))
}

fn resolve_task_failure_hook() -> Option<String> {
    if let Ok(value) = env::var("FLOW_TASK_FAILURE_HOOK") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let config = config::load_ts_config()?;
    let flow = config.flow?;
    let hook = flow.task_failure_hook?;
    let trimmed = hook.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn truncate_output_for_hook(output: &str, max_lines: usize, max_chars: usize) -> String {
    let mut lines: Vec<&str> = output.lines().collect();
    if lines.len() > max_lines {
        lines = lines[lines.len().saturating_sub(max_lines)..].to_vec();
    }
    let mut joined = lines.join("\n");
    if joined.len() > max_chars {
        let start = joined.len().saturating_sub(max_chars);
        joined = format!("...{}", &joined[start..]);
    }
    joined
}

fn maybe_run_task_failure_hook(
    task_name: &str,
    command: &str,
    workdir: &Path,
    output: &str,
    status: Option<i32>,
) {
    if env::var_os("FLOW_DISABLE_TASK_FAILURE_HOOK").is_some() {
        return;
    }
    let Some(hook) = resolve_task_failure_hook() else {
        return;
    };
    if !std::io::stdin().is_terminal() {
        return;
    }
    let mut hook = hook;
    if env::var_os("FLOW_TASK_FAILURE_HOOK_ALLOW_OPEN").is_none() {
        let hook_lower = hook.to_ascii_lowercase();
        if hook_lower.contains("rise work") {
            hook = sanitize_rise_work_hook_no_open(&hook);
        }
    }
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(&hook)
        .current_dir(workdir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd.env("FLOW_TASK_NAME", task_name);
    cmd.env("FLOW_TASK_COMMAND", secret_redact::redact_text(command));
    cmd.env("FLOW_TASK_WORKDIR", workdir.display().to_string());
    cmd.env("FLOW_TASK_STATUS", status.unwrap_or(-1).to_string());
    if let Some(path) = failure_bundle_path() {
        cmd.env("FLOW_FAILURE_BUNDLE_PATH", path.display().to_string());
    }
    let tail = truncate_output_for_hook(output, 120, 12000);
    if !tail.is_empty() {
        cmd.env("FLOW_TASK_OUTPUT_TAIL", secret_redact::redact_text(&tail));
    }
    match cmd.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("⚠ task failure hook exited with status {:?}", status.code());
        }
        Err(err) => {
            eprintln!("⚠ failed to run task failure hook: {}", err);
        }
    }
}

fn sanitize_rise_work_hook_no_open(hook: &str) -> String {
    let tokens = match shell_words::split(hook) {
        Ok(tokens) => tokens,
        Err(_) => {
            let mut fallback = hook.to_string();
            let lower = fallback.to_ascii_lowercase();
            if !lower.contains("--no-open") {
                fallback.push_str(" --no-open");
            }
            return fallback;
        }
    };

    let mut cleaned: Vec<String> = Vec::new();
    let mut skip_next = false;
    for token in tokens {
        if skip_next {
            skip_next = false;
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if lower == "--focus" {
            continue;
        }
        if lower == "--focus-app" || lower == "--app" || lower == "--target" {
            skip_next = true;
            continue;
        }
        if lower.starts_with("--focus-app=")
            || lower.starts_with("--app=")
            || lower.starts_with("--target=")
        {
            continue;
        }
        cleaned.push(token);
    }

    let mut rebuilt = shell_words::join(cleaned);
    let lower = rebuilt.to_ascii_lowercase();
    if !lower.contains("--no-open") {
        if !rebuilt.is_empty() {
            rebuilt.push(' ');
        }
        rebuilt.push_str("--no-open");
    }
    rebuilt
}

fn truncate_for_bundle(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars {
        return output.to_string();
    }
    let start = output.len().saturating_sub(max_chars);
    format!("...{}", &output[start..])
}

fn write_failure_bundle(
    task_name: &str,
    command: &str,
    workdir: &Path,
    config_path: &Path,
    project_name: Option<&str>,
    output: &str,
    status: Option<i32>,
) {
    let Some(path) = failure_bundle_path() else {
        return;
    };

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let payload = json!({
        "task": task_name,
        "command": secret_redact::redact_text(command),
        "workdir": workdir.display().to_string(),
        "config": config_path.display().to_string(),
        "project": project_name,
        "status": status.unwrap_or(-1),
        "output": secret_redact::redact_text(&truncate_for_bundle(output, 20_000)),
        "fishx": fishx_enabled(),
        "ts": ts,
    });

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(err) = fs::write(&path, payload.to_string().as_bytes()) {
        tracing::warn!(?err, path = %path.display(), "failed to write task failure bundle");
        return;
    }

    if std::io::stdin().is_terminal() {
        eprintln!("🧩 failure bundle: {}", path.display());
        if which("fx-failure").is_ok() {
            eprintln!("   Tip: run `fx-failure` or `last-error` for a quick fix prompt.");
        }
    }
}

fn run_host_command(
    workdir: &Path,
    command: &str,
    args: &[String],
    ctx: Option<TaskContext>,
) -> Result<(ExitStatus, String)> {
    // For interactive tasks, run directly with inherited stdio
    // This ensures proper TTY handling for readline, prompts, etc.
    let interactive = ctx.as_ref().map(|c| c.interactive).unwrap_or(false);
    let is_tty = has_tty_access();

    if interactive && is_tty {
        return run_interactive_command(workdir, command, args, ctx);
    }

    let mut cmd = Command::new("/bin/sh");

    // If args are provided and command doesn't already reference them ($@ or $1, $2, etc.),
    // append "$@" to pass them through properly
    let full_command = if args.is_empty() || command_references_args(command) {
        command.to_string()
    } else {
        format!("{} \"$@\"", command)
    };

    cmd.arg("-c").arg(&full_command);
    if !args.is_empty() {
        cmd.arg("sh"); // $0 placeholder
        for arg in args {
            cmd.arg(arg);
        }
    }
    cmd.current_dir(workdir);
    inject_global_env(&mut cmd);
    run_command_with_tee(cmd, ctx).with_context(|| "failed to spawn command without managed env")
}

fn run_flox_with_reset(
    flox_pkgs: &[(String, FloxInstallSpec)],
    workdir: &Path,
    command: &str,
    args: &[String],
    ctx: Option<TaskContext>,
) -> Result<Option<(ExitStatus, String)>> {
    let mut combined_output = String::new();
    let mut reset_done = false;

    loop {
        let env = flox::ensure_env(workdir, flox_pkgs)?;
        match run_flox_command(&env, workdir, command, args, ctx.clone()) {
            Ok((status, out)) => {
                combined_output.push_str(&out);
                if status.success() {
                    return Ok(Some((status, combined_output)));
                }
                if !reset_done {
                    reset_flox_env(workdir)?;
                    combined_output
                        .push_str("\n[flox activate failed; reset .flox and retrying]\n");
                    reset_done = true;
                    continue;
                }
                mark_flox_disabled(workdir, "flox activate repeatedly failed")?;
                return Ok(None);
            }
            Err(err) => {
                combined_output.push_str(&format!("[flox activate error: {err}]\n"));
                if !reset_done {
                    reset_flox_env(workdir)?;
                    combined_output.push_str("[reset .flox and retrying]\n");
                    reset_done = true;
                    continue;
                }
                mark_flox_disabled(workdir, "flox activate error after reset")?;
                return Ok(None);
            }
        }
    }
}

fn flox_health_check(project_root: &Path, flox_pkgs: &[(String, FloxInstallSpec)]) -> Result<bool> {
    let env = flox::ensure_env(project_root, flox_pkgs)?;
    let flox_bin = which("flox").context("flox is required to run tasks with flox deps")?;
    let mut cmd = Command::new(flox_bin);
    cmd.arg("activate")
        .arg("-d")
        .arg(&env.project_root)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(":")
        .current_dir(project_root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match cmd.status() {
        Ok(status) if status.success() => Ok(true),
        _ => {
            mark_flox_disabled(project_root, "flox health check failed")?;
            Ok(false)
        }
    }
}

fn run_flox_command(
    env: &FloxEnv,
    workdir: &Path,
    command: &str,
    args: &[String],
    ctx: Option<TaskContext>,
) -> Result<(ExitStatus, String)> {
    // For interactive tasks, run directly with inherited stdio
    let interactive = ctx.as_ref().map(|c| c.interactive).unwrap_or(false);

    if interactive && has_tty_access() {
        return run_flox_interactive_command(env, workdir, command, args, ctx);
    }

    let flox_bin = which("flox").context("flox is required to run tasks with flox deps")?;

    // If args are provided and command doesn't already reference them,
    // append "$@" to pass them through properly
    let full_command = if args.is_empty() || command_references_args(command) {
        command.to_string()
    } else {
        format!("{} \"$@\"", command)
    };

    let mut cmd = Command::new(flox_bin);
    cmd.arg("activate")
        .arg("-d")
        .arg(&env.project_root)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(&full_command);
    if !args.is_empty() {
        cmd.arg("sh"); // $0 placeholder
        for arg in args {
            cmd.arg(arg);
        }
    }
    cmd.current_dir(workdir);
    inject_global_env(&mut cmd);
    run_command_with_tee(cmd, ctx).with_context(|| "failed to spawn flox activate for task")
}

/// Run an interactive flox command with inherited stdio for proper TTY handling.
fn run_flox_interactive_command(
    env: &FloxEnv,
    workdir: &Path,
    command: &str,
    args: &[String],
    ctx: Option<TaskContext>,
) -> Result<(ExitStatus, String)> {
    let flox_bin = which("flox").context("flox is required to run tasks with flox deps")?;

    // If args are provided and command doesn't already reference them,
    // append "$@" to pass them through properly
    let full_command = if args.is_empty() || command_references_args(command) {
        command.to_string()
    } else {
        format!("{} \"$@\"", command)
    };

    let mut cmd = Command::new(flox_bin);
    cmd.arg("activate")
        .arg("-d")
        .arg(&env.project_root)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(&full_command);
    if !args.is_empty() {
        cmd.arg("sh"); // $0 placeholder
        for arg in args {
            cmd.arg(arg);
        }
    }
    cmd.current_dir(workdir);

    // Inherit all stdio for full TTY passthrough
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // NOTE: Do NOT create a new process group for interactive commands.
    // The child must remain in the foreground process group to read from the terminal.

    let mut child = cmd
        .spawn()
        .with_context(|| "failed to spawn interactive flox command")?;

    let pid = child.id();
    let pgid = running::get_pgid(pid).unwrap_or(pid);
    set_cleanup_process(pid, pgid);

    if let Some(ref task_ctx) = ctx {
        let entry = RunningProcess {
            pid,
            pgid,
            task_name: task_ctx.task_name.clone(),
            command: task_ctx.command.clone(),
            started_at: running::now_ms(),
            config_path: task_ctx.config_path.clone(),
            project_root: task_ctx.project_root.clone(),
            used_flox: task_ctx.used_flox,
            project_name: task_ctx.project_name.clone(),
        };
        if let Err(err) = running::register_process(entry) {
            tracing::warn!(?err, "failed to register running process");
        }
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on interactive flox command")?;

    if ctx.is_some() {
        if let Err(err) = running::unregister_process(pid) {
            tracing::debug!(?err, "failed to unregister process");
        }
    }

    // No output captured for interactive commands
    Ok((status, String::new()))
}

fn run_command_with_tee(
    mut cmd: Command,
    ctx: Option<TaskContext>,
) -> Result<(ExitStatus, String)> {
    inject_global_env(&mut cmd);
    // Only use `script` for tasks explicitly marked as interactive
    // This avoids issues with non-interactive tasks hanging
    let interactive = ctx.as_ref().map(|c| c.interactive).unwrap_or(false);
    let use_script = interactive && std::io::stdin().is_terminal() && cfg!(unix);

    if use_script {
        run_command_with_script(cmd, ctx)
    } else {
        run_command_with_pipes(cmd, ctx)
    }
}

fn inject_global_env(cmd: &mut Command) {
    let keys = config::global_env_keys();
    if keys.is_empty() {
        return;
    }

    let missing: Vec<String> = keys
        .into_iter()
        .filter(|key| std::env::var_os(key).is_none())
        .collect();

    if missing.is_empty() {
        return;
    }

    // If not logged in to cloud, silently try local env store and skip.
    // This avoids prompting "Not logged in to cloud..." for contributors
    // who don't need cloud env vars (e.g. web-only dev).
    if !crate::env::has_cloud_auth_token() {
        match crate::env::fetch_local_personal_env_vars(&missing) {
            Ok(vars) => {
                for (key, value) in vars {
                    if !value.is_empty() {
                        cmd.env(key, value);
                    }
                }
            }
            Err(err) => {
                tracing::debug!(?err, "failed to read local env vars");
            }
        }
        return;
    }

    match crate::env::fetch_personal_env_vars(&missing) {
        Ok(vars) => {
            for (key, value) in vars {
                if !value.is_empty() {
                    cmd.env(key, value);
                }
            }
        }
        Err(err) => {
            tracing::debug!(?err, "failed to fetch global env vars");
        }
    }
}

/// Use the `script` command to run a command interactively while capturing output.
/// This is more reliable than manual PTY handling for diverse programs.
fn run_command_with_script(cmd: Command, ctx: Option<TaskContext>) -> Result<(ExitStatus, String)> {
    let interactive = ctx.as_ref().map(|c| c.interactive).unwrap_or(false);
    // Create a temp file for capturing output
    let temp_dir = std::env::temp_dir();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let script_log = temp_dir.join(format!("flow_script_{}.log", timestamp));

    // Build the inner command string
    let program = cmd.get_program().to_string_lossy().to_string();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let cwd = cmd.get_current_dir().map(|p| p.to_path_buf());

    // Construct the command to pass to script
    let inner_cmd = if args.is_empty() {
        shell_words::quote(&program).to_string()
    } else {
        format!(
            "{} {}",
            shell_words::quote(&program),
            args.iter()
                .map(|a| shell_words::quote(a).to_string())
                .collect::<Vec<_>>()
                .join(" ")
        )
    };

    // Build the script command
    // macOS: script -q <logfile> /bin/sh -c "command"
    // Linux: script -q -c "command" <logfile>
    let mut script_cmd = Command::new("script");

    #[cfg(target_os = "macos")]
    {
        // macOS script: script [-q] file [command ...]
        // We need to pass the shell and -c as separate args after the file
        script_cmd
            .arg("-q")
            .arg("-F") // Flush immediately
            .arg(&script_log)
            .args(["/bin/sh", "-c", &inner_cmd]);
    }

    #[cfg(not(target_os = "macos"))]
    {
        script_cmd
            .arg("-q")
            .arg("-c")
            .arg(&inner_cmd)
            .arg(&script_log);
    }

    if let Some(dir) = cwd {
        script_cmd.current_dir(dir);
    }

    // Run with TTY stdio for interactivity, even if parent stdio is redirected.
    let tty_in = std::fs::File::open("/dev/tty").ok();
    let tty_out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok();
    let tty_err = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok();

    if let (Some(tty_in), Some(tty_out), Some(tty_err)) = (tty_in, tty_out, tty_err) {
        script_cmd
            .stdin(Stdio::from(tty_in))
            .stdout(Stdio::from(tty_out))
            .stderr(Stdio::from(tty_err));
    } else {
        script_cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }

    // Create process group for proper signal handling (non-interactive only)
    #[cfg(unix)]
    if !interactive {
        use std::os::unix::process::CommandExt;
        script_cmd.process_group(0);
    }

    let mut child = script_cmd
        .spawn()
        .with_context(|| "failed to spawn script command")?;

    let pid = child.id();
    let pgid = running::get_pgid(pid).unwrap_or(pid);
    set_cleanup_process(pid, pgid);

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let entry = RunningProcess {
            pid,
            pgid,
            task_name: task_ctx.task_name.clone(),
            command: task_ctx.command.clone(),
            started_at: running::now_ms(),
            config_path: task_ctx.config_path.clone(),
            project_root: task_ctx.project_root.clone(),
            used_flox: task_ctx.used_flox,
            project_name: task_ctx.project_name.clone(),
        };
        if let Err(err) = running::register_process(entry) {
            tracing::warn!(?err, "failed to register running process");
        }
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on script command")?;

    // Unregister the process
    if ctx.is_some() {
        if let Err(err) = running::unregister_process(pid) {
            tracing::debug!(?err, "failed to unregister process");
        }
    }

    // Read the captured output
    let output = fs::read_to_string(&script_log).unwrap_or_default();

    // Clean up temp file
    let _ = fs::remove_file(&script_log);

    // Also write to log file if configured
    if let Some(ref task_ctx) = ctx {
        if let Some(log_path) = task_log_path(task_ctx) {
            if let Some(parent) = log_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
                let header = format!(
                    "\n--- {} | task:{} | cmd:{} ---\n",
                    running::now_ms(),
                    task_ctx.task_name,
                    task_ctx.command
                );
                let _ = file.write_all(header.as_bytes());
                let _ = file.write_all(output.as_bytes());
            }
        }
    }

    // Strip ANSI codes for history but keep colors for display (already shown)
    let clean_output = String::from_utf8_lossy(&strip_ansi_escapes::strip(&output)).to_string();

    Ok((status, clean_output))
}

/// Run an interactive command with inherited stdio for proper TTY handling.
/// Output is not captured (returns empty string) since interactive commands
/// need direct terminal access for readline, prompts, etc.
fn run_interactive_command(
    workdir: &Path,
    command: &str,
    args: &[String],
    ctx: Option<TaskContext>,
) -> Result<(ExitStatus, String)> {
    let mut cmd = Command::new("/bin/sh");

    // If args are provided and command doesn't already reference them,
    // append "$@" to pass them through properly
    let full_command = if args.is_empty() || command_references_args(command) {
        command.to_string()
    } else {
        format!("{} \"$@\"", command)
    };

    cmd.arg("-c").arg(&full_command);
    if !args.is_empty() {
        cmd.arg("sh"); // $0 placeholder
        for arg in args {
            cmd.arg(arg);
        }
    }
    cmd.current_dir(workdir);
    inject_global_env(&mut cmd);

    // Prefer /dev/tty so interactive tasks keep working even if stdio is redirected.
    // Fall back to inherited stdio if /dev/tty is unavailable.
    let tty_in = std::fs::File::open("/dev/tty").ok();
    let tty_out = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok();
    let tty_err = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .ok();

    if let (Some(tty_in), Some(tty_out), Some(tty_err)) = (tty_in, tty_out, tty_err) {
        cmd.stdin(Stdio::from(tty_in))
            .stdout(Stdio::from(tty_out))
            .stderr(Stdio::from(tty_err));
    } else {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    }

    // NOTE: Do NOT create a new process group for interactive commands.
    // The child must remain in the foreground process group to read from the terminal.

    let mut child = cmd
        .spawn()
        .with_context(|| "failed to spawn interactive command")?;

    let pid = child.id();
    let pgid = running::get_pgid(pid).unwrap_or(pid);
    set_cleanup_process(pid, pgid);

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let entry = RunningProcess {
            pid,
            pgid,
            task_name: task_ctx.task_name.clone(),
            command: task_ctx.command.clone(),
            started_at: running::now_ms(),
            config_path: task_ctx.config_path.clone(),
            project_root: task_ctx.project_root.clone(),
            used_flox: task_ctx.used_flox,
            project_name: task_ctx.project_name.clone(),
        };
        if let Err(err) = running::register_process(entry) {
            tracing::warn!(?err, "failed to register running process");
        }
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on interactive command")?;

    // Unregister the process
    if ctx.is_some() {
        if let Err(err) = running::unregister_process(pid) {
            tracing::debug!(?err, "failed to unregister process");
        }
    }

    // No output captured for interactive commands
    Ok((status, String::new()))
}

#[allow(dead_code)]
fn run_command_with_pty(cmd: Command, ctx: Option<TaskContext>) -> Result<(ExitStatus, String)> {
    let pty_system = NativePtySystem::default();

    // Get terminal size or use defaults
    let size = crossterm::terminal::size()
        .map(|(cols, rows)| PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap_or(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });

    let pair = pty_system
        .openpty(size)
        .map_err(|e| anyhow::anyhow!("failed to open pty: {}", e))?;

    // Build command for PTY - extract info from the std::process::Command
    // We need to reconstruct the command since portable-pty uses its own CommandBuilder
    let program = cmd.get_program().to_string_lossy().to_string();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let cwd = cmd.get_current_dir().map(|p| p.to_path_buf());

    let mut pty_cmd = CommandBuilder::new(&program);
    for arg in &args {
        pty_cmd.arg(arg);
    }
    if let Some(dir) = cwd {
        pty_cmd.cwd(dir);
    }

    let mut child = pair
        .slave
        .spawn_command(pty_cmd)
        .map_err(|e| anyhow::anyhow!("failed to spawn command in pty: {}", e))?;

    // Drop the slave side in the parent
    drop(pair.slave);

    let pid = child.process_id().unwrap_or(0);
    set_cleanup_process(pid, pid);

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let entry = RunningProcess {
            pid,
            pgid: pid, // PTY processes are their own group
            task_name: task_ctx.task_name.clone(),
            command: task_ctx.command.clone(),
            started_at: running::now_ms(),
            config_path: task_ctx.config_path.clone(),
            project_root: task_ctx.project_root.clone(),
            used_flox: task_ctx.used_flox,
            project_name: task_ctx.project_name.clone(),
        };
        if let Err(err) = running::register_process(entry) {
            tracing::warn!(?err, "failed to register running process");
        }
    }

    let output = Arc::new(Mutex::new(String::new()));

    // Set up optional log file
    let log_file = ctx.as_ref().and_then(|c| {
        let path = task_log_path(c)?;
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                let header = format!(
                    "\n--- {} | task:{} | cmd:{} ---\n",
                    running::now_ms(),
                    c.task_name,
                    c.command
                );
                let _ = file.write_all(header.as_bytes());
                Some(Arc::new(Mutex::new(file)))
            }
            Err(_) => None,
        }
    });

    // Get reader/writer for PTY master
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("failed to clone pty reader: {}", e))?;
    let writer = pair.master;

    // Thread to forward stdin to PTY
    let stdin_handle = {
        let mut writer = writer
            .take_writer()
            .map_err(|e| anyhow::anyhow!("failed to take pty writer: {}", e))?;
        thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = writer.flush();
                    }
                    Err(_) => break,
                }
            }
        })
    };

    // Create log ingester for fire-and-forget streaming to daemon
    let ingester = ctx.as_ref().map(|c| {
        Arc::new(LogIngester::new(
            c.project_name.as_deref().unwrap_or("unknown"),
            &c.task_name,
        ))
    });

    // Thread to read PTY output and tee to stdout + capture
    let output_clone = output.clone();
    let log_file_clone = log_file.clone();
    let ingester_clone = ingester.clone();
    let output_handle = thread::spawn(move || {
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 4096];
        let mut line_buf = String::new();
        let preferred_url = lifecycle_preferred_url();
        let mut preferred_url_hint_emitted = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = stdout.write_all(&buf[..n]);
                    let _ = stdout.flush();

                    if let Some(ref file) = log_file_clone {
                        if let Ok(mut f) = file.lock() {
                            let _ = f.write_all(&buf[..n]);
                            let _ = f.flush();
                        }
                    }

                    let text = String::from_utf8_lossy(&buf[..n]);

                    if let Ok(mut out) = output_clone.lock() {
                        out.push_str(&text);
                    }

                    if let Some(ref ing) = ingester_clone {
                        line_buf.push_str(&text);
                        while let Some(pos) = line_buf.find('\n') {
                            let line = line_buf[..pos].to_string();
                            maybe_emit_lifecycle_preferred_url_hint(
                                &preferred_url,
                                &line,
                                &mut preferred_url_hint_emitted,
                            );
                            ing.send(&line);
                            line_buf = line_buf[pos + 1..].to_string();
                        }
                    } else {
                        line_buf.push_str(&text);
                        while let Some(pos) = line_buf.find('\n') {
                            let line = line_buf[..pos].to_string();
                            maybe_emit_lifecycle_preferred_url_hint(
                                &preferred_url,
                                &line,
                                &mut preferred_url_hint_emitted,
                            );
                            line_buf = line_buf[pos + 1..].to_string();
                        }
                    }
                }
                Err(_) => break,
            }
        }
        // Flush remaining partial line
        if !line_buf.is_empty() {
            maybe_emit_lifecycle_preferred_url_hint(
                &preferred_url,
                &line_buf,
                &mut preferred_url_hint_emitted,
            );
            if let Some(ref ing) = ingester_clone {
                ing.send(&line_buf);
            }
        }
    });

    // Wait for the child process
    let exit_status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("failed to wait on child: {}", e))?;

    // Wait for output thread (stdin thread may block on read, so we don't join it)
    let _ = output_handle.join();
    drop(stdin_handle); // Let it terminate naturally

    // Unregister the process
    if ctx.is_some() {
        if let Err(err) = running::unregister_process(pid) {
            tracing::debug!(?err, "failed to unregister process");
        }
    }

    let collected = output
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| String::new());

    // Convert portable_pty ExitStatus to std::process::ExitStatus
    let code = exit_status.exit_code();
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exit {}", code))
        .status()
        .unwrap_or_else(|_| std::process::ExitStatus::default());

    Ok((status, collected))
}

fn run_command_with_pipes(
    mut cmd: Command,
    ctx: Option<TaskContext>,
) -> Result<(ExitStatus, String)> {
    let interactive = ctx.as_ref().map(|c| c.interactive).unwrap_or(false);

    // Interactive mode: inherit all stdio for TTY passthrough
    // NOTE: Do NOT create a new process group for interactive commands.
    // The child must remain in the foreground process group to read from the terminal.
    if interactive {
        let mut child = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| "failed to spawn interactive command")?;

        let pid = child.id();
        let pgid = running::get_pgid(pid).unwrap_or(pid);
        set_cleanup_process(pid, pgid);

        // Register the process
        if let Some(ref task_ctx) = ctx {
            let entry = RunningProcess {
                pid,
                pgid,
                task_name: task_ctx.task_name.clone(),
                command: task_ctx.command.clone(),
                started_at: running::now_ms(),
                config_path: task_ctx.config_path.clone(),
                project_root: task_ctx.project_root.clone(),
                used_flox: task_ctx.used_flox,
                project_name: task_ctx.project_name.clone(),
            };
            if let Err(err) = running::register_process(entry) {
                tracing::warn!(?err, "failed to register running process");
            }
        }

        let status = child.wait().with_context(|| "failed to wait on child")?;

        // Unregister on exit
        if let Err(err) = running::unregister_process(pid) {
            tracing::debug!(?err, "failed to unregister process");
        }

        return Ok((status, String::new()));
    }

    // Create new process group on Unix for reliable child process management
    // (only for non-interactive commands)
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .stdin(Stdio::inherit()) // Allow user input for prompts
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn command")?;

    let pid = child.id();
    let pgid = running::get_pgid(pid).unwrap_or(pid);
    set_cleanup_process(pid, pgid);

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let entry = RunningProcess {
            pid,
            pgid,
            task_name: task_ctx.task_name.clone(),
            command: task_ctx.command.clone(),
            started_at: running::now_ms(),
            config_path: task_ctx.config_path.clone(),
            project_root: task_ctx.project_root.clone(),
            used_flox: task_ctx.used_flox,
            project_name: task_ctx.project_name.clone(),
        };
        if let Err(err) = running::register_process(entry) {
            tracing::warn!(?err, "failed to register running process");
        }
    }

    let output = Arc::new(Mutex::new(String::new()));
    // Set up optional log file for streaming output
    let (ctx, log_file) = match ctx {
        Some(mut c) => {
            let path = task_log_path(&c);
            if let Some(path) = path {
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                match OpenOptions::new().create(true).append(true).open(&path) {
                    Ok(mut file) => {
                        let header = format!(
                            "\n--- {} | task:{} | cmd:{} ---\n",
                            running::now_ms(),
                            c.task_name,
                            c.command
                        );
                        let _ = file.write_all(header.as_bytes());
                        c.log_path = Some(path.clone());
                        (Some(c), Some(Arc::new(Mutex::new(file))))
                    }
                    Err(err) => {
                        if let Ok(mut buf) = output.lock() {
                            buf.push_str(&format!("failed to open log file: {err}\n"));
                        }
                        (Some(c), None)
                    }
                }
            } else {
                (Some(c), None)
            }
        }
        None => (None, None),
    };

    // Create log ingester for fire-and-forget streaming to daemon
    let ingester = ctx.as_ref().map(|c| {
        Arc::new(LogIngester::new(
            c.project_name.as_deref().unwrap_or("unknown"),
            &c.task_name,
        ))
    });

    let mut handles = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        handles.push(tee_stream(
            stdout,
            std::io::stdout(),
            output.clone(),
            log_file.clone(),
            ingester.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        handles.push(tee_stream(
            stderr,
            std::io::stderr(),
            output.clone(),
            log_file.clone(),
            ingester.clone(),
        ));
    }

    for handle in handles {
        let _ = handle.join();
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait for command completion")?;

    // Unregister the process
    if ctx.is_some() {
        if let Err(err) = running::unregister_process(pid) {
            tracing::warn!(?err, "failed to unregister process");
        }
    }

    let collected = output
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| String::new());

    Ok((status, collected))
}

fn lifecycle_preferred_url() -> Option<String> {
    crate::lifecycle::runtime_preferred_url()
}

fn is_service_ready_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    (lower.contains("local:") && lower.contains("http://"))
        || lower.contains("ready on http://")
        || lower.contains("listening on http://")
        || lower.contains("listening at http://")
}

fn maybe_emit_lifecycle_preferred_url_hint(
    preferred_url: &Option<String>,
    line: &str,
    emitted: &mut bool,
) {
    if *emitted {
        return;
    }
    if !is_service_ready_line(line) {
        return;
    }
    let Some(url) = preferred_url.as_deref() else {
        return;
    };
    println!("[flow][up] preferred URL: {url}");
    *emitted = true;
}

fn tee_stream<R, W>(
    mut reader: R,
    mut writer: W,
    buffer: Arc<Mutex<String>>,
    log_file: Option<Arc<Mutex<File>>>,
    ingester: Option<Arc<LogIngester>>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        let mut line_buf = String::new();
        let preferred_url = lifecycle_preferred_url();
        let mut preferred_url_hint_emitted = false;
        loop {
            let read = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };

            let _ = writer.write_all(&chunk[..read]);
            let _ = writer.flush();

            if let Some(file) = log_file.as_ref() {
                if let Ok(mut f) = file.lock() {
                    let _ = f.write_all(&chunk[..read]);
                    let _ = f.flush();
                }
            }

            let text = String::from_utf8_lossy(&chunk[..read]);

            if let Ok(mut buf) = buffer.lock() {
                buf.push_str(&text);
            }

            line_buf.push_str(&text);
            while let Some(pos) = line_buf.find('\n') {
                let line = line_buf[..pos].to_string();
                maybe_emit_lifecycle_preferred_url_hint(
                    &preferred_url,
                    &line,
                    &mut preferred_url_hint_emitted,
                );
                if let Some(ref ing) = ingester {
                    ing.send(&line);
                }
                line_buf = line_buf[pos + 1..].to_string();
            }
        }
        // Flush remaining partial line
        if !line_buf.is_empty() {
            maybe_emit_lifecycle_preferred_url_hint(
                &preferred_url,
                &line_buf,
                &mut preferred_url_hint_emitted,
            );
            if let Some(ref ing) = ingester {
                ing.send(&line_buf);
            }
        }
    })
}

fn reset_flox_env(project_root: &Path) -> Result<()> {
    let dir = project_root.join(".flox");
    if dir.exists() {
        fs::remove_dir_all(&dir)
            .with_context(|| format!("failed to remove flox env at {}", dir.display()))?;
    }
    Ok(())
}

fn flox_disabled_marker(project_root: &Path) -> PathBuf {
    project_root.join(".flox.disabled")
}

fn mark_flox_disabled(project_root: &Path, reason: &str) -> Result<()> {
    let marker = flox_disabled_marker(project_root);
    fs::write(&marker, reason).with_context(|| {
        format!(
            "failed to write flox disable marker at {}",
            marker.display()
        )
    })
}

#[derive(Debug, Default)]
struct ResolvedDependencies {
    commands: Vec<String>,
    flox: Vec<(String, FloxInstallSpec)>,
    /// Task names that must run before this task.
    task_deps: Vec<String>,
}

fn resolve_task_dependencies(task: &TaskConfig, cfg: &Config) -> Result<ResolvedDependencies> {
    if task.dependencies.is_empty() {
        return Ok(ResolvedDependencies::default());
    }

    let mut missing = Vec::new();
    let mut resolved = ResolvedDependencies::default();
    for dep_name in &task.dependencies {
        // First check if it's a [deps] entry
        if let Some(spec) = cfg.dependencies.get(dep_name) {
            match spec {
                config::DependencySpec::Single(cmd) => {
                    // If value looks like a URL/path, use the key as binary name
                    if cmd.contains('/') {
                        resolved.commands.push(dep_name.clone());
                    } else {
                        resolved.commands.push(cmd.clone());
                    }
                }
                config::DependencySpec::Multiple(cmds) => resolved.commands.extend(cmds.clone()),
                config::DependencySpec::Flox(pkg) => {
                    resolved.flox.push((dep_name.clone(), pkg.clone()));
                }
            }
            continue;
        }

        // Check if it's a flox install
        if let Some(flox) = cfg.flox.as_ref().and_then(|f| f.install.get(dep_name)) {
            resolved.flox.push((dep_name.clone(), flox.clone()));
            continue;
        }

        // Check if it's a task name (for task ordering)
        if cfg.tasks.iter().any(|t| t.name == *dep_name) {
            resolved.task_deps.push(dep_name.clone());
            continue;
        }

        missing.push(dep_name.as_str());
    }

    if !missing.is_empty() {
        bail!(
            "task '{}' references unknown dependencies: {} (define them under [deps], [flox.install], or as a task name)",
            task.name,
            missing.join(", ")
        );
    }

    Ok(resolved)
}

fn ensure_command_dependencies_available(commands: &[String]) -> Result<()> {
    if commands.is_empty() {
        return Ok(());
    }

    for command in commands {
        which::which(command).with_context(|| dependency_error(command))?;
    }

    Ok(())
}

fn dependency_error(command: &str) -> String {
    let mut msg = format!(
        "dependency '{}' not found in PATH. Install it or adjust the [dependencies] config.",
        command
    );
    if let Some(extra) = dependency_help(command) {
        msg.push('\n');
        msg.push_str(extra);
    }
    msg
}

fn dependency_help(command: &str) -> Option<&'static str> {
    match command {
        "fast" => {
            Some("Get the fast CLI from https://github.com/nikivdev/fast and ensure it is on PATH.")
        }
        _ => None,
    }
}

fn collect_flox_packages(
    cfg: &Config,
    deps: &[(String, FloxInstallSpec)],
) -> Vec<(String, FloxInstallSpec)> {
    let mut merged = std::collections::BTreeMap::new();
    if let Some(flox) = &cfg.flox {
        for (name, spec) in &flox.install {
            merged.insert(name.clone(), spec.clone());
        }
    }

    for (name, spec) in deps {
        merged.insert(name.clone(), spec.clone());
    }

    merged.into_iter().collect()
}

fn delegate_task_to_hub(
    task: &TaskConfig,
    deps: &ResolvedDependencies,
    workdir: &Path,
    host: IpAddr,
    port: u16,
    command: &str,
) -> Result<()> {
    ensure_hub_running(host, port)?;
    let url = format_task_submit_url(host, port);
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to construct HTTP client for hub delegation")?;

    let flox_specs: Vec<_> = deps
        .flox
        .iter()
        .map(|(name, spec)| json!({ "name": name, "spec": spec }))
        .collect();

    let payload = json!({
        "task": {
            "name": task.name,
            "command": command,
            "dependencies": {
                "commands": deps.commands,
                "flox": flox_specs,
            },
        },
        "cwd": workdir.to_string_lossy(),
        "flow_version": env!("CARGO_PKG_VERSION"),
    });

    let resp = client.post(&url).json(&payload).send().with_context(|| {
        format!(
            "failed to submit task to hub at {}",
            format_addr(host, port)
        )
    })?;

    let status = resp.status();
    if status.is_success() {
        println!(
            "Delegated task '{}' to hub at {}",
            task.name,
            format_addr(host, port)
        );
        Ok(())
    } else {
        let body = resp.text().unwrap_or_default();
        bail!(
            "hub returned {} while delegating task '{}': {}",
            status,
            task.name,
            body
        );
    }
}

fn ensure_hub_running(host: IpAddr, port: u16) -> Result<()> {
    let opts = HubOpts {
        host,
        port,
        config: None,
        no_ui: true,
        docs_hub: false,
    };
    let cmd = HubCommand {
        opts,
        action: Some(HubAction::Start),
    };
    hub::run(cmd)
}

fn format_addr(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}"),
    }
}

fn format_task_submit_url(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}/tasks/run"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}/tasks/run"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DependencySpec, FloxConfig, TaskResolutionConfig};
    use std::collections::HashMap;
    use std::path::Path;

    #[test]
    fn formats_task_lines_with_descriptions() {
        let tasks = vec![
            TaskConfig {
                name: "lint".to_string(),
                command: "golangci-lint run".to_string(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: Some("Run lint checks".to_string()),
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
            TaskConfig {
                name: "test".to_string(),
                command: "gotestsum ./...".to_string(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
        ];

        let lines = format_task_lines(&tasks);
        assert_eq!(
            lines,
            vec![
                " 1. lint – golangci-lint run".to_string(),
                "    Run lint checks".to_string(),
                " 2. test – gotestsum ./...".to_string(),
            ]
        );
    }

    fn discovered_task(scope: &str, relative_dir: &str, name: &str) -> discover::DiscoveredTask {
        discover::DiscoveredTask {
            task: TaskConfig {
                name: name.to_string(),
                command: format!("echo {}", name),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
            config_path: PathBuf::from(format!("{}/flow.toml", scope)),
            relative_dir: relative_dir.to_string(),
            depth: if relative_dir.is_empty() { 0 } else { 1 },
            scope: scope.to_string(),
            scope_aliases: vec![scope.to_ascii_lowercase()],
        }
    }

    #[test]
    fn parse_scoped_selector_supports_colon_and_slash() {
        assert_eq!(
            parse_scoped_selector("mobile:dev"),
            Some(("mobile".to_string(), "dev".to_string()))
        );
        assert_eq!(
            parse_scoped_selector("mobile/dev"),
            Some(("mobile".to_string(), "dev".to_string()))
        );
        assert!(parse_scoped_selector("dev").is_none());
    }

    #[test]
    fn resolve_ambiguous_task_match_uses_route_then_preferred_scope() {
        let mobile = discovered_task("mobile", "mobile", "dev");
        let root = discovered_task("root", "", "dev");
        let matches = vec![&mobile, &root];

        let mut cfg = Config::default();
        cfg.task_resolution = Some(TaskResolutionConfig {
            preferred_scopes: vec!["root".to_string()],
            routes: HashMap::from([(String::from("dev"), String::from("mobile"))]),
            warn_on_implicit_scope: Some(false),
        });

        let selected =
            resolve_ambiguous_task_match("dev", &matches, Some(&cfg)).expect("route should pick");
        assert_eq!(selected.scope, "mobile");

        cfg.task_resolution = Some(TaskResolutionConfig {
            preferred_scopes: vec!["root".to_string()],
            routes: HashMap::new(),
            warn_on_implicit_scope: Some(false),
        });
        let selected = resolve_ambiguous_task_match("dev", &matches, Some(&cfg))
            .expect("preferred scope should pick");
        assert_eq!(selected.scope, "root");
    }

    #[test]
    fn select_discovered_task_allows_exact_names_with_scope_delimiters() {
        let scoped = discovered_task("mobile", "mobile", "run");
        let exact = discovered_task("root", "", "mobile:dev");
        let discovery = discover::DiscoveryResult {
            tasks: vec![scoped, exact],
            root_config: None,
            root_cfg: None,
        };

        let selected = select_discovered_task(&discovery, "mobile:dev")
            .expect("selection should succeed")
            .expect("exact task should resolve");
        assert_eq!(selected.scope, "root");
        assert_eq!(selected.task.name, "mobile:dev");
    }

    #[test]
    fn format_discovered_task_lines_prefixes_scope() {
        let entries = vec![discovered_task("mobile", "mobile", "dev")];
        let ai_entries: Vec<ai_tasks::DiscoveredAiTask> = Vec::new();
        let lines = format_discovered_task_lines(&entries, &ai_entries);
        assert!(lines[0].contains("mobile:dev"));
    }

    #[test]
    fn run_rejects_empty_commands() {
        let task = TaskConfig {
            name: "empty".into(),
            command: "".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: Vec::new(),
            description: None,
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        };
        let empty_args: Vec<String> = Vec::new();
        let err = execute_task(
            &task,
            Path::new("flow.toml"),
            Path::new("."),
            String::new(),
            None,
            &[],
            false,
            "",
            &empty_args,
            &task.name,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("empty command"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn collects_dependency_commands() {
        let mut cfg = Config::default();
        cfg.dependencies
            .insert("fast".into(), DependencySpec::Single("fast".into()));
        cfg.dependencies.insert(
            "toolkit".into(),
            DependencySpec::Multiple(vec!["rg".into(), "fd".into()]),
        );

        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["fast".into(), "toolkit".into()],
            description: None,
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        };

        let resolved = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert_eq!(
            resolved.commands,
            vec!["fast".to_string(), "rg".to_string(), "fd".to_string()]
        );
        assert!(resolved.flox.is_empty());
    }

    #[test]
    fn collects_flox_dependencies_from_dependency_table() {
        let mut cfg = Config::default();
        cfg.dependencies.insert(
            "ripgrep".into(),
            DependencySpec::Flox(FloxInstallSpec {
                pkg_path: "ripgrep".into(),
                pkg_group: None,
                version: None,
                systems: None,
                priority: None,
            }),
        );

        let task = TaskConfig {
            name: "search".into(),
            command: "rg TODO".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["ripgrep".into()],
            description: None,
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        };

        let resolved = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert!(resolved.commands.is_empty());
        assert_eq!(resolved.flox.len(), 1);
        assert_eq!(resolved.flox[0].0, "ripgrep");
        assert_eq!(resolved.flox[0].1.pkg_path, "ripgrep");
    }

    #[test]
    fn collects_flox_dependencies_from_flox_config() {
        let mut cfg = Config::default();
        let mut install = std::collections::HashMap::new();
        install.insert(
            "node".to_string(),
            FloxInstallSpec {
                pkg_path: "nodejs".into(),
                pkg_group: None,
                version: None,
                systems: None,
                priority: None,
            },
        );
        cfg.flox = Some(FloxConfig { install });

        let task = TaskConfig {
            name: "dev".into(),
            command: "npm start".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["node".into()],
            description: None,
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        };

        let resolved = resolve_task_dependencies(&task, &cfg).expect("dependencies should resolve");
        assert!(resolved.commands.is_empty());
        assert_eq!(resolved.flox.len(), 1);
        assert_eq!(resolved.flox[0].0, "node");
        assert_eq!(resolved.flox[0].1.pkg_path, "nodejs");
    }

    #[test]
    fn errors_on_missing_dependencies() {
        let cfg = Config::default();
        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["unknown".into()],
            description: None,
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        };

        let err = resolve_task_dependencies(&task, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("references unknown dependencies"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn errors_when_dependency_not_declared_in_table() {
        let mut cfg = Config::default();
        cfg.dependencies
            .insert("fast".into(), DependencySpec::Single("fast".into()));
        let task = TaskConfig {
            name: "ci".into(),
            command: "ci".into(),
            delegate_to_hub: false,
            activate_on_cd_to_root: false,
            dependencies: vec!["unknown".into()],
            description: None,
            shortcuts: Vec::new(),
            interactive: false,
            confirm_on_match: false,
            on_cancel: None,
            output_file: None,
        };

        let err = resolve_task_dependencies(&task, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("references unknown dependencies"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn find_task_matches_shortcuts_and_abbreviations() {
        let mut cfg = Config::default();
        cfg.tasks = vec![
            TaskConfig {
                name: "deploy-cli-release".into(),
                command: "echo deploy".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: vec!["dcr-alias".into()],
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
            TaskConfig {
                name: "dev-hub".into(),
                command: "echo dev".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
        ];

        let task = find_task(&cfg, "dcr-alias").expect("shortcut should resolve");
        assert_eq!(task.name, "deploy-cli-release");

        let task = find_task(&cfg, "dcr").expect("abbreviation should resolve");
        assert_eq!(task.name, "deploy-cli-release");

        let task = find_task(&cfg, "dev-hub").expect("exact match should resolve");
        assert_eq!(task.name, "dev-hub");

        let task = find_task(&cfg, "DH").expect("case-insensitive match should resolve");
        assert_eq!(task.name, "dev-hub");
    }

    #[test]
    fn ambiguous_abbreviations_do_not_match() {
        let mut cfg = Config::default();
        cfg.tasks = vec![
            TaskConfig {
                name: "deploy-cli-release".into(),
                command: "echo deploy".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
            TaskConfig {
                name: "deploy-core-runner".into(),
                command: "echo runner".into(),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
        ];

        assert!(
            find_task(&cfg, "dcr").is_none(),
            "abbreviation should be ambiguous"
        );
    }

    #[test]
    fn detects_command_arg_references() {
        // Should detect $@, $*, $1, $2, etc.
        assert!(command_references_args("echo $@"));
        assert!(command_references_args("echo $*"));
        assert!(command_references_args("echo $1"));
        assert!(command_references_args("echo $9"));
        assert!(command_references_args("bash -c 'echo $@' --"));
        assert!(command_references_args("script.sh \"$1\" \"$2\""));
        assert!(command_references_args("echo ${1}"));
        assert!(command_references_args("echo ${@}"));

        // Should not detect other $ variables
        assert!(!command_references_args("echo $HOME"));
        assert!(!command_references_args("echo $0")); // $0 is script name, not arg
        assert!(!command_references_args("echo ${HOME}"));
        assert!(!command_references_args("echo $$")); // PID
        assert!(!command_references_args("echo $?")); // exit code
        assert!(!command_references_args(
            "source .env && bun script.ts --delete"
        ));
    }
}
