use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cli::{KillOpts, ProcessOpts, TaskLogsOpts};
use crate::projects;
use crate::running;
use crate::tasks;

/// Show running processes for a project (or all projects)
pub fn show_project_processes(opts: ProcessOpts) -> Result<()> {
    if opts.all {
        show_all_processes()
    } else {
        let (config_path, cfg) = tasks::load_project_config(opts.config)?;
        let canonical = config_path.canonicalize()?;
        show_processes_for_project(&canonical, cfg.project_name.as_deref())
    }
}

fn show_processes_for_project(config_path: &Path, project_name: Option<&str>) -> Result<()> {
    let processes = running::get_project_processes(config_path)?;
    let project_root = config_path.parent().unwrap_or(Path::new("."));

    match project_name {
        Some(name) => println!("Project: {} ({})", name, project_root.display()),
        None => println!("Project: {}", project_root.display()),
    }

    if processes.is_empty() {
        println!("No running flow processes.");
        return Ok(());
    }

    println!("Running processes:");
    for proc in &processes {
        let runtime = format_runtime(proc.started_at);
        println!(
            "  {} [pid: {}, pgid: {}] - {}",
            proc.task_name, proc.pid, proc.pgid, runtime
        );
        println!("    {}", proc.command);
        if proc.used_flox {
            println!("    (flox environment)");
        }
    }

    Ok(())
}

fn show_all_processes() -> Result<()> {
    let all = running::load_running_processes()?;

    if all.projects.is_empty() {
        println!("No running flow processes.");
        return Ok(());
    }

    for (config_path, processes) in &all.projects {
        let project_name = processes
            .first()
            .and_then(|p| p.project_name.as_deref())
            .unwrap_or("unknown");
        let project_root = Path::new(config_path)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| config_path.clone());

        println!("\n{} ({}):", project_name, project_root);
        for proc in processes {
            let runtime = format_runtime(proc.started_at);
            println!("  {} [pid: {}] - {}", proc.task_name, proc.pid, runtime);
        }
    }

    Ok(())
}

fn format_runtime(started_at: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let elapsed_secs = ((now.saturating_sub(started_at)) / 1000) as u64;

    if elapsed_secs < 60 {
        format!("{}s", elapsed_secs)
    } else if elapsed_secs < 3600 {
        format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
    } else {
        format!("{}h {}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
    }
}

/// Kill processes based on options
pub fn kill_processes(opts: KillOpts) -> Result<()> {
    let (config_path, _cfg) = tasks::load_project_config(opts.config)?;
    let canonical = config_path.canonicalize()?;

    if let Some(pid) = opts.pid {
        kill_by_pid(pid, opts.force, opts.timeout)
    } else if let Some(task) = &opts.task {
        kill_by_task(&canonical, task, opts.force, opts.timeout)
    } else if opts.all {
        kill_all_for_project(&canonical, opts.force, opts.timeout)
    } else {
        bail!("Specify a task name, --pid <pid>, or --all")
    }
}

fn kill_by_pid(pid: u32, force: bool, timeout: u64) -> Result<()> {
    let processes = running::load_running_processes()?;

    // Find the process entry to get its PGID
    let entry = processes.projects.values().flatten().find(|p| p.pid == pid);

    let pgid = entry.map(|e| e.pgid).unwrap_or(pid);
    let task_name = entry.map(|e| e.task_name.as_str()).unwrap_or("unknown");

    terminate_process_group(pgid, force, timeout)?;
    running::unregister_process(pid)?;

    println!("Killed {} (pid: {}, pgid: {})", task_name, pid, pgid);
    Ok(())
}

fn kill_by_task(config_path: &Path, task: &str, force: bool, timeout: u64) -> Result<()> {
    let processes = running::get_project_processes(config_path)?;
    let matching: Vec<_> = processes.iter().filter(|p| p.task_name == task).collect();

    if matching.is_empty() {
        bail!("No running process found for task '{}'", task);
    }

    for proc in matching {
        terminate_process_group(proc.pgid, force, timeout)?;
        running::unregister_process(proc.pid)?;
        println!("Killed {} (pid: {})", proc.task_name, proc.pid);
    }

    Ok(())
}

fn kill_all_for_project(config_path: &Path, force: bool, timeout: u64) -> Result<()> {
    let processes = running::get_project_processes(config_path)?;

    if processes.is_empty() {
        println!("No running processes to kill.");
        return Ok(());
    }

    for proc in &processes {
        terminate_process_group(proc.pgid, force, timeout)?;
        running::unregister_process(proc.pid)?;
        println!("Killed {} (pid: {})", proc.task_name, proc.pid);
    }

    Ok(())
}

fn terminate_process_group(pgid: u32, force: bool, timeout: u64) -> Result<()> {
    #[cfg(unix)]
    {
        if force {
            // Immediate SIGKILL to process group
            Command::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", pgid))
                .status()
                .context("failed to send SIGKILL")?;
        } else {
            // Graceful SIGTERM to process group
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(format!("-{}", pgid))
                .status();

            // Wait for process to exit
            for _ in 0..timeout {
                thread::sleep(Duration::from_secs(1));
                if !running::process_alive(pgid) {
                    return Ok(());
                }
            }

            // Force kill if still alive
            if running::process_alive(pgid) {
                Command::new("kill")
                    .arg("-KILL")
                    .arg(format!("-{}", pgid))
                    .status()
                    .context("failed to send SIGKILL after timeout")?;
            }
        }
    }

    #[cfg(windows)]
    {
        Command::new("taskkill")
            .args(["/PID", &pgid.to_string(), "/T", "/F"])
            .status()
            .context("failed to kill process tree")?;
    }

    Ok(())
}

// ============================================================================
// Task Logs
// ============================================================================

fn log_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/logs")
}

fn sanitize_component(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
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

fn project_slug(project_root: &Path, project_name: Option<&str>) -> String {
    let project_root_key = project_root.display().to_string();
    let project_root_hash = short_hash(&project_root_key);
    match project_name {
        Some(name) => {
            let clean = sanitize_component(name);
            if clean.is_empty() {
                format!("proj-{project_root_hash}")
            } else {
                format!("{clean}-{project_root_hash}")
            }
        }
        None => format!("proj-{project_root_hash}"),
    }
}

/// Get the log path for a project/task
fn get_log_path(project_root: &Path, project_name: Option<&str>, task_name: &str) -> PathBuf {
    let base = log_dir();
    let slug = project_slug(project_root, project_name);

    let task = {
        let clean = sanitize_component(task_name);
        if clean.is_empty() {
            "task".to_string()
        } else {
            clean
        }
    };

    base.join(slug).join(format!("{task}.log"))
}

/// Show task logs
pub fn show_task_logs(opts: TaskLogsOpts) -> Result<()> {
    // If task_id is provided, fetch from hub
    if let Some(ref task_id) = opts.task_id {
        return show_hub_task_logs(task_id, opts.follow);
    }

    if opts.list {
        return list_available_logs(opts.all);
    }

    if opts.all {
        return show_all_logs(opts.lines);
    }

    // Resolve project: --project flag > flow.toml in cwd > active project
    let (project_root, config_path, project_name) = if let Some(ref name) = opts.project {
        // Explicit project name
        match projects::resolve_project(name)? {
            Some(entry) => (entry.project_root, entry.config_path, Some(entry.name)),
            None => {
                bail!(
                    "Project '{}' not found. Use `f projects` to see registered projects.",
                    name
                );
            }
        }
    } else if opts.config.exists() {
        // flow.toml in current directory
        let (cfg_path, cfg) = tasks::load_project_config(opts.config.clone())?;
        let canonical = cfg_path.canonicalize().unwrap_or_else(|_| cfg_path.clone());
        let root = cfg_path
            .parent()
            .unwrap_or(Path::new("."))
            .canonicalize()
            .unwrap_or_else(|_| cfg_path.parent().unwrap_or(Path::new(".")).to_path_buf());
        (root, canonical, cfg.project_name)
    } else if let Some(active) = projects::get_active_project() {
        // Fall back to active project
        match projects::resolve_project(&active)? {
            Some(entry) => (entry.project_root, entry.config_path, Some(entry.name)),
            None => {
                bail!(
                    "Active project '{}' not found. Use `f projects` to see registered projects.",
                    active
                );
            }
        }
    } else {
        bail!(
            "No flow.toml in current directory and no active project set.\nRun a task in a project first, or use: f logs -p <project>"
        );
    };

    // If no task specified, try to find available logs - prefer running tasks
    let task_name = match opts.task {
        Some(name) => name,
        None => {
            let logs = get_project_log_files(&project_root, project_name.as_deref());

            if logs.is_empty() {
                println!("No logs found for this project.");
                return Ok(());
            }

            // Check for running tasks
            let running = running::get_project_processes(&config_path).unwrap_or_default();
            let running_tasks: Vec<_> = running.iter().map(|p| p.task_name.clone()).collect();
            let running_logs: Vec<_> = logs
                .iter()
                .filter(|log| running_tasks.contains(log))
                .cloned()
                .collect();

            if running_logs.len() == 1 {
                // Single running task - use it
                running_logs[0].clone()
            } else if running_logs.len() > 1 {
                // Multiple running tasks
                println!("Multiple running tasks. Specify which to view:");
                for log in &running_logs {
                    println!("  f logs {}", log);
                }
                return Ok(());
            } else if logs.len() == 1 {
                // No running tasks, but only one log file
                logs[0].clone()
            } else {
                // No running tasks, multiple log files
                println!("No running tasks. Available logs:");
                for log in &logs {
                    println!("  f logs {}", log);
                }
                return Ok(());
            }
        }
    };

    let log_path = get_log_path(&project_root, project_name.as_deref(), &task_name);

    if !log_path.exists() {
        bail!(
            "No log file found for task '{}' at {}",
            task_name,
            log_path.display()
        );
    }

    if opts.follow {
        tail_follow(&log_path, opts.lines, opts.quiet)?;
    } else {
        tail_lines(&log_path, opts.lines)?;
    }

    Ok(())
}

fn show_all_logs(lines: usize) -> Result<()> {
    let base = log_dir();
    if !base.exists() {
        println!("No logs found at {}", base.display());
        return Ok(());
    }

    // Find the most recently modified log file
    let mut newest: Option<(PathBuf, u64)> = None;

    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            for log_entry in fs::read_dir(&path)? {
                let log_entry = log_entry?;
                let log_path = log_entry.path();
                if log_path.extension().map(|e| e == "log").unwrap_or(false) {
                    if let Ok(meta) = fs::metadata(&log_path) {
                        let modified = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);

                        if newest.as_ref().map(|(_, t)| modified > *t).unwrap_or(true) {
                            newest = Some((log_path, modified));
                        }
                    }
                }
            }
        }
    }

    match newest {
        Some((path, _)) => {
            println!("Showing most recent log: {}\n", path.display());
            tail_lines(&path, lines)
        }
        None => {
            println!("No log files found.");
            Ok(())
        }
    }
}

fn list_available_logs(_all: bool) -> Result<()> {
    let base = log_dir();
    if !base.exists() {
        println!("No logs found at {}", base.display());
        return Ok(());
    }

    println!("Available logs in {}:", base.display());

    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let project_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            println!("\n{}:", project_name);

            for log_entry in fs::read_dir(&path)? {
                let log_entry = log_entry?;
                let log_path = log_entry.path();
                if log_path.extension().map(|e| e == "log").unwrap_or(false) {
                    let task_name = log_path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("unknown");
                    let metadata = fs::metadata(&log_path)?;
                    let size = metadata.len();
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let age = format_relative_time(now.saturating_sub(modified));

                    println!("  {} ({} bytes, modified {})", task_name, size, age);
                }
            }
        }
    }

    Ok(())
}

fn format_relative_time(seconds: u64) -> String {
    if seconds < 60 {
        format!("{}s ago", seconds)
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}

/// Get list of task names that have log files for a project
fn get_project_log_files(project_root: &Path, project_name: Option<&str>) -> Vec<String> {
    let base = log_dir();
    let slug = project_slug(project_root, project_name);

    let project_log_dir = base.join(&slug);
    if !project_log_dir.exists() {
        return Vec::new();
    }

    let mut tasks = Vec::new();
    if let Ok(entries) = fs::read_dir(&project_log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "log").unwrap_or(false) {
                if let Some(task_name) = path.file_stem().and_then(|n| n.to_str()) {
                    tasks.push(task_name.to_string());
                }
            }
        }
    }
    tasks
}

fn tail_lines(path: &Path, n: usize) -> Result<()> {
    let file = File::open(path).context("failed to open log file")?;
    let reader = BufReader::new(file);
    let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();

    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{}", line);
    }

    Ok(())
}

fn tail_follow(path: &Path, initial_lines: usize, quiet: bool) -> Result<()> {
    // First show the last N lines
    tail_lines(path, initial_lines)?;

    // Then follow
    let mut file = File::open(path).context("failed to open log file")?;
    file.seek(SeekFrom::End(0))?;

    if !quiet {
        println!("\n--- Following {} (Ctrl+C to stop) ---", path.display());
    }

    let mut buf = vec![0u8; 4096];
    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                // No new data, sleep and retry
                thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                print!("{}", String::from_utf8_lossy(&buf[..n]));
            }
            Err(e) => {
                bail!("Error reading log file: {}", e);
            }
        }
    }
}

/// Fetch and display logs for a hub task by ID
fn show_hub_task_logs(task_id: &str, follow: bool) -> Result<()> {
    use reqwest::blocking::Client;
    use serde::Deserialize;

    const HUB_HOST: &str = "127.0.0.1";
    const HUB_PORT: u16 = 9050;

    #[derive(Debug, Deserialize)]
    struct TaskLog {
        id: String,
        name: String,
        command: String,
        cwd: Option<String>,
        #[allow(dead_code)]
        started_at: u64,
        finished_at: Option<u64>,
        exit_code: Option<i32>,
        output: Vec<OutputLine>,
    }

    #[derive(Debug, Deserialize)]
    struct OutputLine {
        #[allow(dead_code)]
        timestamp_ms: u64,
        stream: String,
        line: String,
    }

    let url = format!("http://{}:{}/tasks/logs/{}", HUB_HOST, HUB_PORT, task_id);

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to create HTTP client")?;

    if follow {
        // Poll for updates
        let mut last_output_count = 0;

        loop {
            let resp = client.get(&url).send();

            match resp {
                Ok(r) if r.status().is_success() => {
                    let log: TaskLog = r.json().context("failed to parse task log")?;

                    // Print new output lines
                    for line in log.output.iter().skip(last_output_count) {
                        let prefix = if line.stream == "stderr" { "!" } else { " " };
                        println!("{} {}", prefix, line.line);
                    }
                    last_output_count = log.output.len();

                    // Check if task is done
                    if log.finished_at.is_some() {
                        if let Some(code) = log.exit_code {
                            if code == 0 {
                                println!("\n✓ Task completed successfully");
                            } else {
                                println!("\n✗ Task failed with exit code {}", code);
                            }
                        }
                        break;
                    }
                }
                Ok(r) if r.status().as_u16() == 404 => {
                    // Task not found yet, wait
                    thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Ok(r) => {
                    bail!("Hub returned error: {}", r.status());
                }
                Err(e) => {
                    bail!("Failed to fetch task logs: {}", e);
                }
            }

            thread::sleep(Duration::from_millis(500));
        }
    } else {
        // One-shot fetch
        let resp = client
            .get(&url)
            .send()
            .context("failed to fetch task logs")?;

        if resp.status().as_u16() == 404 {
            println!(
                "Task '{}' not found yet (queued). Streaming logs...",
                task_id
            );
            return show_hub_task_logs(task_id, true);
        }

        if !resp.status().is_success() {
            bail!("Hub returned error: {}", resp.status());
        }

        let log: TaskLog = resp.json().context("failed to parse task log")?;

        println!("Task: {} ({})", log.name, log.id);
        println!("Command: {}", log.command);
        if let Some(cwd) = &log.cwd {
            println!("Working dir: {}", cwd);
        }
        println!();

        for line in &log.output {
            let prefix = if line.stream == "stderr" { "!" } else { " " };
            println!("{} {}", prefix, line.line);
        }

        if let Some(code) = log.exit_code {
            println!();
            if code == 0 {
                println!("✓ Exit code: {}", code);
            } else {
                println!("✗ Exit code: {}", code);
            }
        } else {
            println!("\n⋯ Task still running...");
        }
    }

    Ok(())
}
