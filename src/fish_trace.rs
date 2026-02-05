use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};

/// Fish shell trace record from io-trace metadata
#[derive(Debug, Clone)]
pub struct FishTraceRecord {
    pub version: u32,
    pub timestamp_secs: u64,
    pub job_id: u64,
    pub cwd: String,
    pub cmd: String,
    pub status: i32,
    pub pipestatus: Vec<i32>,
    pub stdout_len: usize,
    pub stderr_len: usize,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl FishTraceRecord {
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_secs * 1000
    }

    pub fn formatted_time(&self) -> String {
        let system_time = UNIX_EPOCH + Duration::from_secs(self.timestamp_secs);
        let dt: chrono::DateTime<chrono::Local> = system_time.into();
        dt.format("%H:%M:%S").to_string()
    }

    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// Get the fish io-trace directory
pub fn io_trace_dir() -> PathBuf {
    if let Ok(path) = env::var("FISH_IO_TRACE_DIR") {
        return PathBuf::from(path);
    }
    let data_home = env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        });
    data_home.join("fish").join("io-trace")
}

/// Load the last fish trace record
pub fn load_last_record() -> Result<Option<FishTraceRecord>> {
    let dir = io_trace_dir();
    let meta_path = dir.join("last.meta");
    if !meta_path.exists() {
        return Ok(None);
    }
    parse_meta_file(&meta_path)
}

/// Load stdout from last fish trace
pub fn load_last_stdout() -> Result<Option<String>> {
    let dir = io_trace_dir();
    let path = dir.join("last.stdout");
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(content))
}

/// Load stderr from last fish trace
pub fn load_last_stderr() -> Result<Option<String>> {
    let dir = io_trace_dir();
    let path = dir.join("last.stderr");
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(Some(content))
}

fn parse_meta_file(path: &Path) -> Result<Option<FishTraceRecord>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;

    let mut record = FishTraceRecord {
        version: 1,
        timestamp_secs: 0,
        job_id: 0,
        cwd: String::new(),
        cmd: String::new(),
        status: 0,
        pipestatus: vec![],
        stdout_len: 0,
        stderr_len: 0,
        stdout_truncated: false,
        stderr_truncated: false,
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "version" => record.version = value.parse().unwrap_or(1),
            "timestamp_secs" => record.timestamp_secs = value.parse().unwrap_or(0),
            "job_id" => record.job_id = value.parse().unwrap_or(0),
            "cwd" => record.cwd = value.to_string(),
            "cmd" => {
                // Remove surrounding quotes if present
                let v = value.trim();
                if v.starts_with('"') && v.ends_with('"') && v.len() > 1 {
                    record.cmd = v[1..v.len() - 1].to_string();
                } else {
                    record.cmd = v.to_string();
                }
            }
            "status" => record.status = value.parse().unwrap_or(0),
            "pipestatus" => {
                record.pipestatus = value
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
            }
            "stdout_len" => record.stdout_len = value.parse().unwrap_or(0),
            "stderr_len" => record.stderr_len = value.parse().unwrap_or(0),
            "stdout_truncated" => record.stdout_truncated = value != "0",
            "stderr_truncated" => record.stderr_truncated = value != "0",
            _ => {}
        }
    }

    if record.timestamp_secs == 0 {
        return Ok(None);
    }

    Ok(Some(record))
}

/// Print the last fish shell command and its output (like `trail last`)
pub fn print_last_fish_cmd() -> Result<()> {
    let Some(record) = load_last_record()? else {
        println!("No fish trace found at {}", io_trace_dir().display());
        return Ok(());
    };

    println!("{}", record.cmd);

    let stdout = load_last_stdout()?.unwrap_or_default();
    let stderr = load_last_stderr()?.unwrap_or_default();

    if !stdout.is_empty() {
        print!("{stdout}");
        if !stdout.ends_with('\n') {
            println!();
        }
    }

    if !stderr.is_empty() {
        eprint!("{stderr}");
        if !stderr.ends_with('\n') {
            eprintln!();
        }
    }

    if stdout.is_empty() && stderr.is_empty() && !record.success() {
        println!("(exit status: {})", record.status);
    }

    Ok(())
}

/// Print full details of the last fish shell command
pub fn print_last_fish_cmd_full() -> Result<()> {
    let Some(record) = load_last_record()? else {
        println!("No fish trace found at {}", io_trace_dir().display());
        return Ok(());
    };

    println!("cmd: {}", record.cmd);
    println!("cwd: {}", record.cwd);
    println!("job_id: {}", record.job_id);
    println!("timestamp: {}", record.formatted_time());
    println!(
        "status: {} (code: {})",
        if record.success() {
            "success"
        } else {
            "failure"
        },
        record.status
    );
    if !record.pipestatus.is_empty() {
        let ps: Vec<String> = record.pipestatus.iter().map(|s| s.to_string()).collect();
        println!("pipestatus: {}", ps.join(","));
    }
    println!(
        "stdout: {} bytes{}",
        record.stdout_len,
        if record.stdout_truncated {
            " (truncated)"
        } else {
            ""
        }
    );
    println!(
        "stderr: {} bytes{}",
        record.stderr_len,
        if record.stderr_truncated {
            " (truncated)"
        } else {
            ""
        }
    );

    let stdout = load_last_stdout()?.unwrap_or_default();
    let stderr = load_last_stderr()?.unwrap_or_default();

    if !stdout.is_empty() || !stderr.is_empty() {
        println!("--- output ---");
    }
    if !stdout.is_empty() {
        print!("{stdout}");
        if !stdout.ends_with('\n') {
            println!();
        }
    }
    if !stderr.is_empty() {
        eprintln!("--- stderr ---");
        eprint!("{stderr}");
    }

    Ok(())
}

/// Check if traced fish shell is installed
pub fn is_traced_fish_installed() -> bool {
    let bin_path = traced_fish_bin_path();
    bin_path.exists()
}

/// Get the path to the traced fish binary
pub fn traced_fish_bin_path() -> PathBuf {
    if let Ok(path) = env::var("FISH_TRACED_BIN") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("bin")
        .join("fish")
}

/// Get the path to fish-shell source repo (for building)
pub fn fish_source_path() -> Option<PathBuf> {
    // Check env var first
    if let Ok(path) = env::var("FISH_SOURCE_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    // Check common locations
    let home = dirs::home_dir()?;
    let candidates = [
        home.join("repos/fish-shell/fish-shell"),
        home.join("code/fish-shell"),
        home.join(".local/src/fish-shell"),
    ];

    for candidate in candidates {
        if candidate.join("Cargo.toml").exists() {
            return Some(candidate);
        }
    }

    None
}
