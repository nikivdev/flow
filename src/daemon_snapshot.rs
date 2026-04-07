use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{activity_log, config, daemon, supervisor};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowDaemonEntry {
    pub name: String,
    pub status: String,
    pub running: bool,
    #[serde(default)]
    pub healthy: Option<bool>,
    pub pid: Option<u32>,
    pub health_target: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowDaemonStaleEntry {
    pub name: String,
    pub path: String,
    pub stdout_log_path: Option<String>,
    pub stderr_log_path: Option<String>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub last_updated_unix: Option<u64>,
    pub recent_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowDaemonSnapshot {
    pub total: usize,
    pub running: usize,
    pub healthy: usize,
    pub unhealthy: usize,
    pub stopped: usize,
    pub stale: usize,
    pub entries: Vec<FlowDaemonEntry>,
    #[serde(default)]
    pub stale_entries: Vec<FlowDaemonStaleEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowDaemonCleanupResult {
    pub archived_count: usize,
    pub archived_names: Vec<String>,
    pub archive_root: String,
    pub snapshot: FlowDaemonSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowDaemonAction {
    Start,
    Stop,
    Restart,
}

pub fn load_daemon_snapshot(config_path: Option<&Path>) -> Result<FlowDaemonSnapshot> {
    let mut entries: Vec<_> = supervisor::daemon_status_views(config_path)?
        .into_iter()
        .map(|view| {
            let status = if !view.running {
                "stopped"
            } else if view.healthy == Some(false) {
                "unhealthy"
            } else {
                "healthy"
            };
            FlowDaemonEntry {
                name: view.name,
                status: status.to_string(),
                running: view.running,
                healthy: view.healthy,
                pid: view.pid,
                health_target: view.health_target,
                description: view.description,
            }
        })
        .collect();

    entries.sort_by(|left, right| {
        right
            .running
            .cmp(&left.running)
            .then_with(|| left.status.cmp(&right.status))
            .then_with(|| left.name.cmp(&right.name))
    });

    let running = entries.iter().filter(|entry| entry.running).count();
    let unhealthy = entries
        .iter()
        .filter(|entry| entry.running && entry.healthy == Some(false))
        .count();
    let healthy = entries
        .iter()
        .filter(|entry| entry.running && entry.healthy != Some(false))
        .count();
    let stopped = entries.iter().filter(|entry| !entry.running).count();
    let stale_entries = load_stale_daemon_entries(
        &entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
    )?;

    Ok(FlowDaemonSnapshot {
        total: entries.len(),
        running,
        healthy,
        unhealthy,
        stopped,
        stale: stale_entries.len(),
        entries,
        stale_entries,
    })
}

pub fn run_daemon_action(
    name: &str,
    action: FlowDaemonAction,
    config_path: Option<&Path>,
) -> Result<FlowDaemonSnapshot> {
    let action_name = match action {
        FlowDaemonAction::Start => "start",
        FlowDaemonAction::Stop => "stop",
        FlowDaemonAction::Restart => "restart",
    };
    match action {
        FlowDaemonAction::Start => daemon::start_daemon_with_path(name, config_path)?,
        FlowDaemonAction::Stop => daemon::stop_daemon_with_path(name, config_path)?,
        FlowDaemonAction::Restart => {
            daemon::stop_daemon_with_path(name, config_path).ok();
            daemon::start_daemon_with_path(name, config_path)?;
        }
    }
    let summary = match action_name {
        "start" => "started",
        "stop" => "stopped",
        "restart" => "restarted",
        _ => action_name,
    };
    let mut activity_event =
        activity_log::ActivityEvent::done(format!("daemon.{action_name}"), summary);
    activity_event.scope = Some(name.to_string());
    activity_event.source = Some("daemon-control".to_string());
    let _ = activity_log::append_daily_event(activity_event);
    load_daemon_snapshot(config_path)
}

pub fn cleanup_stale_daemons(config_path: Option<&Path>) -> Result<FlowDaemonCleanupResult> {
    let snapshot = load_daemon_snapshot(config_path)?;
    if snapshot.stale_entries.is_empty() {
        return Ok(FlowDaemonCleanupResult {
            archived_count: 0,
            archived_names: Vec::new(),
            archive_root: daemon_archive_root()?.display().to_string(),
            snapshot,
        });
    }
    let archive_root = daemon_archive_root()?;
    fs::create_dir_all(&archive_root)?;
    let stamp = now_unix_secs();
    let mut archived_names = Vec::new();
    for entry in &snapshot.stale_entries {
        let source = PathBuf::from(&entry.path);
        if !source.exists() {
            continue;
        }
        let target = archive_root.join(format!("{stamp}-{}", entry.name));
        fs::rename(&source, &target).with_context(|| {
            format!(
                "failed to archive stale daemon dir {} -> {}",
                source.display(),
                target.display()
            )
        })?;
        archived_names.push(entry.name.clone());
    }

    let mut activity_event = activity_log::ActivityEvent::done(
        "daemon.cleanup-stale".to_string(),
        "archived".to_string(),
    );
    activity_event.source = Some("daemon-control".to_string());
    let _ = activity_log::append_daily_event(activity_event);
    Ok(FlowDaemonCleanupResult {
        archived_count: archived_names.len(),
        archived_names,
        archive_root: archive_root.display().to_string(),
        snapshot: load_daemon_snapshot(config_path)?,
    })
}

fn load_stale_daemon_entries(configured_names: &[&str]) -> Result<Vec<FlowDaemonStaleEntry>> {
    let daemons_root = daemon_logs_root()?;
    if !daemons_root.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for child in fs::read_dir(&daemons_root)? {
        let child = child?;
        let path = child.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name == "_archived" {
            continue;
        }
        if configured_names
            .iter()
            .any(|configured| *configured == name)
        {
            continue;
        }
        let stdout_log = path.join("stdout.log");
        let stderr_log = path.join("stderr.log");
        let stdout_meta = fs::metadata(&stdout_log).ok();
        let stderr_meta = fs::metadata(&stderr_log).ok();
        let last_updated_unix = stdout_meta
            .as_ref()
            .and_then(|meta| modified_unix(meta.modified().ok()))
            .into_iter()
            .chain(
                stderr_meta
                    .as_ref()
                    .and_then(|meta| modified_unix(meta.modified().ok())),
            )
            .max();
        entries.push(FlowDaemonStaleEntry {
            name: name.to_string(),
            path: path.display().to_string(),
            stdout_log_path: stdout_log
                .exists()
                .then(|| stdout_log.display().to_string()),
            stderr_log_path: stderr_log
                .exists()
                .then(|| stderr_log.display().to_string()),
            stdout_bytes: stdout_meta.map(|meta| meta.len()).unwrap_or(0),
            stderr_bytes: stderr_meta.map(|meta| meta.len()).unwrap_or(0),
            last_updated_unix,
            recent_error: read_recent_error_line(&stderr_log),
        });
    }
    entries.sort_by(|left, right| {
        right
            .last_updated_unix
            .cmp(&left.last_updated_unix)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn daemon_logs_root() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?.join("daemons"))
}

fn daemon_archive_root() -> Result<PathBuf> {
    Ok(daemon_logs_root()?.join("_archived"))
}

fn modified_unix(value: Option<SystemTime>) -> Option<u64> {
    value
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn read_recent_error_line(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    contents
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(220).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_and_counts_daemon_states() {
        let snapshot = FlowDaemonSnapshot {
            total: 3,
            running: 2,
            healthy: 1,
            unhealthy: 1,
            stopped: 1,
            stale: 0,
            entries: vec![
                FlowDaemonEntry {
                    name: "jd".to_string(),
                    status: "healthy".to_string(),
                    running: true,
                    healthy: Some(true),
                    pid: Some(10),
                    health_target: Some("unix:/tmp/jd.sock".to_string()),
                    description: None,
                },
                FlowDaemonEntry {
                    name: "api".to_string(),
                    status: "unhealthy".to_string(),
                    running: true,
                    healthy: Some(false),
                    pid: Some(11),
                    health_target: Some("http://127.0.0.1:8780/health".to_string()),
                    description: None,
                },
                FlowDaemonEntry {
                    name: "worker".to_string(),
                    status: "stopped".to_string(),
                    running: false,
                    healthy: None,
                    pid: None,
                    health_target: None,
                    description: None,
                },
            ],
            stale_entries: Vec::new(),
        };

        assert_eq!(snapshot.total, 3);
        assert_eq!(snapshot.running, 2);
        assert_eq!(snapshot.healthy, 1);
        assert_eq!(snapshot.unhealthy, 1);
        assert_eq!(snapshot.stopped, 1);
        assert_eq!(snapshot.stale, 0);
    }
}
