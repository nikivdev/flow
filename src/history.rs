use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct InvocationRecord {
    pub timestamp_ms: u128,
    pub duration_ms: u128,
    pub project_root: String,
    pub config_path: String,
    pub task_name: String,
    pub command: String,
    pub status: Option<i32>,
    pub success: bool,
    pub used_flox: bool,
    pub output: String,
    pub flow_version: String,
}

impl InvocationRecord {
    pub fn new(
        project_root: impl Into<String>,
        config_path: impl Into<String>,
        task_name: impl Into<String>,
        command: impl Into<String>,
        used_flox: bool,
    ) -> Self {
        Self {
            timestamp_ms: now_ms(),
            duration_ms: 0,
            project_root: project_root.into(),
            config_path: config_path.into(),
            task_name: task_name.into(),
            command: command.into(),
            status: None,
            success: false,
            used_flox,
            output: String::new(),
            flow_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

pub fn record(invocation: InvocationRecord) -> Result<()> {
    let path = history_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create history dir {}", parent.display()))?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open history file {}", path.display()))?;

    let line = serde_json::to_string(&invocation).context("failed to serialize invocation")?;
    writeln!(file, "{line}").context("failed to write invocation to history")?;
    Ok(())
}

/// Print the most recent invocation with output and status.
pub fn print_last_record() -> Result<()> {
    let path = history_path();
    if !path.exists() {
        println!("No history found at {}", path.display());
        return Ok(());
    }

    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read history at {}", path.display()))?;
    let mut last: Option<InvocationRecord> = None;
    for line in contents.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<InvocationRecord>(line) {
            Ok(rec) => {
                last = Some(rec);
                break;
            }
            Err(_) => continue,
        }
    }

    let Some(rec) = last else {
        println!("No valid history entries found in {}", path.display());
        return Ok(());
    };

    println!("task: {}", rec.task_name);
    println!("command: {}", rec.command);
    println!("project: {}", rec.project_root);
    println!("config: {}", rec.config_path);
    println!(
        "status: {} (code: {})",
        if rec.success { "success" } else { "failure" },
        rec.status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    );
    println!("duration_ms: {}", rec.duration_ms);
    println!("flow_version: {}", rec.flow_version);
    println!("--- output ---");
    print!("{}", rec.output);
    Ok(())
}

fn history_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("flow")
        .join("history.jsonl")
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
