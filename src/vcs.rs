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
    let output = Command::new("jj")
        .current_dir(path)
        .arg("root")
        .output()
        .context("failed to run jj root")?;
    if !output.status.success() {
        bail!(
            "This repo is not a jj workspace. Run `jj git init --colocate` in {} and retry.",
            path.display()
        );
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
}
