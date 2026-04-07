use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{activity_log, ai, config, daemon_snapshot, running};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowOpsProcessSnapshot {
    pub name: String,
    pub source: String,
    pub status: String,
    pub pid: Option<u32>,
    pub project: Option<String>,
    pub command: String,
    pub started_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowOpsLogSnapshot {
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub last_updated_unix: Option<u64>,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowOpsOverview {
    pub generated_at_unix: u64,
    pub target_path: String,
    pub eval: ai::CodexEvalSnapshot,
    pub daemons: daemon_snapshot::FlowDaemonSnapshot,
    pub processes: Vec<FlowOpsProcessSnapshot>,
    pub recent_activity: Vec<activity_log::ActivityEvent>,
    pub logs: Vec<FlowOpsLogSnapshot>,
}

pub fn load(
    target_path: &Path,
    activity_limit: usize,
    log_lines: usize,
) -> Result<FlowOpsOverview> {
    let daemons = daemon_snapshot::load_daemon_snapshot(None)?;
    Ok(FlowOpsOverview {
        generated_at_unix: now_unix_secs(),
        target_path: target_path.display().to_string(),
        eval: ai::codex_eval_snapshot(target_path, 200)?,
        daemons: daemons.clone(),
        processes: collect_processes(&daemons)?,
        recent_activity: activity_log::recent_events(activity_limit.max(1))?,
        logs: collect_logs(log_lines.max(1))?,
    })
}

fn collect_processes(
    daemons: &daemon_snapshot::FlowDaemonSnapshot,
) -> Result<Vec<FlowOpsProcessSnapshot>> {
    let mut items = Vec::new();
    let state_dir = config::ensure_global_state_dir()?;

    items.push(process_from_pid_file(
        "flow-server",
        "server",
        &state_dir.join("server.pid"),
        "f server --host 127.0.0.1 --port 9050",
    ));
    items.push(process_from_pid_file(
        "supervisor",
        "supervisor",
        &state_dir.join("supervisor.pid"),
        "f supervisor run",
    ));

    for entry in &daemons.entries {
        items.push(FlowOpsProcessSnapshot {
            name: entry.name.clone(),
            source: "daemon".to_string(),
            status: entry.status.clone(),
            pid: entry.pid,
            project: None,
            command: entry.description.clone().unwrap_or_default(),
            started_at_unix: None,
        });
    }

    if let Ok(processes) = running::load_running_processes() {
        for procs in processes.projects.into_values() {
            for process in procs {
                items.push(FlowOpsProcessSnapshot {
                    name: process.task_name,
                    source: "task".to_string(),
                    status: "running".to_string(),
                    pid: Some(process.pid),
                    project: process.project_name,
                    command: process.command,
                    started_at_unix: Some((process.started_at / 1000) as u64),
                });
            }
        }
    }

    items.sort_by(|left, right| {
        rank_status(&right.status)
            .cmp(&rank_status(&left.status))
            .then_with(|| right.started_at_unix.cmp(&left.started_at_unix))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(items)
}

fn process_from_pid_file(
    name: &str,
    source: &str,
    pid_path: &Path,
    command: &str,
) -> FlowOpsProcessSnapshot {
    let pid = load_pid(pid_path).filter(|pid| running::process_alive(*pid));
    FlowOpsProcessSnapshot {
        name: name.to_string(),
        source: source.to_string(),
        status: if pid.is_some() {
            "running".to_string()
        } else {
            "stopped".to_string()
        },
        pid,
        project: None,
        command: command.to_string(),
        started_at_unix: None,
    }
}

fn load_pid(path: &Path) -> Option<u32> {
    let contents = fs::read_to_string(path).ok()?;
    let pid = contents.trim().parse::<u32>().ok()?;
    if pid == 0 { None } else { Some(pid) }
}

fn collect_logs(lines: usize) -> Result<Vec<FlowOpsLogSnapshot>> {
    let base = config::ensure_global_state_dir()?;
    let entries = [
        ("server.stdout", base.join("server.stdout.log")),
        ("server.stderr", base.join("server.stderr.log")),
        ("supervisor", base.join("supervisor.log")),
        (
            "jd.stdout",
            base.join("daemons").join("jd").join("stdout.log"),
        ),
        (
            "jd.stderr",
            base.join("daemons").join("jd").join("stderr.log"),
        ),
    ];

    entries
        .into_iter()
        .map(|(name, path)| read_log_snapshot(name, &path, lines))
        .collect()
}

fn read_log_snapshot(name: &str, path: &Path, lines: usize) -> Result<FlowOpsLogSnapshot> {
    let bytes = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let last_updated_unix = fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(system_time_unix);
    Ok(FlowOpsLogSnapshot {
        name: name.to_string(),
        path: path.display().to_string(),
        bytes,
        last_updated_unix,
        lines: tail_lines(path, lines)?,
    })
}

fn tail_lines(path: &Path, lines: usize) -> Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = read_tail_bytes(path, 32 * 1024)?;
    let mut out = String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    if out.len() > lines {
        out.drain(0..out.len() - lines);
    }
    Ok(out)
}

fn read_tail_bytes(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek {}", path.display()))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .with_context(|| format!("failed to read {}", path.display()))?;
    Ok(buf)
}

fn system_time_unix(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|value| value.as_secs())
}

fn now_unix_secs() -> u64 {
    system_time_unix(SystemTime::now()).unwrap_or(0)
}

fn rank_status(value: &str) -> u8 {
    match value {
        "running" | "healthy" => 3,
        "unhealthy" => 2,
        _ => 1,
    }
}
