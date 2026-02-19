use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::cli::{DomainsAction, DomainsAddOpts, DomainsCommand, DomainsEngineArg, DomainsRmOpts};

const PROXY_CONTAINER_NAME: &str = "flow-local-domains-proxy";
const NATIVE_PROXY_HEADER: &str = "x-flow-domainsd: 1";
const MACOS_DOMAINSD_LABEL: &str = "dev.flow.domainsd";

const COMPOSE_FILE: &str = r#"services:
  proxy:
    container_name: flow-local-domains-proxy
    image: nginx:1.27-alpine
    restart: unless-stopped
    ports:
      - "80:80"
    volumes:
      - ./nginx/default.conf:/etc/nginx/conf.d/default.conf:ro
      - ./routes:/etc/nginx/conf.d/routes:ro
"#;

const NGINX_MAIN_CONF: &str = r#"map $http_upgrade $connection_upgrade {
  default upgrade;
  "" close;
}

server {
  listen 80 default_server;
  server_name _;
  return 404 "No local Flow domain route configured for this host.\n";
}

include /etc/nginx/conf.d/routes/*.conf;
"#;

#[derive(Debug)]
struct DomainsPaths {
    root: PathBuf,
    compose: PathBuf,
    nginx_main: PathBuf,
    routes_dir: PathBuf,
    routes_state: PathBuf,
    native_pid: PathBuf,
    native_log: PathBuf,
    native_bin: PathBuf,
}

impl DomainsPaths {
    fn resolve() -> Result<Self> {
        let cfg = dirs::config_dir().context("Could not find config directory")?;
        let root = cfg.join("flow").join("local-domains");
        Ok(Self {
            compose: root.join("docker-compose.yml"),
            nginx_main: root.join("nginx").join("default.conf"),
            routes_dir: root.join("routes"),
            routes_state: root.join("routes.json"),
            native_pid: root.join("domainsd.pid"),
            native_log: root.join("domainsd.log"),
            native_bin: root.join("domainsd-cpp"),
            root,
        })
    }
}

pub fn run(cmd: DomainsCommand) -> Result<()> {
    let paths = DomainsPaths::resolve()?;
    let engine = resolve_engine(cmd.engine);
    match cmd.action {
        Some(DomainsAction::Up) => run_up(&paths, engine),
        Some(DomainsAction::Down) => run_down(&paths, engine),
        Some(DomainsAction::List) | None => run_list(&paths),
        Some(DomainsAction::Add(opts)) => run_add(&paths, opts, engine),
        Some(DomainsAction::Rm(opts)) => run_rm(&paths, opts, engine),
        Some(DomainsAction::Doctor) => run_doctor(&paths, engine),
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum DomainsEngine {
    Docker,
    Native,
}

fn resolve_engine(cli_engine: Option<DomainsEngineArg>) -> DomainsEngine {
    if let Some(engine) = cli_engine {
        return match engine {
            DomainsEngineArg::Docker => DomainsEngine::Docker,
            DomainsEngineArg::Native => DomainsEngine::Native,
        };
    }
    match std::env::var("FLOW_DOMAINS_ENGINE") {
        Ok(v) if v.eq_ignore_ascii_case("native") => DomainsEngine::Native,
        _ => DomainsEngine::Docker,
    }
}

fn run_up(paths: &DomainsPaths, engine: DomainsEngine) -> Result<()> {
    if engine == DomainsEngine::Native {
        return run_up_native(paths);
    }
    ensure_docker_available()?;
    ensure_layout(paths)?;
    let routes = load_routes(paths)?;
    write_route_files(paths, &routes)?;
    assert_no_port_80_conflict()?;

    run_compose(paths, &["up", "-d"])?;
    println!("Local domains proxy is up (container: {PROXY_CONTAINER_NAME}).");
    println!("Config root: {}", paths.root.display());
    println!("Routes: {}", routes.len());
    if routes.is_empty() {
        println!("No routes yet. Add one with:");
        println!("  f domains add linsa.localhost 127.0.0.1:3481");
    }
    Ok(())
}

fn run_down(paths: &DomainsPaths, engine: DomainsEngine) -> Result<()> {
    if engine == DomainsEngine::Native {
        return run_down_native(paths);
    }
    ensure_docker_available()?;
    ensure_layout(paths)?;
    run_compose(paths, &["down"])?;
    println!("Local domains proxy stopped.");
    Ok(())
}

fn run_list(paths: &DomainsPaths) -> Result<()> {
    ensure_layout(paths)?;
    let routes = load_routes(paths)?;
    if routes.is_empty() {
        println!("No local domain routes configured.");
        println!("Add one with: f domains add linsa.localhost 127.0.0.1:3481");
        return Ok(());
    }

    println!("{:<32} {}", "HOST", "TARGET");
    println!("{}", "-".repeat(58));
    for (host, target) in routes {
        println!("{:<32} {}", host, target);
    }
    Ok(())
}

fn run_add(paths: &DomainsPaths, opts: DomainsAddOpts, engine: DomainsEngine) -> Result<()> {
    ensure_layout(paths)?;
    let host = normalize_host(&opts.host)?;
    let target = normalize_target(&opts.target)?;

    let mut routes = load_routes(paths)?;
    if let Some(existing) = routes.get(&host) {
        if existing == &target {
            println!("Route already exists: {host} -> {target}");
            maybe_reload_running_proxy(paths, engine)?;
            return Ok(());
        }
        if !opts.replace {
            bail!(
                "Route already exists: {} -> {}. Use --replace to update.",
                host,
                existing
            );
        }
    }

    routes.insert(host.clone(), target.clone());
    save_routes(paths, &routes)?;
    write_route_files(paths, &routes)?;
    maybe_reload_running_proxy(paths, engine)?;
    println!("Added route: {host} -> {target}");
    Ok(())
}

fn run_rm(paths: &DomainsPaths, opts: DomainsRmOpts, engine: DomainsEngine) -> Result<()> {
    ensure_layout(paths)?;
    let host = normalize_host(&opts.host)?;
    let mut routes = load_routes(paths)?;
    if routes.remove(&host).is_none() {
        bail!("Route not found: {}", host);
    }
    save_routes(paths, &routes)?;
    write_route_files(paths, &routes)?;
    maybe_reload_running_proxy(paths, engine)?;
    println!("Removed route: {host}");
    Ok(())
}

fn run_doctor(paths: &DomainsPaths, engine: DomainsEngine) -> Result<()> {
    if engine == DomainsEngine::Native {
        return run_doctor_native(paths);
    }
    ensure_layout(paths)?;
    let routes = load_routes(paths)?;

    println!("Local domains doctor");
    println!("--------------------");
    println!("Config root: {}", paths.root.display());
    println!("Routes: {}", routes.len());
    println!(
        "Docker: {}",
        if docker_available() {
            "available"
        } else {
            "missing"
        }
    );

    let running = proxy_is_running()?;
    println!(
        "Proxy container: {}",
        if running { "running" } else { "stopped" }
    );

    if let Some(owner) = docker_container_owning_port_80()? {
        if owner == PROXY_CONTAINER_NAME {
            println!("Port 80 owner: {} (expected)", owner);
        } else {
            println!("Port 80 owner: {} (conflict)", owner);
        }
    } else if let Some(listener) = port_80_listener_summary()? {
        println!("Port 80 listener: {}", listener);
    } else {
        println!("Port 80 listener: none");
    }

    if !routes.is_empty() {
        println!();
        println!("{:<32} {}", "HOST", "TARGET");
        println!("{}", "-".repeat(58));
        for (host, target) in routes {
            println!("{:<32} {}", host, target);
        }
    }

    Ok(())
}

fn normalize_host(raw: &str) -> Result<String> {
    let mut host = raw.trim().to_ascii_lowercase();
    if let Some(stripped) = host.strip_prefix("http://") {
        host = stripped.to_string();
    } else if let Some(stripped) = host.strip_prefix("https://") {
        host = stripped.to_string();
    }
    host = host.trim_end_matches('/').to_string();
    if host.is_empty() {
        bail!("Host is empty");
    }
    if host.contains('/') || host.contains(':') || host.contains(char::is_whitespace) {
        bail!("Host must be a hostname like linsa.localhost");
    }
    if !host.ends_with(".localhost") {
        bail!("Host must end with .localhost");
    }
    if host == ".localhost" || host == "localhost" {
        bail!("Host must include a subdomain (for example: linsa.localhost)");
    }
    Ok(host)
}

fn normalize_target(raw: &str) -> Result<String> {
    let mut target = raw.trim().to_string();
    if let Some(stripped) = target.strip_prefix("http://") {
        target = stripped.to_string();
    } else if let Some(stripped) = target.strip_prefix("https://") {
        target = stripped.to_string();
    }
    target = target.trim_end_matches('/').to_string();
    if target.is_empty() {
        bail!("Target is empty");
    }
    if target.contains('/') || target.contains('?') || target.contains('#') {
        bail!("Target must be host:port");
    }

    let (host, port) = target
        .rsplit_once(':')
        .context("Target must include port (for example: 127.0.0.1:3481)")?;
    if host.trim().is_empty() {
        bail!("Target host is empty");
    }
    let port_num = port
        .trim()
        .parse::<u16>()
        .context("Target port must be a valid number")?;
    Ok(format!("{}:{}", host.trim(), port_num))
}

fn ensure_layout(paths: &DomainsPaths) -> Result<()> {
    fs::create_dir_all(paths.routes_dir.as_path())?;
    if let Some(parent) = paths.nginx_main.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&paths.compose, COMPOSE_FILE)?;
    fs::write(&paths.nginx_main, NGINX_MAIN_CONF)?;

    if !paths.routes_state.exists() {
        fs::write(&paths.routes_state, "{}\n")?;
    }
    Ok(())
}

fn load_routes(paths: &DomainsPaths) -> Result<BTreeMap<String, String>> {
    if !paths.routes_state.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(&paths.routes_state)?;
    let parsed: BTreeMap<String, String> =
        serde_json::from_str(&raw).context("Failed to parse routes.json")?;
    Ok(parsed)
}

fn save_routes(paths: &DomainsPaths, routes: &BTreeMap<String, String>) -> Result<()> {
    let payload = serde_json::to_string_pretty(routes)?;
    fs::write(&paths.routes_state, format!("{payload}\n"))?;
    Ok(())
}

fn write_route_files(paths: &DomainsPaths, routes: &BTreeMap<String, String>) -> Result<()> {
    fs::create_dir_all(&paths.routes_dir)?;
    for entry in fs::read_dir(&paths.routes_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext == "conf")
            .unwrap_or(false)
        {
            fs::remove_file(path)?;
        }
    }

    for (host, target) in routes {
        let file = paths.routes_dir.join(route_file_name(host));
        fs::write(file, render_route(host, target))?;
    }
    Ok(())
}

fn route_file_name(host: &str) -> String {
    let mut safe = String::with_capacity(host.len());
    for ch in host.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    format!("{safe}.conf")
}

fn render_route(host: &str, target: &str) -> String {
    let (upstream_target, host_header) = docker_upstream(target);
    format!(
        r#"server {{
  listen 80;
  server_name {host};

  location / {{
    proxy_pass http://{upstream_target};
    proxy_http_version 1.1;
    proxy_set_header Host {host_header};
    proxy_set_header X-Forwarded-Host $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;
  }}
}}
"#
    )
}

fn docker_upstream(target: &str) -> (String, String) {
    let Some((host, port)) = target.rsplit_once(':') else {
        return (target.to_string(), target.to_string());
    };
    match host {
        "127.0.0.1" | "localhost" | "::1" => (
            format!("host.docker.internal:{}", port),
            "localhost".to_string(),
        ),
        _ => (format!("{}:{}", host, port), host.to_string()),
    }
}

fn ensure_docker_available() -> Result<()> {
    if docker_available() {
        Ok(())
    } else {
        bail!("docker is required for local domains. Install Docker/OrbStack first.")
    }
}

fn docker_available() -> bool {
    which::which("docker").is_ok()
}

fn run_compose(paths: &DomainsPaths, args: &[&str]) -> Result<()> {
    let output = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&paths.compose)
        .args(args)
        .output()
        .context("Failed to run docker compose")?;
    ensure_success(output, "docker compose command failed")
}

fn ensure_success(output: Output, context_msg: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    if detail.is_empty() {
        bail!("{context_msg}");
    }
    bail!("{context_msg}: {detail}");
}

fn maybe_reload_running_proxy(paths: &DomainsPaths, engine: DomainsEngine) -> Result<()> {
    match engine {
        DomainsEngine::Docker => maybe_reload_running_proxy_docker(),
        DomainsEngine::Native => {
            if native_proxy_running(paths)? {
                // Native daemon reloads routes.json lazily on mtime change.
                return Ok(());
            }
            println!("Native proxy not running yet. Start it with: f domains --engine native up");
            Ok(())
        }
    }
}

fn maybe_reload_running_proxy_docker() -> Result<()> {
    if !docker_available() {
        return Ok(());
    }
    if !proxy_is_running()? {
        println!("Proxy not running yet. Start it with: f domains up");
        return Ok(());
    }

    let output = Command::new("docker")
        .args(["exec", PROXY_CONTAINER_NAME, "nginx", "-s", "reload"])
        .output()
        .context("Failed to reload proxy")?;
    ensure_success(output, "Failed to reload running proxy")
}

fn proxy_is_running() -> Result<bool> {
    if !docker_available() {
        return Ok(false);
    }
    let output = Command::new("docker")
        .args([
            "ps",
            "--filter",
            &format!("name=^/{}$", PROXY_CONTAINER_NAME),
            "--format",
            "{{.Names}}",
        ])
        .output()
        .context("Failed to check docker container status")?;
    if !output.status.success() {
        return Ok(false);
    }
    let names = String::from_utf8_lossy(&output.stdout);
    Ok(names
        .lines()
        .any(|line| line.trim() == PROXY_CONTAINER_NAME))
}

fn docker_container_owning_port_80() -> Result<Option<String>> {
    if !docker_available() {
        return Ok(None);
    }
    let output = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}\t{{.Ports}}"])
        .output()
        .context("Failed to inspect docker port bindings")?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let mut parts = line.splitn(2, '\t');
        let name = parts.next().unwrap_or("").trim();
        let ports = parts.next().unwrap_or("");
        if ports.contains(":80->80/tcp") {
            return Ok(Some(name.to_string()));
        }
    }
    Ok(None)
}

fn assert_no_port_80_conflict() -> Result<()> {
    if let Some(owner) = docker_container_owning_port_80()? {
        if owner != PROXY_CONTAINER_NAME {
            bail!(
                "Port 80 is already owned by docker container '{}'. Stop it first (for example: docker stop {}).",
                owner,
                owner
            );
        }
        return Ok(());
    }

    if proxy_is_running()? {
        return Ok(());
    }

    if let Some(listener) = port_80_listener_summary()? {
        bail!(
            "Port 80 is already in use by '{}'. Stop that listener, then retry `f domains up`.",
            listener
        );
    }
    Ok(())
}

fn port_80_listener_summary() -> Result<Option<String>> {
    if which::which("lsof").is_err() {
        return Ok(None);
    }
    let output = Command::new("lsof")
        .args(["-nP", "-iTCP:80", "-sTCP:LISTEN"])
        .output()
        .context("Failed to inspect port 80 listeners")?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let _header = lines.next();
    if let Some(line) = lines.next() {
        let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
        return Ok(Some(compact));
    }
    Ok(None)
}

fn run_up_native(paths: &DomainsPaths) -> Result<()> {
    ensure_layout(paths)?;
    let routes = load_routes(paths)?;
    assert_no_port_80_conflict_native(paths)?;
    ensure_native_binary(paths)?;

    if native_proxy_running(paths)? {
        println!("Native local domains proxy is already running.");
        println!("Config root: {}", paths.root.display());
        println!("Routes: {}", routes.len());
        return Ok(());
    }

    start_native_proxy(paths)?;
    println!("Native local domains proxy is up (binary: domainsd-cpp).");
    println!("Config root: {}", paths.root.display());
    println!("Routes: {}", routes.len());
    if routes.is_empty() {
        println!("No routes yet. Add one with:");
        println!("  f domains add linsa.localhost 127.0.0.1:3481");
    }
    Ok(())
}

fn run_down_native(paths: &DomainsPaths) -> Result<()> {
    ensure_layout(paths)?;
    if cfg!(target_os = "macos") {
        let launchd_plist = macos_launchd_plist_path();
        if launchd_plist.exists() {
            println!(
                "Native local domains appears launchd-managed: {}",
                launchd_plist.display()
            );
            println!(
                "To stop/uninstall launchd mode, run:\n  sudo {}",
                macos_launchd_uninstall_script_path().display()
            );
            return Ok(());
        }
    }
    let Some(pid) = read_native_pid(paths)? else {
        println!("Native local domains proxy is not running.");
        return Ok(());
    };

    if !pid_alive(pid) {
        let _ = fs::remove_file(&paths.native_pid);
        println!("Native local domains proxy was not running (removed stale pid file).");
        return Ok(());
    }

    let output = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .output()
        .context("Failed to stop native local domains proxy")?;
    if !output.status.success() {
        ensure_success(output, "Failed to stop native local domains proxy")?;
    }

    for _ in 0..40 {
        if !pid_alive(pid) {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = fs::remove_file(&paths.native_pid);
    println!("Native local domains proxy stopped.");
    Ok(())
}

fn run_doctor_native(paths: &DomainsPaths) -> Result<()> {
    ensure_layout(paths)?;
    let routes = load_routes(paths)?;
    let running = native_proxy_running(paths)?;

    println!("Local domains doctor");
    println!("--------------------");
    println!("Engine: native");
    println!("Config root: {}", paths.root.display());
    println!("Routes: {}", routes.len());
    println!(
        "Native daemon: {}",
        if running { "running" } else { "stopped" }
    );
    println!("Native binary: {}", paths.native_bin.display());
    println!("Native pid file: {}", paths.native_pid.display());
    println!("Native log file: {}", paths.native_log.display());

    if let Some(owner) = docker_container_owning_port_80()? {
        println!("Port 80 docker owner: {}", owner);
    } else if let Some(listener) = port_80_listener_summary()? {
        println!("Port 80 listener: {}", listener);
    } else {
        println!("Port 80 listener: none");
    }

    println!(
        "Native health: {}",
        if native_healthcheck().unwrap_or(false) {
            "ok"
        } else {
            "unreachable"
        }
    );

    if !routes.is_empty() {
        println!();
        println!("{:<32} {}", "HOST", "TARGET");
        println!("{}", "-".repeat(58));
        for (host, target) in routes {
            println!("{:<32} {}", host, target);
        }
    }

    Ok(())
}

fn native_source_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tools")
        .join("domainsd-cpp")
        .join("domainsd.cpp")
}

fn domainsd_tools_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tools")
        .join("domainsd-cpp")
}

fn macos_launchd_install_script_path() -> PathBuf {
    domainsd_tools_dir().join("install-macos-launchd.sh")
}

fn macos_launchd_uninstall_script_path() -> PathBuf {
    domainsd_tools_dir().join("uninstall-macos-launchd.sh")
}

fn macos_launchd_plist_path() -> PathBuf {
    PathBuf::from("/Library/LaunchDaemons").join(format!("{MACOS_DOMAINSD_LABEL}.plist"))
}

fn log_contains_permission_denied(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read native log {}", path.display()))?;
    Ok(content.contains("Permission denied"))
}

fn ensure_native_binary(paths: &DomainsPaths) -> Result<()> {
    let source = native_source_path();
    if !source.exists() {
        bail!(
            "Native domains daemon source not found at {}",
            source.display()
        );
    }

    let rebuild = if !paths.native_bin.exists() {
        true
    } else {
        let src_mtime = fs::metadata(&source)
            .and_then(|m| m.modified())
            .context("Failed to read native daemon source mtime")?;
        let bin_mtime = fs::metadata(&paths.native_bin)
            .and_then(|m| m.modified())
            .context("Failed to read native daemon binary mtime")?;
        src_mtime > bin_mtime
    };

    if !rebuild {
        return Ok(());
    }

    let compiler = if std::path::Path::new("/usr/bin/clang++").exists() {
        PathBuf::from("/usr/bin/clang++")
    } else {
        which::which("clang++")
            .context("clang++ is required for --engine native (install Xcode command line tools)")?
    };

    let output = Command::new(&compiler)
        .args([
            "-std=c++20",
            "-O3",
            "-DNDEBUG",
            "-Wall",
            "-Wextra",
            "-pthread",
            source.to_string_lossy().as_ref(),
            "-o",
            paths.native_bin.to_string_lossy().as_ref(),
        ])
        .output()
        .context("Failed to build native local domains daemon")?;
    ensure_success(output, "Failed to build native local domains daemon")?;
    Ok(())
}

fn assert_no_port_80_conflict_native(paths: &DomainsPaths) -> Result<()> {
    if native_proxy_running(paths)? {
        return Ok(());
    }

    if let Some(owner) = docker_container_owning_port_80()? {
        bail!(
            "Port 80 is owned by docker container '{}'. Stop it first before starting native domains proxy.",
            owner
        );
    }
    if let Some(listener) = port_80_listener_summary()? {
        bail!(
            "Port 80 is already in use by '{}'. Stop that listener, then retry `f domains --engine native up`.",
            listener
        );
    }
    Ok(())
}

fn start_native_proxy(paths: &DomainsPaths) -> Result<()> {
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.native_log)
        .with_context(|| format!("Failed to open log file {}", paths.native_log.display()))?;
    let err_file = log_file
        .try_clone()
        .context("Failed to duplicate native proxy log file handle")?;

    let mut cmd = Command::new(&paths.native_bin);
    cmd.arg("--listen")
        .arg("127.0.0.1:80")
        .arg("--routes")
        .arg(&paths.routes_state)
        .arg("--pidfile")
        .arg(&paths.native_pid);

    for (flag, value) in native_tuning_args()? {
        cmd.arg(flag).arg(value);
    }

    let child = cmd
        .stdout(log_file)
        .stderr(err_file)
        .spawn()
        .context("Failed to spawn native local domains daemon")?;

    let pid = child.id();
    for _ in 0..50 {
        if native_healthcheck().unwrap_or(false) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    if cfg!(target_os = "macos") && log_contains_permission_denied(&paths.native_log)? {
        bail!(
            "Native local domains proxy failed to bind port 80 (permission denied).\n\
macOS requires privileged socket ownership for :80 in native mode.\n\
Run once:\n  sudo {}\n\
Then retry: f domains --engine native up\n\
Log: {}",
            macos_launchd_install_script_path().display(),
            paths.native_log.display()
        );
    }
    bail!(
        "Native local domains proxy failed to become healthy (pid {}). Check logs: {}",
        pid,
        paths.native_log.display()
    )
}

fn native_tuning_args() -> Result<Vec<(&'static str, String)>> {
    const MAPPINGS: [(&str, &str); 8] = [
        (
            "FLOW_DOMAINS_NATIVE_MAX_ACTIVE_CLIENTS",
            "--max-active-clients",
        ),
        (
            "FLOW_DOMAINS_NATIVE_UPSTREAM_CONNECT_TIMEOUT_MS",
            "--upstream-connect-timeout-ms",
        ),
        (
            "FLOW_DOMAINS_NATIVE_UPSTREAM_IO_TIMEOUT_MS",
            "--upstream-io-timeout-ms",
        ),
        (
            "FLOW_DOMAINS_NATIVE_CLIENT_IO_TIMEOUT_MS",
            "--client-io-timeout-ms",
        ),
        (
            "FLOW_DOMAINS_NATIVE_POOL_MAX_IDLE_PER_KEY",
            "--pool-max-idle-per-key",
        ),
        (
            "FLOW_DOMAINS_NATIVE_POOL_MAX_IDLE_TOTAL",
            "--pool-max-idle-total",
        ),
        (
            "FLOW_DOMAINS_NATIVE_POOL_IDLE_TIMEOUT_MS",
            "--pool-idle-timeout-ms",
        ),
        ("FLOW_DOMAINS_NATIVE_POOL_MAX_AGE_MS", "--pool-max-age-ms"),
    ];

    let mut out = Vec::new();
    for (env_name, flag) in MAPPINGS {
        if let Ok(raw) = std::env::var(env_name) {
            let parsed = raw
                .trim()
                .parse::<u64>()
                .with_context(|| format!("Invalid {} value: {}", env_name, raw))?;
            if parsed == 0 {
                bail!("{} must be > 0", env_name);
            }
            out.push((flag, parsed.to_string()));
        }
    }
    Ok(out)
}

fn read_native_pid(paths: &DomainsPaths) -> Result<Option<u32>> {
    if !paths.native_pid.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&paths.native_pid)
        .with_context(|| format!("Failed to read {}", paths.native_pid.display()))?;
    let parsed = raw
        .trim()
        .parse::<u32>()
        .with_context(|| format!("Invalid pid in {}", paths.native_pid.display()))?;
    Ok(Some(parsed))
}

fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn native_proxy_running(paths: &DomainsPaths) -> Result<bool> {
    let Some(pid) = read_native_pid(paths)? else {
        return Ok(false);
    };
    if !pid_alive(pid) {
        return Ok(false);
    }
    Ok(native_healthcheck().unwrap_or(false))
}

fn native_healthcheck() -> Result<bool> {
    let mut stream = match TcpStream::connect("127.0.0.1:80") {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    stream.write_all(
        b"GET /_flow/domains/health HTTP/1.1\r\nHost: flow-domains-health.localhost\r\nConnection: close\r\n\r\n",
    )?;
    let mut buf = [0_u8; 2048];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(false);
    }
    let response = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
    Ok(response.contains(NATIVE_PROXY_HEADER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_route_uses_localhost_host_header_for_loopback_targets() {
        let rendered = render_route("linsa.localhost", "127.0.0.1:3481");
        assert!(rendered.contains("proxy_pass http://host.docker.internal:3481;"));
        assert!(rendered.contains("proxy_set_header Host localhost;"));
    }

    #[test]
    fn normalize_host_requires_localhost_suffix() {
        assert!(normalize_host("linsa.localhost").is_ok());
        assert!(normalize_host("linsa.dev").is_err());
    }

    #[test]
    fn normalize_target_requires_port() {
        assert!(normalize_target("127.0.0.1:3481").is_ok());
        assert!(normalize_target("127.0.0.1").is_err());
    }
}
