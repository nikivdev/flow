use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{daemon, supervisor};

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
pub struct FlowDaemonSnapshot {
    pub total: usize,
    pub running: usize,
    pub healthy: usize,
    pub unhealthy: usize,
    pub stopped: usize,
    pub entries: Vec<FlowDaemonEntry>,
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

    Ok(FlowDaemonSnapshot {
        total: entries.len(),
        running,
        healthy,
        unhealthy,
        stopped,
        entries,
    })
}

pub fn run_daemon_action(
    name: &str,
    action: FlowDaemonAction,
    config_path: Option<&Path>,
) -> Result<FlowDaemonSnapshot> {
    match action {
        FlowDaemonAction::Start => daemon::start_daemon_with_path(name, config_path)?,
        FlowDaemonAction::Stop => daemon::stop_daemon_with_path(name, config_path)?,
        FlowDaemonAction::Restart => {
            daemon::stop_daemon_with_path(name, config_path).ok();
            daemon::start_daemon_with_path(name, config_path)?;
        }
    }
    load_daemon_snapshot(config_path)
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
            entries: vec![
                FlowDaemonEntry {
                    name: "codexd".to_string(),
                    status: "healthy".to_string(),
                    running: true,
                    healthy: Some(true),
                    pid: Some(10),
                    health_target: Some("unix:/tmp/codexd.sock".to_string()),
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
        };

        assert_eq!(snapshot.total, 3);
        assert_eq!(snapshot.running, 2);
        assert_eq!(snapshot.healthy, 1);
        assert_eq!(snapshot.unhealthy, 1);
        assert_eq!(snapshot.stopped, 1);
    }
}
