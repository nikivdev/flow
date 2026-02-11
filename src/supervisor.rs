use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{DaemonAction, SupervisorAction, SupervisorCommand};
use crate::{config, daemon, projects, running};

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
    #[serde(default)]
    healthy: Option<bool>,
    health_url: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Clone)]
struct ManagedDaemon {
    name: String,
    config_path: Option<PathBuf>,
    restart: config::DaemonRestartPolicy,
    retry_remaining: Option<u32>,
    autostop: bool,
    disabled: bool,
    health_failures: u32,
    restart_attempts: u32,
    next_restart_at: Option<Instant>,
}

#[derive(Default)]
struct SupervisorState {
    managed: HashMap<String, ManagedDaemon>,
}

type SharedState = Arc<Mutex<SupervisorState>>;

pub fn run(cmd: SupervisorCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(SupervisorAction::Status);
    let socket_path = resolve_socket_path(cmd.socket.as_deref())?;

    match action {
        SupervisorAction::Start { boot } => {
            ensure_supervisor_running(&socket_path, true, boot)?;
            Ok(())
        }
        SupervisorAction::Run { boot } => run_server(&socket_path, boot),
        SupervisorAction::Install { boot } => install_launch_agent(&socket_path, boot),
        SupervisorAction::Uninstall => uninstall_launch_agent(&socket_path),
        SupervisorAction::Stop => stop_supervisor(&socket_path),
        SupervisorAction::Status => show_status(&socket_path),
    }
}

pub fn ensure_running(boot: bool, announce: bool) -> Result<()> {
    let socket_path = resolve_socket_path(None)?;
    ensure_supervisor_running(&socket_path, announce, boot)?;
    Ok(())
}

pub fn is_running() -> bool {
    resolve_socket_path(None)
        .map(|path| supervisor_running(&path))
        .unwrap_or(false)
}

pub fn try_handle_daemon_action(action: &DaemonAction, config_path: Option<&Path>) -> Result<bool> {
    let socket_path = resolve_socket_path(None)?;
    if !supervisor_running(&socket_path) {
        if !ensure_supervisor_running(&socket_path, false, false).unwrap_or(false) {
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
            DaemonAction::Status { .. } => SupervisorIpcAction::Status {
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
            DaemonAction::Status { name } => print_status_views(&daemons, name.as_deref()),
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

fn supervisor_log_path() -> Result<PathBuf> {
    let base = config::ensure_global_state_dir()?;
    Ok(base.join("supervisor.log"))
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

fn ensure_supervisor_running(socket_path: &Path, announce: bool, boot: bool) -> Result<bool> {
    if supervisor_running(socket_path) {
        if announce {
            println!("Supervisor already running.");
        }
        return Ok(true);
    }

    if let Some(started) = ensure_supervisor_via_launchd(socket_path, announce, boot)? {
        return Ok(started);
    }

    let exe = std::env::current_exe().context("failed to resolve flow binary")?;
    let mut cmd = Command::new(exe);
    cmd.arg("supervisor").arg("run");
    cmd.arg("--socket").arg(socket_path);
    if boot {
        cmd.arg("--boot");
    }
    cmd.stdin(Stdio::null());

    let log_path = supervisor_log_path().ok();
    if let Some(path) = &log_path {
        let log_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        if let Some(file) = log_file {
            let file_err = file.try_clone().ok();
            cmd.stdout(file);
            if let Some(err) = file_err {
                cmd.stderr(err);
            } else {
                cmd.stderr(Stdio::null());
            }
        } else {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd.spawn().context("failed to start supervisor")?;
    persist_supervisor_pid(child.id())?;

    // Give the socket a moment to come up.
    let mut ready = false;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(150));
        if supervisor_running(socket_path) {
            ready = true;
            break;
        }
    }

    if ready {
        if announce {
            println!("Supervisor started.");
        }
        return Ok(true);
    }

    if announce {
        if let Some(path) = log_path {
            eprintln!(
                "WARN supervisor did not respond after launch. Check {}",
                path.display()
            );
        } else {
            eprintln!("WARN supervisor did not respond after launch.");
        }
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

fn run_server(socket_path: &Path, boot: bool) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }

    if socket_path.exists() {
        if supervisor_running(socket_path) {
            println!("Supervisor already running; exiting.");
            return Ok(());
        }
        fs::remove_file(socket_path).ok();
    }

    #[cfg(unix)]
    let listener = std::os::unix::net::UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind {}", socket_path.display()))?;

    #[cfg(not(unix))]
    {
        bail!("Supervisor IPC is only supported on unix platforms right now.");
    }

    let state = Arc::new(Mutex::new(SupervisorState::default()));
    let active_path = resolve_active_project_config_path();
    bootstrap_daemons(&state, active_path.as_deref(), boot)?;

    let monitor_state = Arc::clone(&state);
    std::thread::spawn(move || {
        if let Err(err) = monitor_daemons(monitor_state) {
            eprintln!("WARN supervisor monitor failed: {err}");
        }
    });

    #[cfg(unix)]
    {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(err) = handle_client(stream, &state) {
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

fn ensure_supervisor_via_launchd(
    socket_path: &Path,
    announce: bool,
    boot: bool,
) -> Result<Option<bool>> {
    #[cfg(target_os = "macos")]
    {
        if !launch_agent_installed() {
            return Ok(None);
        }

        if boot {
            install_launch_agent(socket_path, true)?;
        }

        if announce {
            println!("Starting supervisor via launchd...");
        }

        launch_agent_kickstart()?;

        let mut ready = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(150));
            if supervisor_running(socket_path) {
                ready = true;
                break;
            }
        }

        if ready {
            if announce {
                println!("Supervisor started (launchd).");
            }
            return Ok(Some(true));
        }

        if announce {
            eprintln!("WARN launchd supervisor did not respond.");
        }
        return Ok(Some(false));
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = socket_path;
        let _ = announce;
        let _ = boot;
        Ok(None)
    }
}

#[cfg(target_os = "macos")]
fn launch_agent_label() -> &'static str {
    "io.linsa.flow-supervisor"
}

#[cfg(target_os = "macos")]
fn launch_agent_plist_path() -> Result<PathBuf> {
    let dir = config::expand_path("~/Library/LaunchAgents");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir.join(format!("{}.plist", launch_agent_label())))
}

#[cfg(target_os = "macos")]
fn launch_agent_installed() -> bool {
    launch_agent_plist_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn launch_agent_domain() -> String {
    let uid = unsafe { libc::getuid() };
    format!("gui/{}", uid)
}

#[cfg(target_os = "macos")]
fn launch_agent_target() -> String {
    format!("{}/{}", launch_agent_domain(), launch_agent_label())
}

#[cfg(target_os = "macos")]
fn launch_agent_program_args(socket_path: &Path, boot: bool) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("failed to resolve flow binary")?;
    let mut args = vec![
        exe.to_string_lossy().into_owned(),
        "supervisor".to_string(),
        "run".to_string(),
        "--socket".to_string(),
        socket_path.to_string_lossy().into_owned(),
    ];
    if boot {
        args.push("--boot".to_string());
    }
    Ok(args)
}

#[cfg(target_os = "macos")]
fn launch_agent_plist(socket_path: &Path, boot: bool, log_path: Option<&Path>) -> Result<String> {
    let args = launch_agent_program_args(socket_path, boot)?;
    let mut buf = String::new();
    buf.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    buf.push_str(
        "<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n",
    );
    buf.push_str("<plist version=\"1.0\">\n<dict>\n");
    buf.push_str("  <key>Label</key>\n");
    buf.push_str(&format!("  <string>{}</string>\n", launch_agent_label()));
    buf.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for arg in args {
        buf.push_str(&format!("    <string>{}</string>\n", xml_escape(&arg)));
    }
    buf.push_str("  </array>\n");
    buf.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    buf.push_str("  <key>KeepAlive</key>\n  <dict>\n");
    buf.push_str("    <key>SuccessfulExit</key>\n    <false/>\n");
    buf.push_str("  </dict>\n");
    if let Some(path) = log_path {
        let log = path.to_string_lossy();
        buf.push_str("  <key>StandardOutPath</key>\n");
        buf.push_str(&format!(
            "  <string>{}</string>\n",
            xml_escape(log.as_ref())
        ));
        buf.push_str("  <key>StandardErrorPath</key>\n");
        buf.push_str(&format!(
            "  <string>{}</string>\n",
            xml_escape(log.as_ref())
        ));
    }
    buf.push_str("</dict>\n</plist>\n");
    Ok(buf)
}

#[cfg(target_os = "macos")]
fn install_launch_agent(socket_path: &Path, boot: bool) -> Result<()> {
    let plist_path = launch_agent_plist_path()?;
    let log_path = supervisor_log_path().ok();
    let plist = launch_agent_plist(socket_path, boot, log_path.as_deref())?;
    fs::write(&plist_path, plist)
        .with_context(|| format!("failed to write {}", plist_path.display()))?;

    let domain = launch_agent_domain();
    let target = launch_agent_target();

    let _ = Command::new("launchctl")
        .args(["bootout", &domain, plist_path.to_string_lossy().as_ref()])
        .output();

    let output = Command::new("launchctl")
        .args(["bootstrap", &domain, plist_path.to_string_lossy().as_ref()])
        .output()
        .context("failed to bootstrap launch agent")?;
    if !output.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let _ = Command::new("launchctl").args(["enable", &target]).output();

    let output = Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()
        .context("failed to kickstart launch agent")?;
    if !output.status.success() {
        bail!(
            "launchctl kickstart failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!(
        "Installed launch agent {} at {}",
        launch_agent_label(),
        plist_path.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn launch_agent_kickstart() -> Result<()> {
    let target = launch_agent_target();
    let output = Command::new("launchctl")
        .args(["kickstart", "-k", &target])
        .output()
        .context("failed to kickstart launch agent")?;
    if !output.status.success() {
        bail!(
            "launchctl kickstart failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_launch_agent(_socket_path: &Path) -> Result<()> {
    let plist_path = launch_agent_plist_path()?;
    let domain = launch_agent_domain();

    let _ = Command::new("launchctl")
        .args(["bootout", &domain, plist_path.to_string_lossy().as_ref()])
        .output();

    if plist_path.exists() {
        fs::remove_file(&plist_path)
            .with_context(|| format!("failed to remove {}", plist_path.display()))?;
    }

    println!("Removed launch agent {}", launch_agent_label());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn install_launch_agent(_socket_path: &Path, _boot: bool) -> Result<()> {
    bail!("launch agent install is only supported on macOS");
}

#[cfg(not(target_os = "macos"))]
fn uninstall_launch_agent(_socket_path: &Path) -> Result<()> {
    bail!("launch agent uninstall is only supported on macOS");
}

#[cfg(target_os = "macos")]
fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}
#[cfg(unix)]
fn handle_client(stream: std::os::unix::net::UnixStream, state: &SharedState) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    if line.trim().is_empty() {
        return Ok(());
    }

    let request: IpcRequest = serde_json::from_str(line.trim())?;
    let response = handle_request(request, state)?;

    let mut writer = &stream;
    let payload = serde_json::to_string(&response)?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn handle_request(request: IpcRequest, state: &SharedState) -> Result<IpcResponse> {
    match request.action {
        SupervisorIpcAction::Ping => Ok(IpcResponse {
            ok: true,
            message: Some("pong".to_string()),
            daemons: None,
        }),
        SupervisorIpcAction::StartDaemon { name, config_path } => {
            let path = resolve_config_path(config_path.as_deref());
            let daemon_cfg = resolve_daemon_config(&name, path.as_deref())?;
            daemon::start_daemon_with_path(&name, path.as_deref())?;
            register_managed_daemon(state, &daemon_cfg, path.as_deref(), false)?;
            Ok(IpcResponse {
                ok: true,
                message: Some(format!("{} started", name)),
                daemons: None,
            })
        }
        SupervisorIpcAction::StopDaemon { name, config_path } => {
            let path = resolve_config_path(config_path.as_deref());
            daemon::stop_daemon_with_path(&name, path.as_deref())?;
            disable_managed_daemon(state, &name, path.as_deref())?;
            Ok(IpcResponse {
                ok: true,
                message: Some(format!("{} stopped", name)),
                daemons: None,
            })
        }
        SupervisorIpcAction::RestartDaemon { name, config_path } => {
            let path = resolve_config_path(config_path.as_deref());
            let daemon_cfg = resolve_daemon_config(&name, path.as_deref())?;
            daemon::stop_daemon_with_path(&name, path.as_deref()).ok();
            std::thread::sleep(Duration::from_millis(300));
            daemon::start_daemon_with_path(&name, path.as_deref())?;
            register_managed_daemon(state, &daemon_cfg, path.as_deref(), false)?;
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
            healthy: status.healthy,
            health_url,
            description,
        });
    }
    Ok(views)
}

fn resolve_config_path(config_path: Option<&str>) -> Option<PathBuf> {
    config_path.map(|path| config::expand_path(path))
}

fn resolve_active_project_config_path() -> Option<PathBuf> {
    let name = projects::get_active_project()?;
    let entry = projects::resolve_project(&name).ok().flatten()?;
    Some(entry.config_path)
}

fn bootstrap_daemons(
    state: &SharedState,
    active_config_path: Option<&Path>,
    boot: bool,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();

    let global_path = config::default_config_path();
    if global_path.exists() {
        if let Ok(cfg) = config::load(&global_path) {
            start_daemon_set(state, cfg.daemons, None, boot, &mut seen)?;
        }
    }

    if let Some(path) = active_config_path {
        if path.exists() {
            if let Ok(cfg) = config::load(path) {
                start_daemon_set(state, cfg.daemons, Some(path), boot, &mut seen)?;
            }
        }
    }

    Ok(())
}

fn start_daemon_set(
    state: &SharedState,
    daemons: Vec<config::DaemonConfig>,
    config_path: Option<&Path>,
    boot: bool,
    seen: &mut std::collections::HashSet<String>,
) -> Result<()> {
    for daemon_cfg in daemons {
        if !should_start_daemon(&daemon_cfg, boot) {
            continue;
        }

        let key = daemon_key(&daemon_cfg.name, config_path);
        if !seen.insert(key) {
            continue;
        }

        match daemon::start_daemon_with_path(&daemon_cfg.name, config_path) {
            Ok(()) => {
                register_managed_daemon(state, &daemon_cfg, config_path, false)?;
            }
            Err(err) => {
                eprintln!("WARN failed to autostart {}: {}", daemon_cfg.name, err);
            }
        }
    }
    Ok(())
}

fn should_start_daemon(daemon_cfg: &config::DaemonConfig, boot: bool) -> bool {
    if daemon_cfg.autostart {
        return true;
    }
    if boot && daemon_cfg.boot {
        return true;
    }
    false
}

fn should_restart(entry: &ManagedDaemon) -> bool {
    match entry.restart {
        config::DaemonRestartPolicy::Never => false,
        config::DaemonRestartPolicy::Always => true,
        config::DaemonRestartPolicy::OnFailure => true,
    }
}

fn monitor_daemons(state: SharedState) -> Result<()> {
    let mut last_active = resolve_active_project_config_path()
        .as_deref()
        .map(normalize_path);

    loop {
        std::thread::sleep(Duration::from_secs(2));
        let now = Instant::now();

        let active_path = resolve_active_project_config_path()
            .as_deref()
            .map(normalize_path);
        if active_path != last_active {
            bootstrap_daemons(&state, active_path.as_deref(), false).ok();
            last_active = active_path.clone();
        }

        let entries: Vec<ManagedDaemon> = {
            let state = state.lock().expect("supervisor state lock");
            state.managed.values().cloned().collect()
        };

        let mut to_restart: Vec<(String, Option<PathBuf>)> = Vec::new();
        let mut to_stop: Vec<(String, Option<PathBuf>)> = Vec::new();
        let mut updates: Vec<(String, Option<u32>, bool, u32, u32, Option<Instant>)> = Vec::new();

        for entry in entries {
            if entry.disabled {
                continue;
            }

            if entry.autostop {
                if let Some(ref path) = entry.config_path {
                    if !active_path_matches(&active_path, path) {
                        to_stop.push((entry.name.clone(), entry.config_path.clone()));
                        updates.push((
                            daemon_key(&entry.name, entry.config_path.as_deref()),
                            entry.retry_remaining,
                            true,
                            entry.health_failures,
                            entry.restart_attempts,
                            entry.next_restart_at,
                        ));
                        continue;
                    }
                }
            }

            let config_path = entry.config_path.clone();
            let daemon_cfg = match resolve_daemon_config(&entry.name, config_path.as_deref()) {
                Ok(cfg) => cfg,
                Err(err) => {
                    eprintln!(
                        "WARN supervisor missing daemon config for {}: {}",
                        entry.name, err
                    );
                    continue;
                }
            };

            let status = daemon::get_daemon_status(&daemon_cfg);
            if status.running {
                if status.healthy == Some(false) {
                    let key = daemon_key(&entry.name, config_path.as_deref());
                    let failures = entry.health_failures.saturating_add(1);
                    let should_restart_for_health = failures >= 3;
                    if should_restart_for_health && should_restart(&entry) {
                        if entry
                            .next_restart_at
                            .map(|deadline| now < deadline)
                            .unwrap_or(false)
                        {
                            updates.push((
                                key,
                                entry.retry_remaining,
                                false,
                                failures,
                                entry.restart_attempts,
                                entry.next_restart_at,
                            ));
                            continue;
                        }

                        let delay_secs = 2u64
                            .saturating_pow(entry.restart_attempts.saturating_add(1))
                            .min(60);
                        let next_restart_at = Some(now + Duration::from_secs(delay_secs));
                        updates.push((
                            key,
                            entry.retry_remaining,
                            false,
                            failures,
                            entry.restart_attempts.saturating_add(1),
                            next_restart_at,
                        ));
                        to_restart.push((entry.name.clone(), config_path));
                    } else {
                        updates.push((
                            key,
                            entry.retry_remaining,
                            false,
                            failures,
                            entry.restart_attempts,
                            entry.next_restart_at,
                        ));
                    }
                } else if entry.health_failures != 0 || entry.restart_attempts != 0 {
                    let key = daemon_key(&entry.name, config_path.as_deref());
                    updates.push((key, entry.retry_remaining, false, 0, 0, None));
                }
                continue;
            }

            let key = daemon_key(&entry.name, config_path.as_deref());
            if !should_restart(&entry) {
                continue;
            }
            if entry
                .next_restart_at
                .map(|deadline| now < deadline)
                .unwrap_or(false)
            {
                updates.push((
                    key,
                    entry.retry_remaining,
                    false,
                    entry.health_failures,
                    entry.restart_attempts,
                    entry.next_restart_at,
                ));
                continue;
            }

            let mut retry_remaining = entry.retry_remaining;
            if entry.restart != config::DaemonRestartPolicy::Always {
                if let Some(remaining) = retry_remaining {
                    if remaining == 0 {
                        continue;
                    }
                    retry_remaining = Some(remaining.saturating_sub(1));
                }
            }

            let delay_secs = 2u64
                .saturating_pow(entry.restart_attempts.saturating_add(1))
                .min(60);
            updates.push((
                key,
                retry_remaining,
                false,
                entry.health_failures.saturating_add(1),
                entry.restart_attempts.saturating_add(1),
                Some(now + Duration::from_secs(delay_secs)),
            ));
            to_restart.push((entry.name.clone(), config_path));
        }

        if !updates.is_empty() {
            let mut state = state.lock().expect("supervisor state lock");
            for (
                key,
                retry_remaining,
                disabled,
                health_failures,
                restart_attempts,
                next_restart_at,
            ) in updates
            {
                if let Some(entry) = state.managed.get_mut(&key) {
                    entry.retry_remaining = retry_remaining;
                    entry.disabled = disabled;
                    entry.health_failures = health_failures;
                    entry.restart_attempts = restart_attempts;
                    entry.next_restart_at = next_restart_at;
                }
            }
        }

        for (name, config_path) in to_stop {
            daemon::stop_daemon_with_path(&name, config_path.as_deref()).ok();
        }

        for (name, config_path) in to_restart {
            daemon::start_daemon_with_path(&name, config_path.as_deref()).ok();
        }
    }
}

fn active_path_matches(active: &Option<PathBuf>, candidate: &Path) -> bool {
    match active {
        Some(active_path) => active_path == &normalize_path(candidate),
        None => false,
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_daemon_config(name: &str, config_path: Option<&Path>) -> Result<config::DaemonConfig> {
    let cfg = daemon::load_merged_config_with_path(config_path)?;
    cfg.daemons
        .into_iter()
        .find(|daemon| daemon.name == name)
        .ok_or_else(|| anyhow::anyhow!("daemon '{}' not found in config", name))
}

fn register_managed_daemon(
    state: &SharedState,
    daemon_cfg: &config::DaemonConfig,
    config_path: Option<&Path>,
    disabled: bool,
) -> Result<()> {
    let mut state = state.lock().expect("supervisor state lock");
    let key = daemon_key(&daemon_cfg.name, config_path);
    let entry = ManagedDaemon {
        name: daemon_cfg.name.clone(),
        config_path: config_path.map(|path| path.to_path_buf()),
        restart: daemon::restart_policy_for(daemon_cfg),
        retry_remaining: daemon_cfg.retry,
        autostop: daemon_cfg.autostop,
        disabled,
        health_failures: 0,
        restart_attempts: 0,
        next_restart_at: None,
    };
    state.managed.insert(key, entry);
    Ok(())
}

fn disable_managed_daemon(
    state: &SharedState,
    name: &str,
    config_path: Option<&Path>,
) -> Result<()> {
    let mut state = state.lock().expect("supervisor state lock");
    let key = daemon_key(name, config_path);
    if let Some(entry) = state.managed.get_mut(&key) {
        entry.disabled = true;
        return Ok(());
    }

    if let Ok(cfg) = resolve_daemon_config(name, config_path) {
        let entry = ManagedDaemon {
            name: cfg.name.clone(),
            config_path: config_path.map(|path| path.to_path_buf()),
            restart: daemon::restart_policy_for(&cfg),
            retry_remaining: cfg.retry,
            autostop: cfg.autostop,
            disabled: true,
            health_failures: 0,
            restart_attempts: 0,
            next_restart_at: None,
        };
        state.managed.insert(key, entry);
    }
    Ok(())
}

fn daemon_key(name: &str, config_path: Option<&Path>) -> String {
    match config_path {
        Some(path) => format!("{}::{}", name, path.display()),
        None => name.to_string(),
    }
}

fn print_status_views(daemons: &[DaemonStatusView], filter: Option<&str>) {
    if daemons.is_empty() {
        println!("No daemons configured.");
        return;
    }

    let mut matched = false;
    println!("Daemon Status:");
    println!();
    for daemon in daemons {
        if let Some(name) = filter {
            if daemon.name != name {
                continue;
            }
        }
        matched = true;
        let icon = if daemon.running {
            if daemon.healthy == Some(false) {
                "WARN"
            } else {
                "OK"
            }
        } else {
            "NO"
        };
        let state = if daemon.running { "running" } else { "stopped" };
        print!("  {} {}: {}", icon, daemon.name, state);
        if let Some(url) = &daemon.health_url {
            if daemon.running {
                if daemon.healthy == Some(false) {
                    print!(" (unhealthy: {})", url.replace("/health", ""));
                } else {
                    print!(" ({})", url.replace("/health", ""));
                }
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

    if let Some(name) = filter {
        if !matched {
            println!("Daemon '{}' not found.", name);
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
    if pid == 0 { Ok(None) } else { Ok(Some(pid)) }
}

fn remove_supervisor_pid(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).ok();
    }
    Ok(())
}

fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("kill").arg("-9").arg(pid.to_string()).status();
    if let Ok(status) = status {
        if status.success() {
            return Ok(());
        }
    }
    bail!("failed to stop supervisor process {}", pid)
}
