use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

pub fn resolve_bin() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("FLOW_BASE_BIN") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    // Prefer a more specific name, but fall back to the current base repo binary name.
    for name in ["base", "db"] {
        if let Ok(path) = which::which(name) {
            return Some(path);
        }
    }

    None
}

pub fn run_inherit_stdio(bin: &Path, args: &[String]) -> Result<()> {
    let status = Command::new(bin)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run {} {}", bin.display(), args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("{} exited with {}", bin.display(), status);
    }
    Ok(())
}

pub fn run_with_stdin(bin: &Path, args: &[String], stdin: &str) -> Result<()> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // This path is currently only used for best-effort "task run" ingestion.
        // If the user has some other `base` binary on PATH (or an older one),
        // it may print usage/errors like "unrecognized subcommand 'ingest'".
        // We intentionally silence stderr to avoid confusing noise during normal runs.
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {} {}", bin.display(), args.join(" ")))?;

    {
        use std::io::Write;
        let child_stdin = child.stdin.as_mut().context("failed to open stdin")?;
        child_stdin.write_all(stdin.as_bytes())?;
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("{} exited with {}", bin.display(), status);
    }
    Ok(())
}
