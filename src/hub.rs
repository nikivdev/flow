use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::{HubAction, HubCommand, HubOpts},
    config, doctor,
};

/// Flow acts as a thin launcher that makes sure the lin hub daemon is running.
pub fn run(cmd: HubCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(HubAction::Start);
    let opts = cmd.opts;
    let runtime = ensure_hub_runtime()?;

    match action {
        HubAction::Start => ensure_daemon(opts, &runtime),
        HubAction::Stop => stop_daemon(&runtime),
    }
}

fn ensure_daemon(opts: HubOpts, runtime: &HubRuntime) -> Result<()> {
    let config_path = opts.config.unwrap_or_else(config::default_config_path);
    let cfg = config::load_or_default(&config_path);

    // If there is nothing to watch, bail early.
    if cfg.watchers.is_empty() {
        println!(
            "No watchers defined in {}; lin hub will not be started.",
            config_path.display()
        );
        return Ok(());
    }

    ensure_lin_running(runtime, &config_path, cfg.watchers.len())?;

    if !opts.no_ui {
        println!(
            "Lin watcher daemon ensured with {} watcher(s) (config: {}).",
            cfg.watchers.len(),
            config_path.display()
        );
    }

    Ok(())
}

fn stop_daemon(runtime: &HubRuntime) -> Result<()> {
    stop_lin_process().ok();
    println!("Lin hub stopped (if it was running) [{}]", runtime.binary.display());
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HubRuntime {
    binary: PathBuf,
    version: Option<String>,
}

fn ensure_hub_runtime() -> Result<HubRuntime> {
    if let Some(existing) = load_hub_runtime()? {
        return Ok(existing);
    }

    let runtime = prompt_hub_runtime()?;
    persist_hub_runtime(&runtime)?;
    Ok(runtime)
}

fn prompt_hub_runtime() -> Result<HubRuntime> {
    let binary = doctor::ensure_lin_available_interactive()?;
    Ok(HubRuntime {
        version: detect_binary_version(&binary),
        binary,
    })
}

fn detect_binary_version(path: &Path) -> Option<String> {
    Command::new(path)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
}

fn hub_runtime_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/flow/hub-runtime.json")
    } else {
        PathBuf::from(".config/flow/hub-runtime.json")
    }
}

fn load_hub_runtime() -> Result<Option<HubRuntime>> {
    let path = hub_runtime_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let runtime: HubRuntime = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse hub runtime at {}", path.display()))?;
    Ok(Some(runtime))
}

fn persist_hub_runtime(runtime: &HubRuntime) -> Result<()> {
    let path = hub_runtime_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let payload = serde_json::to_string_pretty(runtime)
        .context("failed to serialize hub runtime selection")?;
    fs::write(&path, payload).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn ensure_lin_running(runtime: &HubRuntime, config_path: &Path, watcher_count: usize) -> Result<()> {
    if watcher_count == 0 {
        return Ok(());
    }

    if let Some(pid) = load_lin_pid()? {
        if process_alive(pid)? {
            return Ok(());
        }
        remove_lin_pid().ok();
    }

    println!(
        "Starting lin watcher daemon ({} watcher{}) using {}",
        watcher_count,
        if watcher_count == 1 { "" } else { "s" },
        config_path.display()
    );
    start_lin_process(&runtime.binary, config_path)
}

fn start_lin_process(binary: &Path, config_path: &Path) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("--config").arg(config_path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to start lin from {}", binary.display()))?;
    persist_lin_pid(child.id())?;
    Ok(())
}

fn stop_lin_process() -> Result<()> {
    if let Some(pid) = load_lin_pid()? {
        terminate_process(pid).ok();
        remove_lin_pid().ok();
    }
    Ok(())
}

fn lin_pid_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/flow/lin.pid")
    } else {
        PathBuf::from(".config/flow/lin.pid")
    }
}

fn load_lin_pid() -> Result<Option<u32>> {
    let path = lin_pid_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let pid: u32 = contents.trim().parse().ok().unwrap_or(0);
    if pid == 0 {
        Ok(None)
    } else {
        Ok(Some(pid))
    }
}

fn persist_lin_pid(pid: u32) -> Result<()> {
    let path = lin_pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create pid dir {}", parent.display()))?;
    }
    fs::write(&path, pid.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn remove_lin_pid() -> Result<()> {
    let path = lin_pid_path();
    if path.exists() {
        fs::remove_file(path).ok();
    }
    Ok(())
}

fn process_alive(pid: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        let status = Command::new("kill").arg("-0").arg(pid.to_string()).status();
        return Ok(status.map(|s| s.success()).unwrap_or(false));
    }

    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .output()
            .context("failed to invoke tasklist")?;
        if !output.status.success() {
            return Ok(false);
        }
        let needle = pid.to_string();
        let body = String::from_utf8_lossy(&output.stdout);
        Ok(body.lines().any(|line| line.contains(&needle)))
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg(format!("{pid}"))
            .status()
            .context("failed to invoke kill command")?;
        if status.success() {
            return Ok(());
        }
        bail!(
            "kill command exited with status {}",
            status.code().unwrap_or(-1)
        );
    }

    #[cfg(windows)]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .status()
            .context("failed to invoke taskkill")?;
        if status.success() {
            return Ok(());
        }
        bail!(
            "taskkill exited with status {}",
            status.code().unwrap_or(-1)
        );
    }
}
