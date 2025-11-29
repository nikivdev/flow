use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Serialize)]
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
