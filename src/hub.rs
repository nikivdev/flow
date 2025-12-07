use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;

use crate::{
    cli::{HubAction, HubCommand, HubOpts},
    config, doctor,
    lin_runtime::{self, LinRuntime},
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

fn ensure_daemon(opts: HubOpts, runtime: &LinRuntime) -> Result<()> {
    let host = opts.host;
    let port = opts.port;
    let lin_config = opts.config.as_ref().map(|path| {
        if path.is_absolute() {
            path.clone()
        } else {
            config::expand_path(&path.to_string_lossy())
        }
    });

    if hub_healthy(host, port) {
        if !opts.no_ui {
            println!(
                "Lin watcher daemon already running at {}",
                format_addr(host, port)
            );
        }
        return Ok(());
    }

    if let Some(pid) = load_lin_pid()? {
        if process_alive(pid)? {
            terminate_process(pid).ok();
        }
        remove_lin_pid().ok();
    }

    println!(
        "Starting lin watcher daemon using {}{}",
        runtime.binary.display(),
        lin_config
            .as_ref()
            .map(|p| format!(" (config: {})", p.display()))
            .unwrap_or_default()
    );
    start_lin_process(&runtime.binary, host, port, lin_config.as_deref())?;

    if !opts.no_ui {
        println!("Lin watcher daemon ensured at {}", format_addr(host, port));
    }
    Ok(())
}

fn stop_daemon(runtime: &LinRuntime) -> Result<()> {
    stop_lin_process().ok();
    println!(
        "Lin hub stopped (if it was running) [{}]",
        runtime.binary.display()
    );
    Ok(())
}
fn ensure_hub_runtime() -> Result<LinRuntime> {
    if let Some(existing) = lin_runtime::load_runtime()? {
        return Ok(existing);
    }

    let binary = doctor::ensure_lin_available_interactive()?;
    let runtime = LinRuntime {
        version: lin_runtime::detect_binary_version(&binary),
        binary,
    };
    lin_runtime::persist_runtime(&runtime)?;
    Ok(runtime)
}

fn start_lin_process(
    binary: &Path,
    host: IpAddr,
    port: u16,
    config_path: Option<&Path>,
) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("daemon")
        .arg("--host")
        .arg(host.to_string())
        .arg("--port")
        .arg(port.to_string());

    if let Some(path) = config_path {
        cmd.arg("--config").arg(path);
    }

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
    if pid == 0 { Ok(None) } else { Ok(Some(pid)) }
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

/// Check if the hub is healthy and responding.
pub fn hub_healthy(host: IpAddr, port: u16) -> bool {
    let url = format_health_url(host, port);
    let client = Client::builder()
        .timeout(Duration::from_millis(750))
        .build();

    let Ok(client) = client else {
        return false;
    };

    client
        .get(url)
        .send()
        .and_then(|resp| resp.error_for_status())
        .map(|_| true)
        .unwrap_or(false)
}

fn format_addr(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}"),
    }
}

fn format_health_url(host: IpAddr, port: u16) -> String {
    match host {
        IpAddr::V4(_) => format!("http://{host}:{port}/health"),
        IpAddr::V6(_) => format!("http://[{host}]:{port}/health"),
    }
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
            .stderr(std::process::Stdio::null())
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
