use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A process started by flow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningProcess {
    /// Process ID of the main task process
    pub pid: u32,
    /// Process group ID (for killing child processes)
    pub pgid: u32,
    /// Name of the task from flow.toml
    pub task_name: String,
    /// Full command that was executed
    pub command: String,
    /// Timestamp when the process was started (ms since epoch)
    pub started_at: u128,
    /// Canonical path to the flow.toml that defines this task
    pub config_path: PathBuf,
    /// Canonical path to the project root directory
    pub project_root: PathBuf,
    /// Whether flox environment was used
    pub used_flox: bool,
    /// Optional project name from flow.toml
    #[serde(default)]
    pub project_name: Option<String>,
}

/// All running processes tracked by flow
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunningProcesses {
    /// Map from project config path to list of running processes
    pub projects: HashMap<String, Vec<RunningProcess>>,
}

/// Returns ~/.config/flow/running.json
pub fn running_processes_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/running.json")
}

/// Load running processes, validating that PIDs are still alive
pub fn load_running_processes() -> Result<RunningProcesses> {
    let path = running_processes_path();
    if !path.exists() {
        return Ok(RunningProcesses::default());
    }

    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut processes: RunningProcesses = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    // Validate and clean up dead processes
    let mut changed = false;
    for procs in processes.projects.values_mut() {
        let before = procs.len();
        procs.retain(|p| process_alive(p.pid));
        if procs.len() != before {
            changed = true;
        }
    }
    processes.projects.retain(|_, v| !v.is_empty());

    if changed {
        save_running_processes(&processes)?;
    }

    Ok(processes)
}

/// Atomically save running processes (write to temp, then rename)
pub fn save_running_processes(processes: &RunningProcesses) -> Result<()> {
    let path = running_processes_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let temp_path = path.with_extension("json.tmp");
    let contents = serde_json::to_string_pretty(processes)?;
    fs::write(&temp_path, contents)?;
    fs::rename(&temp_path, &path)?;

    Ok(())
}

/// Register a new running process
pub fn register_process(entry: RunningProcess) -> Result<()> {
    let mut processes = load_running_processes()?;
    let key = entry.config_path.display().to_string();
    processes.projects.entry(key).or_default().push(entry);
    save_running_processes(&processes)
}

/// Unregister a process by PID
pub fn unregister_process(pid: u32) -> Result<()> {
    let mut processes = load_running_processes()?;
    for procs in processes.projects.values_mut() {
        procs.retain(|p| p.pid != pid);
    }
    processes.projects.retain(|_, v| !v.is_empty());
    save_running_processes(&processes)
}

/// Get processes for a specific project
pub fn get_project_processes(config_path: &Path) -> Result<Vec<RunningProcess>> {
    let processes = load_running_processes()?;
    let key = config_path.display().to_string();
    Ok(processes.projects.get(&key).cloned().unwrap_or_default())
}

/// Check if a process is alive
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
}

/// Get process group ID for a PID
#[cfg(unix)]
pub fn get_pgid(pid: u32) -> Option<u32> {
    let output = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(not(unix))]
pub fn get_pgid(pid: u32) -> Option<u32> {
    Some(pid)
}

/// Get current timestamp in milliseconds
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
