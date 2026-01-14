use std::{
    collections::hash_map::DefaultHasher,
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
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde_json::json;
use shell_words;
use which::which;

use crate::{
    cli::{
        GlobalAction, GlobalCommand, HubAction, HubCommand, HubOpts, TaskActivateOpts, TaskRunOpts,
        TasksOpts,
    },
    config::{self, Config, FloxInstallSpec, TaskConfig},
    discover,
    flox::{self, FloxEnv},
    history::{self, InvocationRecord},
    hub, init, jazz_state, projects,
    running::{self, RunningProcess},
    task_match,
};

/// Global state for cancel cleanup handler.
static CANCEL_HANDLER_SET: AtomicBool = AtomicBool::new(false);

/// Cleanup state shared with the signal handler.
struct CleanupState {
    command: Option<String>,
    workdir: PathBuf,
}

static CLEANUP_STATE: std::sync::OnceLock<Mutex<CleanupState>> = std::sync::OnceLock::new();

/// Run the cleanup command if one is set.
fn run_cleanup() {
    let state = CLEANUP_STATE.get_or_init(|| {
        Mutex::new(CleanupState {
            command: None,
            workdir: PathBuf::from("."),
        })
    });

    if let Ok(guard) = state.lock() {
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
        })
    });

    // Update the cleanup state
    if let Ok(mut guard) = state.lock() {
        guard.command = on_cancel.map(|s| s.to_string());
        guard.workdir = workdir.to_path_buf();
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
        })
    });

    if let Ok(mut guard) = state.lock() {
        guard.command = None;
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
    ];

    let first_line = command.lines().next().unwrap_or("").trim();
    let first_word = first_line.split_whitespace().next().unwrap_or("");
    let base_cmd = first_word.rsplit('/').next().unwrap_or(first_word);

    interactive_commands.contains(&base_cmd)
}

pub fn list(opts: TasksOpts) -> Result<()> {
    // Determine root directory for discovery
    let root = if opts.config.is_absolute() {
        opts.config
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };

    let discovery = discover::discover_tasks(&root)?;

    if discovery.tasks.is_empty() {
        println!("No tasks defined in {} or subdirectories", root.display());
        return Ok(());
    }

    println!("Tasks (root: {}):", root.display());
    for line in format_discovered_task_lines(&discovery.tasks) {
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
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let discovery = discover::discover_tasks(&root)?;

    // Find the task in discovered tasks
    let discovered = discovery.tasks.iter().find(|d| {
        d.task.name.eq_ignore_ascii_case(task_name)
            || d.task
                .shortcuts
                .iter()
                .any(|s| s.eq_ignore_ascii_case(task_name))
    });

    // Also try abbreviation matching
    let discovered = discovered.or_else(|| {
        let needle = task_name.to_ascii_lowercase();
        if needle.len() < 2 {
            return None;
        }
        let mut matches = discovery.tasks.iter().filter(|d| {
            generate_abbreviation(&d.task.name)
                .map(|abbr| abbr == needle)
                .unwrap_or(false)
        });
        let first = matches.next()?;
        // Only match if unambiguous
        if matches.next().is_some() {
            None
        } else {
            Some(first)
        }
    });

    if let Some(discovered) = discovered {
        // Run the task with its specific config path
        return run(TaskRunOpts {
            config: discovered.config_path.clone(),
            delegate_to_hub: false,
            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
            hub_port: 9050,
            name: discovered.task.name.clone(),
            args,
        });
    }

    // List available tasks in error message
    let available: Vec<_> = discovery
        .tasks
        .iter()
        .map(|d| {
            if d.relative_dir.is_empty() {
                d.task.name.clone()
            } else {
                format!("{} ({})", d.task.name, d.relative_dir)
            }
        })
        .collect();
    bail!(
        "task '{}' not found.\nAvailable tasks: {}",
        task_name,
        available.join(", ")
    );
}

pub fn run(opts: TaskRunOpts) -> Result<()> {
    let config_path_for_deps = opts.config.clone();
    let (config_path, cfg) = load_project_config(opts.config)?;
    let project_name = cfg.project_name.clone();
    let workdir = config_path.parent().unwrap_or(Path::new("."));

    // Set active project when running a task
    if let Some(ref name) = project_name {
        let _ = projects::set_active_project(name);
    }

    let Some(task) = find_task(&cfg, &opts.name) else {
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
    let config_path = resolve_path(path)?;
    if !config_path.exists() {
        let is_default =
            config_path.file_name().and_then(|name| name.to_str()) == Some("flow.toml");
        if is_default {
            init::write_template(&config_path)?;
            println!("Created starter flow.toml at {}", config_path.display());
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

fn format_discovered_task_lines(tasks: &[discover::DiscoveredTask]) -> Vec<String> {
    let mut lines = Vec::new();
    for (idx, discovered) in tasks.iter().enumerate() {
        let task = &discovered.task;
        let shortcut_display = if task.shortcuts.is_empty() {
            String::new()
        } else {
            format!(" [{}]", task.shortcuts.join(", "))
        };

        // Show relative path for nested tasks
        let path_suffix = if let Some(path_label) = discovered.path_label() {
            format!(" ({})", path_label)
        } else {
            String::new()
        };

        lines.push(format!(
            "{:>2}. {}{}{} – {}",
            idx + 1,
            task.name,
            shortcut_display,
            path_suffix,
            task.command
        ));
        if let Some(desc) = &task.description {
            lines.push(format!("    {desc}"));
        }
    }
    lines
}

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

    if let Some(ref task_ctx) = ctx {
        let pgid = running::get_pgid(pid).unwrap_or(pid);
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

fn run_command_with_tee(cmd: Command, ctx: Option<TaskContext>) -> Result<(ExitStatus, String)> {
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

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let pgid = running::get_pgid(pid).unwrap_or(pid);
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

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let pgid = running::get_pgid(pid).unwrap_or(pid);
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

    // Thread to read PTY output and tee to stdout + capture
    let output_clone = output.clone();
    let log_file_clone = log_file.clone();
    let output_handle = thread::spawn(move || {
        let mut stdout = std::io::stdout();
        let mut buf = [0u8; 4096];
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

                    if let Ok(mut out) = output_clone.lock() {
                        out.push_str(&String::from_utf8_lossy(&buf[..n]));
                    }
                }
                Err(_) => break,
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

        // Register the process
        if let Some(ref task_ctx) = ctx {
            let pgid = running::get_pgid(pid).unwrap_or(pid);
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn command")?;

    let pid = child.id();

    // Register the process if we have task context
    if let Some(ref task_ctx) = ctx {
        let pgid = running::get_pgid(pid).unwrap_or(pid);
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
    let mut handles = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        handles.push(tee_stream(
            stdout,
            std::io::stdout(),
            output.clone(),
            log_file.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        handles.push(tee_stream(
            stderr,
            std::io::stderr(),
            output.clone(),
            log_file.clone(),
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

fn tee_stream<R, W>(
    mut reader: R,
    mut writer: W,
    buffer: Arc<Mutex<String>>,
    log_file: Option<Arc<Mutex<File>>>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
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

            if let Ok(mut buf) = buffer.lock() {
                buf.push_str(&String::from_utf8_lossy(&chunk[..read]));
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
    use crate::config::{DependencySpec, FloxConfig};
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
