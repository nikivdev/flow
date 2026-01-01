//! Deploy projects to hosts and cloud platforms.
//!
//! Supports:
//! - Linux hosts via SSH (with systemd + nginx)
//! - Cloudflare Workers
//! - Railway

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::cli::{DeployAction, DeployCommand};
use crate::config::Config;
use crate::deploy_setup::{CloudflareSetupDefaults, CloudflareSetupResult, run_cloudflare_setup};
use crate::env::parse_env_file;

/// Host configuration stored globally at ~/.config/flow/deploy.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeployConfig {
    /// SSH user@host:port for linux host deployments.
    pub host: Option<HostConnection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConnection {
    pub user: String,
    pub host: String,
    pub port: u16,
}

impl HostConnection {
    /// Parse connection string like "user@host:port" or "user@host".
    pub fn parse(s: &str) -> Result<Self> {
        let (user_host, port) = if let Some((uh, p)) = s.rsplit_once(':') {
            (uh, p.parse::<u16>().unwrap_or(22))
        } else {
            (s, 22)
        };

        let (user, host) = user_host
            .split_once('@')
            .context("connection string must be user@host[:port]")?;

        Ok(Self {
            user: user.to_string(),
            host: host.to_string(),
            port,
        })
    }

    /// Format as user@host for SSH commands.
    pub fn ssh_target(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// Host deployment config from flow.toml [host] section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HostConfig {
    /// Remote destination path (e.g., /opt/myapp).
    pub dest: Option<String>,
    /// Setup script to run after syncing.
    pub setup: Option<String>,
    /// Command to run the service.
    pub run: Option<String>,
    /// Port the service listens on.
    pub port: Option<u16>,
    /// Systemd service name.
    pub service: Option<String>,
    /// Path to .env file for secrets.
    pub env_file: Option<String>,
    /// Public domain for nginx.
    pub domain: Option<String>,
    /// Enable SSL via Let's Encrypt.
    #[serde(default)]
    pub ssl: bool,
}

/// Cloudflare deployment config from flow.toml [cloudflare] section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudflareConfig {
    /// Path to worker directory (relative to project root).
    pub path: Option<String>,
    /// Path to .env file for secrets.
    pub env_file: Option<String>,
    /// Env source for secrets ("1focus" or "file").
    pub env_source: Option<String>,
    /// Specific env keys to fetch when env_source = "1focus".
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Env keys to set as non-secret vars when env_source = "1focus".
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// Wrangler environment name (e.g., staging).
    #[serde(default, alias = "env")]
    pub environment: Option<String>,
    /// Custom deploy command.
    pub deploy: Option<String>,
    /// Custom dev command.
    pub dev: Option<String>,
    /// URL for health checks (e.g., https://my-worker.workers.dev).
    pub url: Option<String>,
}

/// Railway deployment config from flow.toml [railway] section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RailwayConfig {
    /// Railway project ID.
    pub project: Option<String>,
    /// Service name.
    pub service: Option<String>,
    /// Environment (production, staging).
    pub environment: Option<String>,
    /// Start command.
    pub start: Option<String>,
    /// Path to .env file.
    pub env_file: Option<String>,
}

/// Get the deploy config file path.
fn deploy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flow")
        .join("deploy.json")
}

/// Load global deploy config.
pub fn load_deploy_config() -> Result<DeployConfig> {
    let path = deploy_config_path();
    if path.exists() {
        let content = fs::read_to_string(&path)?;
        Ok(serde_json::from_str(&content).unwrap_or_default())
    } else {
        Ok(DeployConfig::default())
    }
}

/// Save global deploy config.
pub fn save_deploy_config(config: &DeployConfig) -> Result<()> {
    let path = deploy_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(config)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Run the deploy command.
pub fn run(cmd: DeployCommand) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let config_path = project_root.join("flow.toml");
    let flow_config = if config_path.exists() {
        Some(crate::config::load(&config_path)?)
    } else {
        None
    };

    match cmd.action {
        None => {
            // Auto-detect platform from flow.toml
            auto_deploy(&project_root, flow_config.as_ref())
        }
        Some(DeployAction::Host { remote_build, setup }) => {
            deploy_host(&project_root, flow_config.as_ref(), remote_build, setup)
        }
        Some(DeployAction::Cloudflare { secrets, dev }) => {
            deploy_cloudflare(&project_root, flow_config.as_ref(), secrets, dev)
        }
        Some(DeployAction::Setup) => setup_cloudflare(&project_root, flow_config.as_ref()),
        Some(DeployAction::Railway) => deploy_railway(&project_root, flow_config.as_ref()),
        Some(DeployAction::Status) => show_status(&project_root, flow_config.as_ref()),
        Some(DeployAction::Logs { follow, lines }) => {
            show_logs(&project_root, flow_config.as_ref(), follow, lines)
        }
        Some(DeployAction::Restart) => restart_service(&project_root, flow_config.as_ref()),
        Some(DeployAction::Stop) => stop_service(&project_root, flow_config.as_ref()),
        Some(DeployAction::Shell) => open_shell(),
        Some(DeployAction::SetHost { connection }) => set_host(&connection),
        Some(DeployAction::ShowHost) => show_host(),
        Some(DeployAction::Health { url, status }) => {
            check_health(&project_root, flow_config.as_ref(), url, status)
        }
    }
}

/// Auto-detect platform and deploy.
fn auto_deploy(project_root: &Path, config: Option<&Config>) -> Result<()> {
    let config = config.context("No flow.toml found. Run 'f init' first.")?;

    // Check which platform configs exist
    if config.host.is_some() {
        println!("Detected [host] config, deploying to Linux host...");
        return deploy_host(project_root, Some(config), false, false);
    }

    if config.cloudflare.is_some() {
        println!("Detected [cloudflare] config, deploying to Cloudflare...");
        return deploy_cloudflare(project_root, Some(config), false, false);
    }

    if config.railway.is_some() {
        println!("Detected [railway] config, deploying to Railway...");
        return deploy_railway(project_root, Some(config));
    }

    bail!(
        "No deployment config found in flow.toml.\n\n\
        Add one of:\n\
        [host]\n\
        dest = \"/opt/myapp\"\n\
        run = \"./server\"\n\n\
        [cloudflare]\n\
        path = \"worker\"\n\n\
        [railway]\n\
        project = \"my-project\"\n\n\
        Or run:\n\
        f deploy setup"
    );
}

/// Deploy to a Linux host via SSH.
fn deploy_host(
    project_root: &Path,
    config: Option<&Config>,
    _remote_build: bool,
    force_setup: bool,
) -> Result<()> {
    let deploy_config = load_deploy_config()?;
    let conn = deploy_config
        .host
        .as_ref()
        .context("No host configured. Run: f deploy set-host user@host:port")?;

    let host_cfg = config
        .and_then(|c| c.host.as_ref())
        .context("No [host] section in flow.toml")?;

    let dest = host_cfg.dest.as_deref().unwrap_or("/opt/app");
    let service_name = host_cfg
        .service
        .as_deref()
        .unwrap_or_else(|| project_root.file_name().unwrap().to_str().unwrap());

    println!("Deploying to {}:{}", conn.ssh_target(), dest);

    // 1. Sync files via rsync
    println!("\n==> Syncing files...");
    rsync_upload(project_root, conn, dest)?;

    // 2. Copy env file if specified
    if let Some(env_file) = &host_cfg.env_file {
        let local_env = project_root.join(env_file);
        if local_env.exists() {
            println!("==> Copying {}...", env_file);
            let remote_env = format!("{}/.env", dest);
            scp_file(&local_env, conn, &remote_env)?;
        }
    }

    // 3. Run setup script if needed
    if let Some(setup) = &host_cfg.setup {
        if force_setup || !service_exists(conn, service_name)? {
            println!("==> Running setup...");
            ssh_run(conn, &format!("cd {} && {}", dest, setup))?;
        }
    }

    // 4. Create/update systemd service
    if let Some(run_cmd) = &host_cfg.run {
        println!("==> Configuring systemd service: {}", service_name);
        create_systemd_service(conn, service_name, dest, run_cmd, host_cfg)?;
    }

    // 5. Configure nginx if domain specified
    if let Some(domain) = &host_cfg.domain {
        if let Some(port) = host_cfg.port {
            println!("==> Configuring nginx for {}", domain);
            setup_nginx(conn, domain, port, host_cfg.ssl)?;
        }
    }

    // 6. Restart service
    println!("==> Starting service...");
    ssh_run(conn, &format!("systemctl restart {}", service_name))?;

    println!("\n✓ Deployed successfully!");
    if let Some(domain) = &host_cfg.domain {
        let scheme = if host_cfg.ssl { "https" } else { "http" };
        println!("  URL: {}://{}", scheme, domain);
    }

    Ok(())
}

/// Deploy to Cloudflare Workers.
fn deploy_cloudflare(
    project_root: &Path,
    config: Option<&Config>,
    set_secrets: bool,
    dev_mode: bool,
) -> Result<()> {
    let default_cf = CloudflareConfig::default();
    let cf_cfg = config
        .and_then(|c| c.cloudflare.as_ref())
        .unwrap_or(&default_cf);

    let worker_path = cf_cfg
        .path
        .as_ref()
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.to_path_buf());

    ensure_wrangler_config(&worker_path)?;

    let env_name = cf_cfg.environment.as_deref();

    // Set secrets if requested
    if set_secrets {
        if is_1focus_source(cf_cfg.env_source.as_deref()) {
            apply_cloudflare_env_from_config(project_root, cf_cfg)?;
        } else if let Some(env_file) = &cf_cfg.env_file {
            let env_path = project_root.join(env_file);
            if env_path.exists() {
                println!("==> Setting secrets from {}...", env_file);
                set_wrangler_secrets(&worker_path, &env_path, env_name, None)?;
            }
        }
    }

    // Deploy or dev
    let cmd = if dev_mode {
        cf_cfg.dev.as_deref().unwrap_or("wrangler dev")
    } else {
        cf_cfg.deploy.as_deref().unwrap_or("wrangler deploy")
    };
    let cmd = append_env_arg(cmd, env_name);

    println!("==> Running: {}", cmd);
    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(&worker_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("Cloudflare deployment failed");
    }

    println!("\n✓ Deployed to Cloudflare!");
    Ok(())
}

pub fn apply_cloudflare_env(project_root: &Path, config: Option<&Config>) -> Result<()> {
    let cf_cfg = config
        .and_then(|c| c.cloudflare.as_ref())
        .context("No [cloudflare] section in flow.toml")?;
    apply_cloudflare_env_from_config(project_root, cf_cfg)
}

fn apply_cloudflare_env_from_config(project_root: &Path, cf_cfg: &CloudflareConfig) -> Result<()> {
    if !is_1focus_source(cf_cfg.env_source.as_deref()) {
        bail!("cloudflare.env_source must be set to \"1focus\" to apply envs");
    }

    let worker_path = cf_cfg
        .path
        .as_ref()
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.to_path_buf());
    ensure_wrangler_config(&worker_path)?;

    let wrangler_env = cf_cfg.environment.as_deref();
    let onefocus_env = wrangler_env.unwrap_or("production");

    let mut keys = cf_cfg.env_keys.clone();
    if !keys.is_empty() {
        for key in &cf_cfg.env_vars {
            if !keys.iter().any(|existing| existing == key) {
                keys.push(key.clone());
            }
        }
    }

    let vars = crate::env::fetch_project_env_vars(onefocus_env, &keys)?;
    if vars.is_empty() {
        bail!("No env vars found in 1focus for environment '{}'", onefocus_env);
    }

    let var_keys: HashSet<String> = cf_cfg.env_vars.iter().cloned().collect();
    println!("==> Applying {} env var(s) from 1focus...", vars.len());
    set_wrangler_env_map(&worker_path, wrangler_env, &vars, &var_keys)?;
    Ok(())
}

fn ensure_wrangler_config(worker_path: &Path) -> Result<()> {
    let has_wrangler = worker_path.join("wrangler.toml").exists()
        || worker_path.join("wrangler.jsonc").exists()
        || worker_path.join("wrangler.json").exists();

    if !has_wrangler {
        bail!(
            "No wrangler config found in {}.\n\
            Create a wrangler.toml or run: npx wrangler init",
            worker_path.display()
        );
    }

    Ok(())
}

fn is_1focus_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("1focus") | Some("1f") | Some("onefocus")
    )
}

fn set_wrangler_env_map(
    worker_path: &Path,
    env_name: Option<&str>,
    vars: &HashMap<String, String>,
    var_keys: &HashSet<String>,
) -> Result<()> {
    for (key, value) in vars {
        if var_keys.contains(key) {
            println!("  Setting var {}...", key);
            set_wrangler_var_value(worker_path, env_name, key, value)?;
        } else {
            println!("  Setting secret {}...", key);
            set_wrangler_secret_value(worker_path, env_name, key, value)?;
        }
    }
    Ok(())
}

fn set_wrangler_var_value(
    worker_path: &Path,
    env_name: Option<&str>,
    key: &str,
    value: &str,
) -> Result<()> {
    let mut cmd = Command::new("wrangler");
    cmd.args(["vars", "set", key, value]);
    if let Some(env) = env_name {
        cmd.args(["--env", env]);
    }
    let status = cmd
        .current_dir(worker_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("Failed to set wrangler var {}", key);
    }
    Ok(())
}

fn set_wrangler_secret_value(
    worker_path: &Path,
    env_name: Option<&str>,
    key: &str,
    value: &str,
) -> Result<()> {
    let mut cmd = Command::new("wrangler");
    cmd.args(["secret", "put", key]);
    if let Some(env) = env_name {
        cmd.args(["--env", env]);
    }
    let mut child = cmd
        .current_dir(worker_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{}", value)?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("Failed to set wrangler secret {}", key);
    }
    Ok(())
}

fn setup_cloudflare(project_root: &Path, config: Option<&Config>) -> Result<()> {
    let default_cf = CloudflareConfig::default();
    let cf_cfg = config
        .and_then(|c| c.cloudflare.as_ref())
        .unwrap_or(&default_cf);

    let defaults = CloudflareSetupDefaults {
        worker_path: cf_cfg
            .path
            .as_ref()
            .map(|p| project_root.join(p)),
        env_file: cf_cfg
            .env_file
            .as_ref()
            .map(|p| project_root.join(p)),
        environment: cf_cfg.environment.clone(),
    };

    let result = run_cloudflare_setup(project_root, defaults)?;
    let Some(result) = result else {
        return Ok(());
    };

    let flow_path = project_root.join("flow.toml");
    if !flow_path.exists() {
        bail!("flow.toml not found. Run `f init` first.");
    }

    update_flow_toml_cloudflare(&flow_path, project_root, &result)?;

    if result.apply_secrets {
        if let Some(env_file) = result.env_file.as_ref() {
            let env_name = result.environment.as_deref();
            set_wrangler_secrets(
                &result.worker_path,
                env_file,
                env_name,
                Some(&result.selected_keys),
            )?;
        }
    }

    println!("\n✓ Cloudflare deploy setup complete.");
    Ok(())
}

/// Deploy to Railway.
fn deploy_railway(project_root: &Path, config: Option<&Config>) -> Result<()> {
    let default_rail = RailwayConfig::default();
    let rail_cfg = config
        .and_then(|c| c.railway.as_ref())
        .unwrap_or(&default_rail);

    // Check railway CLI
    if which::which("railway").is_err() {
        bail!("Railway CLI not found. Install: npm install -g @railway/cli");
    }

    // Link project if specified
    if let (Some(project), Some(env)) = (&rail_cfg.project, &rail_cfg.environment) {
        println!("==> Linking to Railway project...");
        let status = Command::new("railway")
            .args(["link", project, "--environment", env])
            .current_dir(project_root)
            .status()?;
        if !status.success() {
            bail!("Failed to link Railway project");
        }
    }

    // Set env vars from file
    if let Some(env_file) = &rail_cfg.env_file {
        let env_path = project_root.join(env_file);
        if env_path.exists() {
            println!("==> Setting environment variables...");
            set_railway_env(&env_path)?;
        }
    }

    // Deploy
    println!("==> Deploying to Railway...");
    let status = Command::new("railway")
        .args(["up", "--detach"])
        .current_dir(project_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("Railway deployment failed");
    }

    println!("\n✓ Deployed to Railway!");
    Ok(())
}

/// Show deployment status.
fn show_status(_project_root: &Path, config: Option<&Config>) -> Result<()> {
    let deploy_config = load_deploy_config()?;

    println!("Deployment Status\n");

    // Host status
    if let Some(conn) = &deploy_config.host {
        println!("Host: {}@{}:{}", conn.user, conn.host, conn.port);
        if let Some(cfg) = config.and_then(|c| c.host.as_ref()) {
            if let Some(service) = &cfg.service {
                let output = ssh_capture(conn, &format!("systemctl is-active {} 2>/dev/null || echo inactive", service))?;
                println!("  Service '{}': {}", service, output.trim());
            }
        }
    } else {
        println!("Host: not configured");
    }

    Ok(())
}

/// Show deployment logs.
fn show_logs(_project_root: &Path, config: Option<&Config>, follow: bool, lines: usize) -> Result<()> {
    let deploy_config = load_deploy_config()?;
    let conn = deploy_config
        .host
        .as_ref()
        .context("No host configured")?;

    let service = config
        .and_then(|c| c.host.as_ref())
        .and_then(|h| h.service.as_ref())
        .context("No service name in [host] config")?;

    let follow_flag = if follow { "-f" } else { "" };
    let cmd = format!(
        "journalctl -u {} -n {} {} --no-pager",
        service, lines, follow_flag
    );

    ssh_run(conn, &cmd)?;
    Ok(())
}

/// Restart the deployed service.
fn restart_service(_project_root: &Path, config: Option<&Config>) -> Result<()> {
    let deploy_config = load_deploy_config()?;
    let conn = deploy_config.host.as_ref().context("No host configured")?;
    let service = config
        .and_then(|c| c.host.as_ref())
        .and_then(|h| h.service.as_ref())
        .context("No service name")?;

    println!("Restarting {}...", service);
    ssh_run(conn, &format!("systemctl restart {}", service))?;
    println!("✓ Restarted");
    Ok(())
}

/// Stop the deployed service.
fn stop_service(_project_root: &Path, config: Option<&Config>) -> Result<()> {
    let deploy_config = load_deploy_config()?;
    let conn = deploy_config.host.as_ref().context("No host configured")?;
    let service = config
        .and_then(|c| c.host.as_ref())
        .and_then(|h| h.service.as_ref())
        .context("No service name")?;

    println!("Stopping {}...", service);
    ssh_run(conn, &format!("systemctl stop {}", service))?;
    println!("✓ Stopped");
    Ok(())
}

/// Open SSH shell to host.
fn open_shell() -> Result<()> {
    let deploy_config = load_deploy_config()?;
    let conn = deploy_config.host.as_ref().context("No host configured")?;

    println!("Connecting to {}...", conn.ssh_target());
    let status = Command::new("ssh")
        .args(["-p", &conn.port.to_string(), &conn.ssh_target()])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("SSH connection failed");
    }
    Ok(())
}

/// Set the host connection.
fn set_host(connection: &str) -> Result<()> {
    let conn = HostConnection::parse(connection)?;
    let mut config = load_deploy_config()?;
    config.host = Some(conn.clone());
    save_deploy_config(&config)?;

    println!("✓ Host set: {}@{}:{}", conn.user, conn.host, conn.port);
    println!("\nTest connection: f deploy shell");
    Ok(())
}

/// Show current host.
fn show_host() -> Result<()> {
    let config = load_deploy_config()?;
    if let Some(conn) = &config.host {
        println!("Host: {}@{}:{}", conn.user, conn.host, conn.port);
    } else {
        println!("No host configured.");
        println!("Set one with: f deploy set-host user@host:port");
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────
// SSH/rsync helpers
// ─────────────────────────────────────────────────────────────

/// Run SSH command with inherited stdio.
fn ssh_run(conn: &HostConnection, cmd: &str) -> Result<()> {
    let status = Command::new("ssh")
        .args([
            "-p",
            &conn.port.to_string(),
            "-o",
            "StrictHostKeyChecking=accept-new",
            &conn.ssh_target(),
            cmd,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run SSH")?;

    if !status.success() {
        bail!("SSH command failed: {}", cmd);
    }
    Ok(())
}

/// Run SSH command and capture output.
fn ssh_capture(conn: &HostConnection, cmd: &str) -> Result<String> {
    let output = Command::new("ssh")
        .args([
            "-p",
            &conn.port.to_string(),
            "-o",
            "StrictHostKeyChecking=accept-new",
            &conn.ssh_target(),
            cmd,
        ])
        .output()
        .context("Failed to run SSH")?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Sync directory via rsync.
fn rsync_upload(local: &Path, conn: &HostConnection, remote_dest: &str) -> Result<()> {
    let remote = format!("{}:{}", conn.ssh_target(), remote_dest);
    let ssh_cmd = format!("ssh -p {}", conn.port);

    // Create remote directory first
    ssh_run(conn, &format!("mkdir -p {}", remote_dest))?;

    let status = Command::new("rsync")
        .args([
            "-avz",
            "--delete",
            "--exclude=target/",
            "--exclude=.git/",
            "--exclude=node_modules/",
            "--exclude=.env",
            "--exclude=*.log",
            "-e",
            &ssh_cmd,
            &format!("{}/", local.display()),
            &remote,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run rsync")?;

    if !status.success() {
        bail!("rsync failed");
    }
    Ok(())
}

/// Copy file via scp.
fn scp_file(local: &Path, conn: &HostConnection, remote: &str) -> Result<()> {
    let dest = format!("{}:{}", conn.ssh_target(), remote);
    let status = Command::new("scp")
        .args([
            "-P",
            &conn.port.to_string(),
            &local.display().to_string(),
            &dest,
        ])
        .status()
        .context("Failed to run scp")?;

    if !status.success() {
        bail!("scp failed");
    }
    Ok(())
}

/// Check if systemd service exists.
fn service_exists(conn: &HostConnection, name: &str) -> Result<bool> {
    let output = ssh_capture(conn, &format!("systemctl list-unit-files {} 2>/dev/null | grep -c {} || true", name, name))?;
    Ok(output.trim() != "0")
}

/// Create systemd service file.
fn create_systemd_service(
    conn: &HostConnection,
    name: &str,
    workdir: &str,
    exec_start: &str,
    config: &HostConfig,
) -> Result<()> {
    let env_file_line = if config.env_file.is_some() {
        format!("EnvironmentFile={}/.env", workdir)
    } else {
        String::new()
    };

    let service = format!(
        r#"[Unit]
Description={name}
After=network.target

[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart={exec_start}
Restart=always
RestartSec=5
{env_file_line}

[Install]
WantedBy=multi-user.target
"#
    );

    let escaped = service.replace('\"', "\\\"").replace('$', "\\$");
    let cmd = format!(
        "echo \"{}\" > /etc/systemd/system/{}.service && systemctl daemon-reload && systemctl enable {}",
        escaped, name, name
    );

    ssh_run(conn, &cmd)?;
    Ok(())
}

/// Set up nginx reverse proxy.
fn setup_nginx(conn: &HostConnection, domain: &str, port: u16, ssl: bool) -> Result<()> {
    let config = format!(
        r#"server {{
    listen 80;
    server_name {domain};

    location / {{
        proxy_pass http://127.0.0.1:{port};
        proxy_http_version 1.1;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection 'upgrade';
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_cache_bypass $http_upgrade;
    }}
}}
"#
    );

    let escaped = config.replace('\"', "\\\"").replace('$', "\\$");
    let cmd = format!(
        "echo \"{}\" > /etc/nginx/sites-available/{} && \
        ln -sf /etc/nginx/sites-available/{} /etc/nginx/sites-enabled/ && \
        nginx -t && systemctl reload nginx",
        escaped, domain, domain
    );

    ssh_run(conn, &cmd)?;

    // Set up SSL if requested
    if ssl {
        println!("==> Setting up SSL certificate...");
        let ssl_cmd = format!(
            "certbot --nginx -d {} --non-interactive --agree-tos -m admin@{} || true",
            domain, domain
        );
        ssh_run(conn, &ssl_cmd)?;
    }

    Ok(())
}

/// Set Cloudflare Worker secrets from env file.
fn set_wrangler_secrets(
    worker_path: &Path,
    env_file: &Path,
    env_name: Option<&str>,
    selected_keys: Option<&[String]>,
) -> Result<()> {
    let content = fs::read_to_string(env_file)?;
    let vars = parse_env_file(&content);
    let allowlist = selected_keys
        .map(|keys| keys.iter().cloned().collect::<HashSet<String>>());

    for (key, value) in vars {
        if let Some(allowlist) = &allowlist {
            if !allowlist.contains(&key) {
                continue;
            }
        }
        println!("  Setting {}...", key);
        set_wrangler_secret_value(worker_path, env_name, &key, &value)?;
    }
    Ok(())
}

fn append_env_arg(cmd: &str, env_name: Option<&str>) -> String {
    if let Some(env) = env_name {
        if cmd.contains("--env") {
            cmd.to_string()
        } else {
            format!("{cmd} --env {env}")
        }
    } else {
        cmd.to_string()
    }
}

fn update_flow_toml_cloudflare(
    flow_path: &Path,
    project_root: &Path,
    setup: &CloudflareSetupResult,
) -> Result<()> {
    let contents = fs::read_to_string(flow_path)?;
    let mut lines: Vec<String> = contents.lines().map(|line| line.to_string()).collect();
    let had_trailing_newline = contents.ends_with('\n');

    let worker_path = relative_dir(project_root, &setup.worker_path);
    let env_file = setup
        .env_file
        .as_ref()
        .map(|path| relative_path(project_root, path));
    let environment = setup.environment.clone();

    if let Some(start) = lines.iter().position(|line| line.trim() == "[cloudflare]") {
        let end = find_section_end(&lines, start + 1);
        let mut section_lines = Vec::new();
        for line in &lines[start + 1..end] {
            if !is_cloudflare_key_line(line) {
                section_lines.push(line.clone());
            }
        }

        let mut updates = Vec::new();
        if let Some(path) = worker_path {
            updates.push(format!("path = \"{}\"", path));
        }
        if let Some(env_file) = env_file {
            updates.push(format!("env_file = \"{}\"", env_file));
        }
        if let Some(environment) = environment {
            updates.push(format!("environment = \"{}\"", environment));
        }

        if !updates.is_empty() {
            let needs_blank = section_lines
                .last()
                .map(|line| !line.trim().is_empty())
                .unwrap_or(false);
            if needs_blank {
                section_lines.push(String::new());
            }
            section_lines.extend(updates);
        }

        let mut updated = Vec::new();
        updated.extend_from_slice(&lines[..start + 1]);
        updated.extend(section_lines);
        updated.extend_from_slice(&lines[end..]);
        lines = updated;
    } else {
        if !lines.is_empty() && !lines.last().map(|line| line.trim().is_empty()).unwrap_or(false) {
            lines.push(String::new());
        }
        lines.push("[cloudflare]".to_string());
        if let Some(path) = worker_path {
            lines.push(format!("path = \"{}\"", path));
        }
        if let Some(env_file) = env_file {
            lines.push(format!("env_file = \"{}\"", env_file));
        }
        if let Some(environment) = environment {
            lines.push(format!("environment = \"{}\"", environment));
        }
    }

    let mut updated = lines.join("\n");
    if had_trailing_newline {
        updated.push('\n');
    }
    fs::write(flow_path, updated)?;
    Ok(())
}

fn find_section_end(lines: &[String], start: usize) -> usize {
    for (idx, line) in lines.iter().enumerate().skip(start) {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return idx;
        }
    }
    lines.len()
}

fn is_cloudflare_key_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with('#') || trimmed.starts_with(';') {
        return false;
    }
    let Some((key, _)) = trimmed.split_once('=') else {
        return false;
    };
    matches!(key.trim(), "path" | "env_file" | "environment" | "env")
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn relative_dir(project_root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    if rel.as_os_str().is_empty() || rel == Path::new(".") {
        None
    } else {
        Some(rel.to_string_lossy().to_string())
    }
}

/// Set Railway environment variables from env file.
fn set_railway_env(env_file: &Path) -> Result<()> {
    let content = fs::read_to_string(env_file)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim_matches('"').trim_matches('\'');
            Command::new("railway")
                .args(["variables", "set", &format!("{}={}", key, value)])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?;
        }
    }
    Ok(())
}

/// Check if deployment is healthy via HTTP.
fn check_health(
    _project_root: &Path,
    config: Option<&Config>,
    custom_url: Option<String>,
    expected_status: u16,
) -> Result<()> {
    use std::time::Instant;

    // Determine URL to check
    let url = if let Some(url) = custom_url {
        url
    } else if let Some(config) = config {
        // Try host domain first
        if let Some(host) = &config.host {
            if let Some(domain) = &host.domain {
                let scheme = if host.ssl { "https" } else { "http" };
                format!("{}://{}", scheme, domain)
            } else {
                bail!("No domain configured. Use --url to specify a URL to check.");
            }
        } else if let Some(cf) = &config.cloudflare {
            // Use configured URL if present
            if let Some(cf_url) = &cf.url {
                cf_url.clone()
            } else {
                bail!("No URL configured in [cloudflare]. Add 'url = \"https://...\"' or use --url.");
            }
        } else {
            bail!("No deployment config found. Use --url to specify a URL to check.");
        }
    } else {
        bail!("No flow.toml found. Use --url to specify a URL to check.");
    };

    println!("Checking health: {}", url);
    let start = Instant::now();

    // Use curl for simplicity (available everywhere)
    let output = Command::new("curl")
        .args([
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "10",
            &url,
        ])
        .output()
        .context("Failed to run curl")?;

    let elapsed = start.elapsed();
    let status_str = String::from_utf8_lossy(&output.stdout);
    let actual_status: u16 = status_str.trim().parse().unwrap_or(0);

    if actual_status == expected_status {
        println!(
            "✓ Healthy (HTTP {} in {:.2}s)",
            actual_status,
            elapsed.as_secs_f64()
        );
        Ok(())
    } else if actual_status == 0 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("✗ Unreachable: {}", stderr.trim());
    } else {
        bail!(
            "✗ Unhealthy: expected HTTP {}, got {} ({:.2}s)",
            expected_status,
            actual_status,
            elapsed.as_secs_f64()
        );
    }
}
