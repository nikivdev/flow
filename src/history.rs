use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Serialize, Deserialize)]
pub struct InvocationRecord {
    pub timestamp_ms: u128,
    pub duration_ms: u128,
    pub project_root: String,
    #[serde(default)]
    pub project_name: Option<String>,
    pub config_path: String,
    pub task_name: String,
    pub command: String,
    #[serde(default)]
    pub user_input: String,
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
        project_name: Option<&str>,
        task_name: impl Into<String>,
        command: impl Into<String>,
        user_input: impl Into<String>,
        used_flox: bool,
    ) -> Self {
        Self {
            timestamp_ms: now_ms(),
            duration_ms: 0,
            project_root: project_root.into(),
            project_name: project_name.map(|s| s.to_string()),
            config_path: config_path.into(),
            task_name: task_name.into(),
            command: command.into(),
            user_input: user_input.into(),
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
    let _ = config::ensure_global_config_dir()
        .with_context(|| format!("failed to create history dir {}", path.display()))?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open history file {}", path.display()))?;

    let line = serde_json::to_string(&invocation).context("failed to serialize invocation")?;
    writeln!(file, "{line}").context("failed to write invocation to history")?;
    Ok(())
}

/// Print the most recent invocation with only the user input and the resulting output or error.
pub fn print_last_record() -> Result<()> {
    let path = history_path();
    let record = load_last_record(&path)?;
    let Some(rec) = record else {
        if path.exists() {
            println!("No valid history entries found in {}", path.display());
        } else {
            println!("No history found at {}", path.display());
        }
        return Ok(());
    };

    let user_input = if rec.user_input.trim().is_empty() {
        rec.task_name.clone()
    } else {
        rec.user_input.clone()
    };
    println!("{user_input}");

    if rec.output.trim().is_empty() {
        if !rec.success {
            let status = rec
                .status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("error (status: {status})");
        }
    } else {
        print!("{}", rec.output);
        if !rec.output.ends_with('\n') {
            println!();
        }
    }

    Ok(())
}

/// Print the most recent invocation with output and status.
pub fn print_last_record_full() -> Result<()> {
    let path = history_path();
    let record = load_last_record(&path)?;
    let Some(rec) = record else {
        if path.exists() {
            println!("No valid history entries found in {}", path.display());
        } else {
            println!("No history found at {}", path.display());
        }
        return Ok(());
    };

    println!("task: {}", rec.task_name);
    println!("command: {}", rec.command);
    println!("project: {}", rec.project_root);
    if let Some(name) = rec.project_name.as_deref() {
        println!("project_name: {name}");
    }
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

fn load_last_record(path: &Path) -> Result<Option<InvocationRecord>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read history at {}", path.display()))?;
    for line in contents.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<InvocationRecord>(line) {
            return Ok(Some(rec));
        }
    }

    Ok(None)
}

/// Load the last invocation record for a specific project root.
pub fn load_last_record_for_project(project_root: &Path) -> Result<Option<InvocationRecord>> {
    let path = history_path();
    if !path.exists() {
        return Ok(None);
    }

    let canonical_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let canonical_str = canonical_root.to_string_lossy();

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read history at {}", path.display()))?;

    for line in contents.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<InvocationRecord>(line) {
            if rec.project_root == canonical_str {
                return Ok(Some(rec));
            }
        }
    }

    Ok(None)
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
