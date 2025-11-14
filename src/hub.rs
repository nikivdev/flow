use std::{
    fs,
    net::IpAddr,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::{HubAction, HubCommand, HubOpts},
    config,
};

pub fn run(cmd: HubCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(HubAction::Start);
    match action {
        HubAction::Start => ensure_daemon(cmd.opts),
        HubAction::Stop => stop_daemon(cmd.opts),
    }
}

fn ensure_daemon(opts: HubOpts) -> Result<()> {
    let host = opts.host;
    let port = opts.port;
    let config_path = opts.config.unwrap_or_else(config::default_config_path);

    if is_daemon_alive(host, port)? {
        println!("hub already running at http://{host}:{port}");
        return Ok(());
    }

    println!(
        "Starting hub daemon at http://{host}:{port} using config {}",
        config_path.display()
    );
    let pid = start_daemon(host, port, &config_path)?;
    persist_pid(PidRecord { pid, host, port })?;
    wait_for_daemon(host, port)?;
    println!("hub ready at http://{host}:{port}");
    Ok(())
}

fn stop_daemon(opts: HubOpts) -> Result<()> {
    let host = opts.host;
    let port = opts.port;

    if let Some(record) = load_pid()? {
        println!(
            "Stopping hub daemon (pid {}) at http://{}:{}",
            record.pid, record.host, record.port
        );
        terminate_process(record.pid)?;
        wait_for_shutdown(record.host, record.port)?;
        remove_pid_file()?;
        println!("hub stopped");
        return Ok(());
    }

    if let Some(pid) = detect_pid_by_port(port)? {
        println!(
            "Stopping hub daemon detected on port {} (pid {}).",
            port, pid
        );
        terminate_process(pid)?;
        wait_for_shutdown(host, port)?;
        println!("hub stopped");
        return Ok(());
    }

    if let Some(pid) = detect_pid_by_command(port)? {
        println!(
            "Stopping hub daemon detected via process scan (pid {}).",
            pid
        );
        terminate_process(pid)?;
        wait_for_shutdown(host, port)?;
        println!("hub stopped");
        return Ok(());
    }

    if is_daemon_alive(host, port)? {
        bail!("hub daemon is running but unable to determine pid; stop it manually");
    }

    println!("hub daemon is not running");
    Ok(())
}

fn is_daemon_alive(host: IpAddr, port: u16) -> Result<bool> {
    let url = format!("http://{host}:{port}/health");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .context("failed to build HTTP client")?;

    match client.get(url).send() {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_err) => Ok(false),
    }
}

fn start_daemon(host: IpAddr, port: u16, config_path: &PathBuf) -> Result<u32> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("--host")
        .arg(host.to_string())
        .arg("--port")
        .arg(port.to_string());

    if !config_path.as_os_str().is_empty() {
        cmd.arg("--config").arg(config_path);
    }

    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn hub daemon")?;
    Ok(child.id())
}

fn wait_for_daemon(host: IpAddr, port: u16) -> Result<()> {
    const MAX_ATTEMPTS: u8 = 10;
    const BACKOFF: Duration = Duration::from_millis(300);

    for _ in 0..MAX_ATTEMPTS {
        if is_daemon_alive(host, port)? {
            return Ok(());
        }
        thread::sleep(BACKOFF);
    }

    let total_ms = BACKOFF.as_millis() as u64 * MAX_ATTEMPTS as u64;
    let total_secs = total_ms as f64 / 1000.0;
    bail!(
        "daemon did not become ready at http://{host}:{port} within {:.1}s",
        total_secs
    );
}

fn wait_for_shutdown(host: IpAddr, port: u16) -> Result<()> {
    const MAX_ATTEMPTS: u8 = 10;
    const BACKOFF: Duration = Duration::from_millis(300);

    for _ in 0..MAX_ATTEMPTS {
        if !is_daemon_alive(host, port)? {
            return Ok(());
        }
        thread::sleep(BACKOFF);
    }

    bail!("daemon failed to terminate cleanly");
}

#[derive(Debug, Serialize, Deserialize)]
struct PidRecord {
    pid: u32,
    host: IpAddr,
    port: u16,
}

fn persist_pid(record: PidRecord) -> Result<()> {
    let path = pid_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create PID directory {}", parent.display()))?;
    }

    let payload = serde_json::to_string(&record).context("failed to serialize pid record")?;
    fs::write(&path, payload)
        .with_context(|| format!("failed to write pid file at {}", path.display()))?;
    Ok(())
}

fn load_pid() -> Result<Option<PidRecord>> {
    let path = pid_file_path();
    if !path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&path)
        .with_context(|| format!("failed to read pid file at {}", path.display()))?;
    let record: PidRecord = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse pid file at {}", path.display()))?;
    Ok(Some(record))
}

fn remove_pid_file() -> Result<()> {
    let path = pid_file_path();
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove pid file at {}", path.display()))?;
    }
    Ok(())
}

fn pid_file_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/flow/hub.pid")
    } else {
        PathBuf::from(".config/flow/hub.pid")
    }
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .arg(format!("{}", pid))
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

fn detect_pid_by_port(port: u16) -> Result<Option<u32>> {
    let lsof = which::which("lsof");
    if lsof.is_err() {
        return Ok(None);
    }

    let output = Command::new(lsof.unwrap())
        .args(["-i", &format!("TCP:{}", port), "-sTCP:LISTEN", "-n", "-P"])
        .output()
        .context("failed to invoke lsof")?;

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 2 {
            continue;
        }
        if let Ok(pid) = cols[1].parse::<u32>() {
            return Ok(Some(pid));
        }
    }

    Ok(None)
}

fn detect_pid_by_command(port: u16) -> Result<Option<u32>> {
    #[cfg(unix)]
    {
        let output = Command::new("ps")
            .args(["-Ao", "pid,args"])
            .output()
            .context("failed to invoke ps")?;
        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let needle = format!("--port {port}");
        for line in stdout.lines().skip(1) {
            if !line.contains(" daemon") || !line.contains(&needle) {
                continue;
            }
            let mut parts = line.trim_start().split_whitespace();
            if let Some(pid_str) = parts.next() {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    return Ok(Some(pid));
                }
            }
        }
        Ok(None)
    }
    #[cfg(not(unix))]
    {
        let _ = port;
        Ok(None)
    }
}
