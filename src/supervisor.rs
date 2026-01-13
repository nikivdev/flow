use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{DaemonAction, SupervisorAction, SupervisorCommand};
use crate::{config, daemon, running};

#[derive(Debug, Serialize, Deserialize)]
struct IpcRequest {
    action: SupervisorIpcAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SupervisorIpcAction {
    Ping,
    StartDaemon {
        name: String,
        config_path: Option<String>,
    },
    StopDaemon {
        name: String,
        config_path: Option<String>,
    },
    RestartDaemon {
        name: String,
        config_path: Option<String>,
    },
    Status {
        config_path: Option<String>,
    },
    List {
        config_path: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcResponse {
    ok: bool,
    message: Option<String>,
    daemons: Option<Vec<DaemonStatusView>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonStatusView {
    name: String,
    running: bool,
    pid: Option<u32>,
    health_url: Option<String>,
    description: Option<String>,
}

pub fn run(cmd: SupervisorCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(SupervisorAction::Status);
    let socket_path = resolve_socket_path(cmd.socket.as_deref())?;

    match action {
        SupervisorAction::Start => {
            ensure_supervisor_running(&socket_path, true)?;
            Ok(())
        }
        SupervisorAction::Run => run_server(&socket_path),
        SupervisorAction::Stop => stop_supervisor(&socket_path),
        SupervisorAction::Status => show_status(&socket_path),
    }
}

pub fn try_handle_daemon_action(
    action: &DaemonAction,
    config_path: Option<&Path>,
) -> Result<bool> {
    let socket_path = resolve_socket_path(None)?;
    if !supervisor_running(&socket_path) {
        if !ensure_supervisor_running(&socket_path, false).unwrap_or(false) {
            return Ok(false);
        }
    }

    let request = IpcRequest {
        action: match action {
            DaemonAction::Start { name } => SupervisorIpcAction::StartDaemon {
                name: name.clone(),
                config_path: config_path.map(|p| p.display().to_string()),
            },
            DaemonAction::Stop { name } => SupervisorIpcAction::StopDaemon {
                name: name.clone(),
                config_path: config_path.map(|p| p.display().to_string()),
            },
            DaemonAction::Restart { name } => SupervisorIpcAction::RestartDaemon {
                name: name.clone(),
                config_path: config_path.map(|p| p.display().to_string()),
            },
            DaemonAction::Status => SupervisorIpcAction::Status {
                config_path: config_path.map(|p| p.display().to_string()),
            },
            DaemonAction::List => SupervisorIpcAction::List {
                config_path: config_path.map(|p| p.display().to_string()),
            },
        },
    };

    let response = match send_request(&socket_path, &request) {
        Ok(resp) => resp,
        Err(_) => return Ok(false),
    };

    if !response.ok {
        if let Some(message) = response.message {
            eprintln!("WARN supervisor error: {}", message);
        }
        return Ok(false);
    }

    if let Some(daemons) = response.daemons {
        match action {
            DaemonAction::Status => print_status_views(&daemons),
            DaemonAction::List => print_list_views(&daemons),
            _ => {}
        }
        return Ok(true);
    }

    if let Some(message) = response.message {
        println!("OK {}", message);
    }

    Ok(true)
}

fn resolve_socket_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(config::expand_path(&path.to_string_lossy()));
    }
    let base = config::ensure_global_state_dir()?;
    Ok(base.join("supervisor.sock"))
}

fn supervisor_pid_path() -> Result<PathBuf> {
    let base = config::ensure_global_state_dir()?;
    Ok(base.join("supervisor.pid"))
}

fn supervisor_running(socket_path: &Path) -> bool {
    if !socket_path.exists() {
        return false;
    }

    let request = IpcRequest {
        action: SupervisorIpcAction::Ping,
    };
    send_request(socket_path, &request)
        .map(|resp| resp.ok)
        .unwrap_or(false)
}

fn ensure_supervisor_running(socket_path: &Path, announce: bool) -> Result<bool> {
    if supervisor_running(socket_path) {
        if announce {
            println!("Supervisor already running.");
        }
        return Ok(true);
    }

    let exe = std::env::current_exe().context("failed to resolve flow binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("supervisor").arg("run");
    cmd.arg("--socket").arg(socket_path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().context("failed to start supervisor")?;
    persist_supervisor_pid(child.id())?;

    // Give the socket a moment to come up.
    std::thread::sleep(Duration::from_millis(300));

    if supervisor_running(socket_path) {
        if announce {
            println!("Supervisor started.");
        }
        return Ok(true);
    }

    if announce {
        eprintln!("WARN supervisor did not respond after launch.");
    }
    Ok(false)
}

fn show_status(socket_path: &Path) -> Result<()> {
    if supervisor_running(socket_path) {
        println!("Supervisor running (socket: {}).", socket_path.display());
        return Ok(());
    }
    println!("Supervisor not running.");
    Ok(())
}

fn stop_supervisor(socket_path: &Path) -> Result<()> {
    if let Ok(pid_path) = supervisor_pid_path() {
        if let Ok(Some(pid)) = load_supervisor_pid(&pid_path) {
            if running::process_alive(pid) {
                terminate_process(pid).ok();
            }
            remove_supervisor_pid(&pid_path).ok();
        }
    }

    if socket_path.exists() {
        fs::remove_file(socket_path).ok();
    }

    println!("Supervisor stopped (if it was running).");
    Ok(())
}

fn run_server(socket_path: &Path) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if socket_path.exists() {
        fs::remove_file(socket_path).ok();
    }

    #[cfg(unix)]
    let listener = std::os::unix::net::UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;

    #[cfg(not(unix))]
    {
        bail!("Supervisor IPC is only supported on unix platforms right now.");
    }

    #[cfg(unix)]
    {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(err) = handle_client(stream) {
                        eprintln!("WARN supervisor request failed: {err}");
                    }
                }
                Err(err) => {
                    eprintln!("WARN supervisor accept failed: {err}");
                }
            }
        }
    }

    Ok(())
}

#[cfg(unix)]
fn handle_client(stream: std::os::unix::net::UnixStream) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    if line.trim().is_empty() {
        return Ok(());
    }

    let request: IpcRequest = serde_json::from_str(line.trim())?;
    let response = handle_request(request)?;

    let mut writer = &stream;
    let payload = serde_json::to_string(&response)?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn handle_request(request: IpcRequest) -> Result<IpcResponse> {
    match request.action {
        SupervisorIpcAction::Ping => Ok(IpcResponse {
            ok: true,
            message: Some("pong".to_string()),
            daemons: None,
        }),
        SupervisorIpcAction::StartDaemon { name, config_path } => {
            let path = resolve_config_path(config_path.as_deref());
            daemon::start_daemon_with_path(&name, path.as_deref())?;
            Ok(IpcResponse {
                ok: true,
                message: Some(format!("{} started", name)),
                daemons: None,
            })
        }
        SupervisorIpcAction::StopDaemon { name, config_path } => {
            let path = resolve_config_path(config_path.as_deref());
            daemon::stop_daemon_with_path(&name, path.as_deref())?;
            Ok(IpcResponse {
                ok: true,
                message: Some(format!("{} stopped", name)),
                daemons: None,
            })
        }
        SupervisorIpcAction::RestartDaemon { name, config_path } => {
            let path = resolve_config_path(config_path.as_deref());
            daemon::stop_daemon_with_path(&name, path.as_deref()).ok();
            std::thread::sleep(Duration::from_millis(300));
            daemon::start_daemon_with_path(&name, path.as_deref())?;
            Ok(IpcResponse {
                ok: true,
                message: Some(format!("{} restarted", name)),
                daemons: None,
            })
        }
        SupervisorIpcAction::Status { config_path } => {
            let views = build_status_views(resolve_config_path(config_path.as_deref()).as_deref())?;
            Ok(IpcResponse {
                ok: true,
                message: None,
                daemons: Some(views),
            })
        }
        SupervisorIpcAction::List { config_path } => {
            let views = build_status_views(resolve_config_path(config_path.as_deref()).as_deref())?;
            Ok(IpcResponse {
                ok: true,
                message: None,
                daemons: Some(views),
            })
        }
    }
}

fn build_status_views(config_path: Option<&Path>) -> Result<Vec<DaemonStatusView>> {
    let config = daemon::load_merged_config_with_path(config_path)?;
    let mut views = Vec::new();
    for daemon_cfg in config.daemons {
        let status = daemon::get_daemon_status(&daemon_cfg);
        let name = daemon_cfg.name.clone();
        let description = daemon_cfg.description.clone();
        let health_url = daemon_cfg.effective_health_url();
        views.push(DaemonStatusView {
            name,
            running: status.running,
            pid: status.pid,
            health_url,
            description,
        });
    }
    Ok(views)
}

fn resolve_config_path(config_path: Option<&str>) -> Option<PathBuf> {
    config_path.map(|path| config::expand_path(path))
}

fn print_status_views(daemons: &[DaemonStatusView]) {
    if daemons.is_empty() {
        println!("No daemons configured.");
        return;
    }

    println!("Daemon Status:");
    println!();
    for daemon in daemons {
        let icon = if daemon.running { "OK" } else { "NO" };
        let state = if daemon.running { "running" } else { "stopped" };
        print!("  {} {}: {}", icon, daemon.name, state);
        if let Some(url) = &daemon.health_url {
            if daemon.running {
                print!(" ({})", url.replace("/health", ""));
            }
        }
        if let Some(pid) = daemon.pid {
            print!(" [PID {}]", pid);
        }
        println!();
        if let Some(desc) = &daemon.description {
            println!("      {}", desc);
        }
    }
}

fn print_list_views(daemons: &[DaemonStatusView]) {
    if daemons.is_empty() {
        println!("No daemons configured.");
        return;
    }
    println!("Available daemons:");
    for daemon in daemons {
        print!("  {}", daemon.name);
        if let Some(desc) = &daemon.description {
            print!(" - {}", desc);
        }
        println!();
    }
}

fn send_request(socket_path: &Path, request: &IpcRequest) -> Result<IpcResponse> {
    #[cfg(unix)]
    {
        let mut stream = std::os::unix::net::UnixStream::connect(socket_path)?;
        let payload = serde_json::to_string(request)?;
        stream.write_all(payload.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let response: IpcResponse = serde_json::from_str(line.trim())?;
        Ok(response)
    }
    #[cfg(not(unix))]
    {
        let _ = socket_path;
        let _ = request;
        bail!("Supervisor IPC is only supported on unix platforms right now.");
    }
}

fn persist_supervisor_pid(pid: u32) -> Result<()> {
    let path = supervisor_pid_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, pid.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_supervisor_pid(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)?;
    let pid: u32 = contents.trim().parse().ok().unwrap_or(0);
    if pid == 0 {
        Ok(None)
    } else {
        Ok(Some(pid))
    }
}

fn remove_supervisor_pid(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).ok();
    }
    Ok(())
}

fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
    if let Ok(status) = status {
        if status.success() {
            return Ok(());
        }
    }
    bail!("failed to stop supervisor process {}", pid)
}
