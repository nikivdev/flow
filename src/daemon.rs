//! Generic daemon management for flow.
//!
//! Allows starting, stopping, and monitoring background daemons defined in flow.toml.

use std::{
    fs,
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::blocking::Client;

use crate::{
    cli::{DaemonAction, DaemonCommand},
    config::{self, DaemonConfig, DaemonRestartPolicy},
    env, supervisor,
};

/// Run the daemon command.
pub fn run(cmd: DaemonCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(DaemonAction::Status { name: None });
    let config_path = resolve_flow_toml_path();

    if supervisor::try_handle_daemon_action(&action, config_path.as_deref())? {
        return Ok(());
    }

    match action {
        DaemonAction::Start { name } => start_daemon(&name)?,
        DaemonAction::Stop { name } => stop_daemon(&name)?,
        DaemonAction::Restart { name } => {
            stop_daemon(&name).ok();
            std::thread::sleep(Duration::from_millis(500));
            start_daemon(&name)?;
        }
        DaemonAction::Status { name } => {
            if let Some(name) = name {
                show_status_for(&name)?;
            } else {
                show_status()?;
            }
        }
        DaemonAction::List => list_daemons()?,
    }

    Ok(())
}

/// Start a daemon by name.
pub fn start_daemon(name: &str) -> Result<()> {
    start_daemon_with_path(name, resolve_flow_toml_path().as_deref())
}

pub fn start_daemon_with_path(name: &str, config_path: Option<&Path>) -> Result<()> {
    let daemon = find_daemon_config_with_path(name, config_path)?;
    start_daemon_inner(&daemon)
}

fn start_daemon_inner(daemon: &DaemonConfig) -> Result<()> {
    // Check if already running
    if let Some(url) = daemon.effective_health_url() {
        if check_health(&url) {
            println!("✓ {} is already running", daemon.name);
            return Ok(());
        }
    }

    // Check if there's a stale PID
    if let Some(pid) = load_daemon_pid(&daemon.name)? {
        if process_alive(pid)? {
            terminate_process(pid).ok();
        }
        remove_daemon_pid(&daemon.name).ok();
    }

    // Find the binary
    let binary = find_binary(&daemon.binary)?;

    println!(
        "Starting {} using {}{}",
        daemon.name,
        binary.display(),
        daemon
            .command
            .as_ref()
            .map(|c| format!(" {}", c))
            .unwrap_or_default()
    );

    let spawned = spawn_daemon_process(daemon, &binary)?;
    persist_daemon_pid(&daemon.name, spawned.pid)?;

    // Wait a moment and check health
    wait_for_daemon_ready(daemon, &spawned.stdout_log)?;

    if let Some(url) = daemon.effective_health_url() {
        if check_health(&url) {
            println!("✓ {} started successfully", daemon.name);
        } else {
            println!(
                "⚠ {} started but health check failed (may need more time)",
                daemon.name
            );
        }
    } else {
        println!("✓ {} started (no health check configured)", daemon.name);
    }

    Ok(())
}

/// Stop a daemon by name.
pub fn stop_daemon(name: &str) -> Result<()> {
    stop_daemon_with_path(name, resolve_flow_toml_path().as_deref())
}

pub fn stop_daemon_with_path(name: &str, config_path: Option<&Path>) -> Result<()> {
    let daemon = find_daemon_config_with_path(name, config_path).ok();

    if let Some(pid) = load_daemon_pid(name)? {
        if process_alive(pid)? {
            terminate_process(pid)?;
            println!("✓ {} stopped (PID {})", name, pid);
        } else {
            println!("✓ {} was not running", name);
        }
        remove_daemon_pid(name).ok();
    } else {
        println!("✓ {} was not running (no PID file)", name);
    }

    // Also try to kill any process listening on the daemon's port
    // This handles cases where child processes outlive the parent
    if let Some(daemon) = daemon {
        if let Some(port) = daemon.port {
            kill_process_on_port(port).ok();
        } else if let Some(url) = &daemon.health_url {
            if let Some(port) = extract_port_from_url(url) {
                kill_process_on_port(port).ok();
            }
        }
    }

    Ok(())
}

/// Show status of all configured daemons.
pub fn show_status() -> Result<()> {
    show_status_with_path(resolve_flow_toml_path().as_deref())
}

/// Show status of a specific daemon.
pub fn show_status_for(name: &str) -> Result<()> {
    show_status_for_with_path(name, resolve_flow_toml_path().as_deref())
}

pub fn show_status_with_path(config_path: Option<&Path>) -> Result<()> {
    let config = load_merged_config_with_path(config_path)?;

    if config.daemons.is_empty() {
        println!("No daemons configured.");
        println!();
        println!("Add daemons to ~/.config/flow/flow.toml or project flow.toml:");
        println!();
        println!("  [[daemon]]");
        println!("  name = \"my-daemon\"");
        println!("  binary = \"my-app\"");
        println!("  command = \"serve\"");
        println!("  health_url = \"http://127.0.0.1:8080/health\"");
        return Ok(());
    }

    println!("Daemon Status:");
    println!();

    for daemon in &config.daemons {
        let status = get_daemon_status(&daemon);
        let icon = if status.running { "✓" } else { "✗" };
        let state = if status.running { "running" } else { "stopped" };

        print!("  {} {}: {}", icon, daemon.name, state);

        if let Some(url) = daemon.effective_health_url() {
            if status.running {
                print!(" ({})", url.replace("/health", ""));
            }
        }

        if let Some(pid) = status.pid {
            print!(" [PID {}]", pid);
        }

        println!();

        if let Some(desc) = &daemon.description {
            println!("      {}", desc);
        }
    }

    Ok(())
}

pub fn show_status_for_with_path(name: &str, config_path: Option<&Path>) -> Result<()> {
    let daemon = find_daemon_config_with_path(name, config_path)?;
    let status = get_daemon_status(&daemon);
    let icon = if status.running { "✓" } else { "✗" };
    let state = if status.running { "running" } else { "stopped" };

    println!("Daemon Status:");
    println!();
    print!("  {} {}: {}", icon, daemon.name, state);

    if let Some(url) = daemon.effective_health_url() {
        if status.running {
            if status.healthy == Some(false) {
                print!(" (unhealthy: {})", url.replace("/health", ""));
            } else {
                print!(" ({})", url.replace("/health", ""));
            }
        }
    }

    if let Some(pid) = status.pid {
        print!(" [PID {}]", pid);
    }

    println!();
    if let Some(desc) = &daemon.description {
        println!("      {}", desc);
    }

    Ok(())
}

/// List available daemons.
pub fn list_daemons() -> Result<()> {
    list_daemons_with_path(resolve_flow_toml_path().as_deref())
}

pub fn list_daemons_with_path(config_path: Option<&Path>) -> Result<()> {
    let config = load_merged_config_with_path(config_path)?;

    if config.daemons.is_empty() {
        println!("No daemons configured.");
        return Ok(());
    }

    println!("Available daemons:");
    println!();

    for daemon in &config.daemons {
        print!("  {}", daemon.name);
        if let Some(desc) = &daemon.description {
            print!(" - {}", desc);
        }
        println!();
    }

    Ok(())
}

/// Status of a daemon.
#[derive(Debug)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub healthy: Option<bool>,
}

/// Get the status of a specific daemon.
pub fn get_daemon_status(daemon: &DaemonConfig) -> DaemonStatus {
    let pid = load_daemon_pid(&daemon.name).ok().flatten();
    let pid_alive = pid
        .map(|pid| process_alive(pid).unwrap_or(false))
        .unwrap_or(false);

    let healthy = daemon.effective_health_url().map(|url| check_health(&url));
    let running = if healthy.is_some() {
        // Prefer PID when available; a transient health blip shouldn't mark the process as stopped.
        if pid.is_some() {
            pid_alive
        } else {
            healthy.unwrap_or(false)
        }
    } else {
        pid_alive
    };

    DaemonStatus {
        running,
        pid,
        healthy,
    }
}

pub fn restart_policy_for(daemon: &DaemonConfig) -> DaemonRestartPolicy {
    match daemon.restart {
        Some(ref policy) => policy.clone(),
        None => {
            if daemon.retry.unwrap_or(0) > 0 {
                DaemonRestartPolicy::OnFailure
            } else {
                DaemonRestartPolicy::Never
            }
        }
    }
}

pub fn daemon_log_dir(name: &str) -> Result<PathBuf> {
    let base = config::ensure_global_state_dir()?;
    let dir = base.join("daemons").join(sanitize_daemon_name(name));
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}

pub fn daemon_log_paths(name: &str) -> Result<(PathBuf, PathBuf)> {
    let dir = daemon_log_dir(name)?;
    Ok((dir.join("stdout.log"), dir.join("stderr.log")))
}

fn sanitize_daemon_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "daemon".to_string()
    } else {
        out
    }
}

struct SpawnedDaemon {
    pid: u32,
    stdout_log: PathBuf,
}

fn spawn_daemon_process(daemon: &DaemonConfig, binary: &Path) -> Result<SpawnedDaemon> {
    let mut cmd = Command::new(binary);

    if let Some(subcommand) = &daemon.command {
        cmd.arg(subcommand);
    }

    for arg in &daemon.args {
        cmd.arg(arg);
    }

    if let Some(wd) = &daemon.working_dir {
        let expanded = config::expand_path(wd);
        if expanded.exists() {
            cmd.current_dir(&expanded);
        }
    }

    if let Ok(vars) = env::fetch_local_personal_env_vars(&config::global_env_keys()) {
        for (key, value) in vars {
            cmd.env(key, value);
        }
    }

    for (key, value) in &daemon.env {
        cmd.env(key, value);
    }

    let (stdout_log, stderr_log) = daemon_log_paths(&daemon.name)?;
    let stdout_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)
        .with_context(|| format!("failed to open {}", stdout_log.display()))?;
    let stderr_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)
        .with_context(|| format!("failed to open {}", stderr_log.display()))?;

    cmd.stdin(std::process::Stdio::null())
        .stdout(stdout_file)
        .stderr(stderr_file);

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to start {} from {}", daemon.name, binary.display()))?;

    Ok(SpawnedDaemon {
        pid: child.id(),
        stdout_log,
    })
}

fn wait_for_daemon_ready(daemon: &DaemonConfig, stdout_log: &Path) -> Result<()> {
    if let Some(delay) = daemon.ready_delay {
        std::thread::sleep(Duration::from_millis(delay));
    } else {
        std::thread::sleep(Duration::from_millis(500));
    }

    let Some(pattern) = daemon.ready_output.as_ref() else {
        return Ok(());
    };

    let regex = Regex::new(pattern).with_context(|| "invalid ready_output regex")?;
    let timeout = Duration::from_secs(30);
    let start = std::time::Instant::now();
    let mut seen_len = 0usize;

    while start.elapsed() < timeout {
        if let Ok(contents) = fs::read_to_string(stdout_log) {
            if contents.len() > seen_len {
                let slice = &contents[seen_len..];
                if regex.is_match(slice) {
                    return Ok(());
                }
                seen_len = contents.len();
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    eprintln!(
        "WARN ready_output '{}' not found for {} (continuing).",
        pattern, daemon.name
    );
    Ok(())
}
/// Find a daemon config by name from merged configs.
fn find_daemon_config_with_path(name: &str, config_path: Option<&Path>) -> Result<DaemonConfig> {
    let config = load_merged_config_with_path(config_path)?;

    config
        .daemons
        .into_iter()
        .find(|d| d.name == name)
        .ok_or_else(|| anyhow::anyhow!("daemon '{}' not found in config", name))
}

/// Load merged config from global and local sources.
pub fn load_merged_config_with_path(config_path: Option<&Path>) -> Result<config::Config> {
    let mut merged = config::Config::default();

    // Load global config
    let global_path = config::default_config_path();
    if global_path.exists() {
        if let Ok(global_cfg) = config::load(&global_path) {
            merged.daemons.extend(global_cfg.daemons);
        }
    }

    // Load local config if it exists
    if let Some(local_path) = config_path {
        if local_path.exists() {
            if let Ok(local_cfg) = config::load(local_path) {
                merged.daemons.extend(local_cfg.daemons);
            }
        }
    }

    Ok(merged)
}

fn resolve_flow_toml_path() -> Option<PathBuf> {
    let mut current = std::env::current_dir().ok()?;
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

/// Find a binary on PATH or as an absolute path.
fn find_binary(name: &str) -> Result<PathBuf> {
    // If it's an absolute path, use it directly
    let path = Path::new(name);
    if path.is_absolute() && path.exists() {
        return Ok(path.to_path_buf());
    }

    // Expand ~ if present
    let expanded = config::expand_path(name);
    if expanded.exists() {
        return Ok(expanded);
    }

    // Try to find on PATH using `which`
    let output = Command::new("which")
        .arg(name)
        .output()
        .with_context(|| format!("failed to find binary '{}'", name))?;

    if output.status.success() {
        let path_str = String::from_utf8_lossy(&output.stdout);
        let path = PathBuf::from(path_str.trim());
        if path.exists() {
            return Ok(path);
        }
    }

    bail!("binary '{}' not found", name)
}

/// Check if a health endpoint is responding.
fn check_health(url: &str) -> bool {
    let client = Client::builder()
        .timeout(Duration::from_millis(750))
        .build();

    let Ok(client) = client else {
        return false;
    };

    client
        .get(url)
        .send()
        .and_then(|resp| resp.error_for_status())
        .map(|_| true)
        .unwrap_or(false)
}

// ============================================================================
// PID file management
// ============================================================================

fn daemon_pid_path(name: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(format!(".config/flow/{}.pid", name))
    } else {
        PathBuf::from(format!(".config/flow/{}.pid", name))
    }
}

fn load_daemon_pid(name: &str) -> Result<Option<u32>> {
    let path = daemon_pid_path(name);
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let pid: u32 = contents.trim().parse().ok().unwrap_or(0);
    if pid == 0 { Ok(None) } else { Ok(Some(pid)) }
}

fn persist_daemon_pid(name: &str, pid: u32) -> Result<()> {
    let path = daemon_pid_path(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create pid dir {}", parent.display()))?;
    }
    fs::write(&path, pid.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn remove_daemon_pid(name: &str) -> Result<()> {
    let path = daemon_pid_path(name);
    if path.exists() {
        fs::remove_file(path).ok();
    }
    Ok(())
}

// ============================================================================
// Process management
// ============================================================================

fn process_alive(pid: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        let status = Command::new("kill").arg("-0").arg(pid.to_string()).status();
        return Ok(status.map(|s| s.success()).unwrap_or(false));
    }

    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .output()
            .context("failed to invoke tasklist")?;
        if !output.status.success() {
            return Ok(false);
        }
        let needle = pid.to_string();
        let body = String::from_utf8_lossy(&output.stdout);
        Ok(body.lines().any(|line| line.contains(&needle)))
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        // First try to kill the process group (negative PID)
        // This ensures child processes are also terminated
        let pgid_kill = Command::new("kill")
            .arg(format!("-{pid}"))
            .stderr(std::process::Stdio::null())
            .status();

        // Also kill the process directly
        let status = Command::new("kill")
            .arg(format!("{pid}"))
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to invoke kill command")?;

        // If either succeeded, we're good
        if status.success() || pgid_kill.map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }
        bail!(
            "kill command exited with status {}",
            status.code().unwrap_or(-1)
        );
    }

    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"]) // /T kills child processes too
            .status()
            .context("failed to invoke taskkill")?;
        if status.success() {
            return Ok(());
        }
        bail!(
            "taskkill exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
}

/// Extract port number from a URL like "http://127.0.0.1:7201/health"
fn extract_port_from_url(url: &str) -> Option<u16> {
    // Simple extraction: find the port after the last colon before any path
    let url = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let host_port = url.split('/').next()?;
    let port_str = host_port.rsplit(':').next()?;
    port_str.parse().ok()
}

/// Kill any process listening on the given port.
#[cfg(unix)]
fn kill_process_on_port(port: u16) -> Result<()> {
    // Use lsof to find the process
    let output = Command::new("lsof")
        .args(["-ti", &format!(":{}", port)])
        .output()
        .context("failed to run lsof")?;

    if !output.status.success() {
        return Ok(()); // No process found on port
    }

    let pids = String::from_utf8_lossy(&output.stdout);
    for pid_str in pids.lines() {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            terminate_process(pid).ok();
        }
    }

    Ok(())
}

#[cfg(windows)]
fn kill_process_on_port(port: u16) -> Result<()> {
    // Use netstat to find the process
    let output = Command::new("netstat")
        .args(["-ano"])
        .output()
        .context("failed to run netstat")?;

    if !output.status.success() {
        return Ok(());
    }

    let port_pattern = format!(":{}", port);
    let lines = String::from_utf8_lossy(&output.stdout);
    for line in lines.lines() {
        if line.contains(&port_pattern) && line.contains("LISTENING") {
            // Last column is PID
            if let Some(pid_str) = line.split_whitespace().last() {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    terminate_process(pid).ok();
                }
            }
        }
    }

    Ok(())
}
