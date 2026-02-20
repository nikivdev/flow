use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

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
    crate::config::global_state_dir().join("running.json")
}

struct RunningProcessesLock {
    path: PathBuf,
}

impl Drop for RunningProcessesLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path() -> PathBuf {
    crate::config::global_state_dir().join("running.json.lock")
}

fn lock_owner_pid(path: &Path) -> Option<u32> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("pid=") {
            if let Ok(pid) = value.trim().parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

fn lock_is_stale(path: &Path, max_age: Duration) -> bool {
    if let Some(owner_pid) = lock_owner_pid(path) {
        if !process_alive(owner_pid) {
            return true;
        }
    }

    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = modified.elapsed() else {
        return false;
    };
    elapsed > max_age
}

fn acquire_running_processes_lock() -> Result<RunningProcessesLock> {
    let _ = crate::config::ensure_global_state_dir()
        .with_context(|| "failed to create flow global state dir")?;

    let path = lock_path();
    let timeout = Duration::from_secs(5);
    let stale_after = Duration::from_secs(120);
    let start = Instant::now();

    loop {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                return Ok(RunningProcessesLock { path });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path, stale_after) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                if start.elapsed() >= timeout {
                    return Err(err).with_context(|| {
                        format!(
                            "timed out acquiring running-process lock at {}",
                            path.display()
                        )
                    });
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to open lock {}", path.display()));
            }
        }
    }
}

fn parse_running_processes(contents: &str, path: &Path) -> Result<(RunningProcesses, bool)> {
    match serde_json::from_str::<RunningProcesses>(contents) {
        Ok(processes) => Ok((processes, false)),
        Err(primary_err) => {
            // Recovery path: accept the first valid JSON value and ignore trailing garbage.
            // This handles cases like a valid object followed by stray braces due to interrupted writes.
            let mut de = serde_json::Deserializer::from_str(contents);
            match RunningProcesses::deserialize(&mut de) {
                Ok(processes) => {
                    warn!(
                        path = %path.display(),
                        error = %primary_err,
                        "running.json contained trailing/invalid suffix; recovered first JSON value"
                    );
                    Ok((processes, true))
                }
                Err(_) => {
                    Err(primary_err).with_context(|| format!("failed to parse {}", path.display()))
                }
            }
        }
    }
}

fn save_running_processes_unlocked(processes: &RunningProcesses) -> Result<()> {
    let path = running_processes_path();
    let _ = crate::config::ensure_global_state_dir()
        .with_context(|| format!("failed to create {}", path.display()))?;

    let temp_path = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let contents = serde_json::to_string_pretty(processes)?;

    {
        let mut file = fs::File::create(&temp_path)
            .with_context(|| format!("failed to create {}", temp_path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", temp_path.display()))?;
    }

    fs::rename(&temp_path, &path).with_context(|| {
        format!(
            "failed to replace running state {} from {}",
            path.display(),
            temp_path.display()
        )
    })?;

    if let Some(parent) = path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

fn load_running_processes_unlocked() -> Result<RunningProcesses> {
    let path = running_processes_path();
    if !path.exists() {
        return Ok(RunningProcesses::default());
    }

    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let (mut processes, recovered) = parse_running_processes(&contents, &path)?;

    // Validate and clean up dead processes
    let mut changed = recovered;
    for procs in processes.projects.values_mut() {
        let before = procs.len();
        procs.retain(|p| process_alive(p.pid));
        if procs.len() != before {
            changed = true;
        }
    }
    processes.projects.retain(|_, v| !v.is_empty());

    if changed {
        save_running_processes_unlocked(&processes)?;
    }

    Ok(processes)
}

/// Load running processes, validating that PIDs are still alive
pub fn load_running_processes() -> Result<RunningProcesses> {
    let _lock = acquire_running_processes_lock()?;
    load_running_processes_unlocked()
}

/// Atomically save running processes (write to temp, then rename)
pub fn save_running_processes(processes: &RunningProcesses) -> Result<()> {
    let _lock = acquire_running_processes_lock()?;
    save_running_processes_unlocked(processes)
}

/// Register a new running process
pub fn register_process(entry: RunningProcess) -> Result<()> {
    let _lock = acquire_running_processes_lock()?;
    let mut processes = load_running_processes_unlocked()?;
    let key = entry.config_path.display().to_string();
    processes.projects.entry(key).or_default().push(entry);
    save_running_processes_unlocked(&processes)
}

/// Unregister a process by PID
pub fn unregister_process(pid: u32) -> Result<()> {
    let _lock = acquire_running_processes_lock()?;
    let mut processes = load_running_processes_unlocked()?;
    for procs in processes.projects.values_mut() {
        procs.retain(|p| p.pid != pid);
    }
    processes.projects.retain(|_, v| !v.is_empty());
    save_running_processes_unlocked(&processes)
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
        use std::process::Stdio;
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        use std::process::Stdio;
        Command::new("tasklist")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_running_processes_recovers_from_trailing_garbage() {
        let raw = "{\n  \"projects\": {}\n}\n}  }\n";
        let (parsed, recovered) =
            parse_running_processes(raw, Path::new("running.json")).expect("recover parse");
        assert!(recovered);
        assert!(parsed.projects.is_empty());
    }

    #[test]
    fn lock_owner_pid_parses_pid_line() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("running.json.lock");
        fs::write(&path, "pid=12345\n").expect("write lock");
        assert_eq!(lock_owner_pid(&path), Some(12345));
    }

    #[test]
    fn lock_owner_pid_ignores_invalid_content() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("running.json.lock");
        fs::write(&path, "oops\npid=abc\n").expect("write lock");
        assert_eq!(lock_owner_pid(&path), None);
    }
}
