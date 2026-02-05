use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::DeployCommand;
use crate::deploy;

pub fn run() -> Result<()> {
    let flow_root = flow_repo_root()?;
    update_flow_repo(&flow_root)?;
    rebuild_flow(&flow_root)?;
    reload_fish_shell()?;
    Ok(())
}

fn flow_repo_root() -> Result<PathBuf> {
    let root = dirs::home_dir()
        .context("failed to resolve home directory")?
        .join("code/flow");
    if !root.exists() {
        bail!("flow repo not found at {}", root.display());
    }
    Ok(root)
}

fn update_flow_repo(root: &PathBuf) -> Result<()> {
    println!("Updating {}", root.display());
    let status = Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap_or(""),
            "pull",
            "--rebase",
            "--autostash",
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run git pull")?;
    if !status.success() {
        bail!("git pull failed");
    }
    Ok(())
}

fn rebuild_flow(root: &PathBuf) -> Result<()> {
    let prev = std::env::current_dir().context("failed to read current directory")?;
    std::env::set_current_dir(root)
        .with_context(|| format!("failed to switch to {}", root.display()))?;
    let result = deploy::run(DeployCommand { action: None });
    std::env::set_current_dir(prev).context("failed to restore previous directory")?;
    result
}

fn reload_fish_shell() -> Result<()> {
    if std::env::var("FISH_VERSION").is_err() {
        return Ok(());
    }
    if !atty::is(atty::Stream::Stdout) {
        return Ok(());
    }

    println!("Reloading fish shell...");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new("fish").arg("-l").exec();
        bail!("failed to exec fish: {}", err);
    }
    #[cfg(not(unix))]
    {
        let _ = Command::new("fish").arg("-l").status();
        Ok(())
    }
}
