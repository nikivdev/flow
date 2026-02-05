use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn ensure_jj_installed() -> Result<()> {
    let status = Command::new("jj")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to run jj --version")?;
    if !status.success() {
        bail!("jj is required but not available on PATH");
    }
    Ok(())
}

pub fn ensure_jj_repo() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    ensure_jj_repo_in(&cwd)
}

pub fn ensure_jj_repo_in(path: &Path) -> Result<PathBuf> {
    ensure_jj_installed()?;
    if let Ok(root) = try_jj_root(path) {
        return Ok(root);
    }

    let git_dir = path.join(".git");
    if git_dir.exists() {
        let status = Command::new("jj")
            .current_dir(path)
            .args(["git", "init", "--colocate"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("failed to run jj git init --colocate")?;
        if status.success() {
            if let Ok(root) = try_jj_root(path) {
                return Ok(root);
            }
        }
    }

    bail!(
        "This repo is not a jj workspace. Run `jj git init --colocate` in {} and retry.",
        path.display()
    );
}

pub fn jj_root_if_exists(path: &Path) -> Option<PathBuf> {
    let output = Command::new("jj")
        .current_dir(path)
        .arg("root")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn try_jj_root(path: &Path) -> Result<PathBuf> {
    let output = Command::new("jj")
        .current_dir(path)
        .arg("root")
        .output()
        .context("failed to run jj root")?;
    if !output.status.success() {
        bail!("jj root failed");
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}
