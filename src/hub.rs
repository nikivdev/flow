use std::{
    fs,
    io::{self, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    cli::{HubAction, HubCommand, HubOpts, ServersOpts},
    config, doctor, servers_tui,
};

pub fn run(cmd: HubCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(HubAction::Start);
    let opts = cmd.opts;
    let runtime = ensure_hub_runtime()?; // prompt if missing
    match action {
        HubAction::Start => {
            ensure_daemon(opts.clone(), &runtime)?;
            if opts.no_ui {
                return Ok(());
            }
            let tui_opts = ServersOpts {
                host: opts.host,
                port: opts.port,
            };
            servers_tui::run(tui_opts)
        }
        HubAction::Stop => stop_daemon(opts, &runtime),
    }
}

fn ensure_daemon(opts: HubOpts, runtime: &HubRuntime) -> Result<()> {
    let host = opts.host;
    let port = opts.port;
    let config_path = opts.config.unwrap_or_else(config::default_config_path);
    let cfg = config::load_or_default(&config_path);

    if runtime.provider == HubProvider::Iris && !cfg.watchers.is_empty() {
        ensure_iris_running(runtime, &config_path, cfg.watchers.len())?;
    }

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

fn stop_daemon(opts: HubOpts, runtime: &HubRuntime) -> Result<()> {
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
        if runtime.provider == HubProvider::Iris {
            stop_iris_process().ok();
        }
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
        if runtime.provider == HubProvider::Iris {
            stop_iris_process().ok();
        }
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
        if runtime.provider == HubProvider::Iris {
            stop_iris_process().ok();
        }
        println!("hub stopped");
        return Ok(());
    }

    if is_daemon_alive(host, port)? {
        bail!("hub daemon is running but unable to determine pid; stop it manually");
    }

    if runtime.provider == HubProvider::Iris {
        stop_iris_process().ok();
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HubRuntime {
    provider: HubProvider,
    binary: PathBuf,
    version: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HubProvider {
    Iris,
    Flow,
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
    println!(
        "Select hub runtime (stored under {}):",
        hub_runtime_path().display()
    );
    println!("1) iris (recommended) – dedicated watcher daemon");
    println!("2) flow (built-in daemon)");
    print!("Choice [1]: ");
    let _ = io::stdout().flush();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    let choice = input.trim();

    match choice {
        "" | "1" => {
            let binary = doctor::ensure_iris_available_interactive()?;
            Ok(HubRuntime {
                provider: HubProvider::Iris,
                version: detect_binary_version(&binary),
                binary,
            })
        }
        "2" => {
            let exe = std::env::current_exe().context("failed to resolve current executable")?;
            Ok(HubRuntime {
                provider: HubProvider::Flow,
                version: detect_binary_version(&exe),
                binary: exe,
            })
        }
        _other => {
            println!("Enter path to hub binary (e.g., iris):");
            let mut path_input = String::new();
            io::stdin().read_line(&mut path_input).ok();
            let path = PathBuf::from(path_input.trim());
            if path.as_os_str().is_empty() {
                bail!("no hub binary specified");
            }
            Ok(HubRuntime {
                provider: HubProvider::Iris,
                version: detect_binary_version(&path),
                binary: path,
            })
        }
    }
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

fn ensure_iris_running(
    runtime: &HubRuntime,
    config_path: &Path,
    watcher_count: usize,
) -> Result<()> {
    if runtime.provider != HubProvider::Iris {
        return Ok(());
    }

    if watcher_count == 0 {
        tracing::info!("no watchers defined; skipping iris launch");
        return Ok(());
    }

    if let Some(pid) = load_iris_pid()? {
        if process_alive(pid)? {
            return Ok(());
        } else {
            remove_iris_pid().ok();
        }
    }

    println!(
        "Starting iris watcher daemon ({} watchers) using {}",
        watcher_count,
        config_path.display()
    );
    start_iris_process(&runtime.binary, config_path)
}

fn start_iris_process(binary: &Path, config_path: &Path) -> Result<()> {
    let mut cmd = Command::new(binary);
    cmd.arg("--config").arg(config_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to start iris from {}", binary.display()))?;
    persist_iris_pid(child.id())?;
    Ok(())
}

fn stop_iris_process() -> Result<()> {
    if let Some(pid) = load_iris_pid()? {
        terminate_process(pid).ok();
        remove_iris_pid().ok();
    }
    Ok(())
}

fn iris_pid_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/flow/iris.pid")
    } else {
        PathBuf::from(".config/flow/iris.pid")
    }
}

fn load_iris_pid() -> Result<Option<u32>> {
    let path = iris_pid_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let pid: u32 = contents.trim().parse().ok().unwrap_or(0);
    if pid == 0 { Ok(None) } else { Ok(Some(pid)) }
}

fn persist_iris_pid(pid: u32) -> Result<()> {
    let path = iris_pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create pid dir {}", parent.display()))?;
    }
    fs::write(&path, pid.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn remove_iris_pid() -> Result<()> {
    let path = iris_pid_path();
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
