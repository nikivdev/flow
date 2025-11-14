use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

use crate::cli::IndexOpts;

pub fn run(opts: IndexOpts) -> Result<()> {
    let codanna_path = which::which(&opts.binary).with_context(|| {
        format!(
            "failed to locate '{}' on PATH – install Codanna or pass --binary",
            opts.binary
        )
    })?;

    let project_root = resolve_project_root(opts.project_root)?;

    ensure_codanna_initialized(&codanna_path, &project_root)?;
    run_codanna_index(&codanna_path, &project_root)?;
    let payload = capture_index_stats(&codanna_path, &project_root)?;
    let db_path = persist_snapshot(&project_root, &codanna_path, &payload, opts.database)?;

    println!("Codanna index snapshot stored at {}", db_path.display());

    Ok(())
}

fn resolve_project_root(path: Option<PathBuf>) -> Result<PathBuf> {
    let raw_path = match path {
        Some(p) if p.is_absolute() => p,
        Some(p) => env::current_dir()?.join(p),
        None => env::current_dir()?,
    };

    raw_path
        .canonicalize()
        .with_context(|| format!("failed to resolve project root at {}", raw_path.display()))
}

fn ensure_codanna_initialized(binary: &Path, project_root: &Path) -> Result<()> {
    let settings = project_root.join(".codanna/settings.toml");
    if settings.exists() {
        return Ok(());
    }

    println!(
        "No Codanna settings found at {} – running 'codanna init'.",
        settings.display()
    );
    let status = Command::new(binary)
        .arg("init")
        .current_dir(project_root)
        .status()
        .with_context(|| "failed to spawn 'codanna init'")?;

    if status.success() {
        Ok(())
    } else {
        bail!(
            "'codanna init' exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
}

fn run_codanna_index(binary: &Path, project_root: &Path) -> Result<()> {
    println!("Indexing project {} via Codanna...", project_root.display());
    let status = Command::new(binary)
        .arg("index")
        .arg("--progress")
        .arg(".")
        .current_dir(project_root)
        .status()
        .with_context(|| "failed to spawn 'codanna index'")?;

    if status.success() {
        Ok(())
    } else {
        bail!(
            "'codanna index' exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
}

fn capture_index_stats(binary: &Path, project_root: &Path) -> Result<String> {
    println!("Fetching Codanna index metadata...");
    let output = Command::new(binary)
        .arg("mcp")
        .arg("get_index_info")
        .arg("--json")
        .current_dir(project_root)
        .output()
        .with_context(|| "failed to run 'codanna mcp get_index_info --json'")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("'codanna mcp get_index_info' failed: {}", stderr.trim());
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .with_context(|| "failed to parse JSON from 'codanna mcp get_index_info --json'")?;

    serde_json::to_string_pretty(&json).with_context(|| "failed to serialize Codanna stats payload")
}

fn persist_snapshot(
    project_root: &Path,
    binary: &Path,
    payload: &str,
    override_path: Option<PathBuf>,
) -> Result<PathBuf> {
    let db_path = override_path.unwrap_or_else(default_db_path);
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let conn = Connection::open(&db_path)
        .with_context(|| format!("failed to open sqlite database at {}", db_path.display()))?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS index_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_path TEXT NOT NULL,
            codanna_binary TEXT NOT NULL,
            indexed_at INTEGER NOT NULL,
            payload TEXT NOT NULL
        )",
        [],
    )
    .with_context(|| "failed to create index_runs table")?;

    let repo_str = project_root.display().to_string();
    let binary_str = binary.display().to_string();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .with_context(|| "system clock before UNIX_EPOCH")?
        .as_secs() as i64;

    conn.execute(
        "INSERT INTO index_runs (repo_path, codanna_binary, indexed_at, payload)
         VALUES (?1, ?2, ?3, ?4)",
        (&repo_str, &binary_str, timestamp, payload),
    )
    .with_context(|| "failed to insert index snapshot")?;

    Ok(db_path)
}

fn default_db_path() -> PathBuf {
    if let Some(home) = env::var_os("HOME") {
        PathBuf::from(home).join(".db/flow/flow.sqlite")
    } else {
        PathBuf::from(".db/flow/flow.sqlite")
    }
}
