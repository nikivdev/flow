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

use crate::cli::{DeployAction, DeployCommand, EnvAction, TaskRunOpts};
use crate::release;
use crate::config::Config;
use crate::deploy_setup::{
    CloudflareSetupDefaults,
    CloudflareSetupResult,
    discover_wrangler_configs,
    run_cloudflare_setup,
};
use crate::env::parse_env_file;
use crate::tasks;

const DEPLOY_HELPER_BIN: &str = "infra";
const DEPLOY_HELPER_REPO_DEFAULT: &str = "~/infra";
const DEPLOY_HELPER_ENV_BIN: &str = "FLOW_DEPLOY_HELPER_BIN";
const DEPLOY_HELPER_ENV_REPO: &str = "FLOW_DEPLOY_HELPER_REPO";

#[derive(Debug, Deserialize)]
struct InfraConfig {
    linux_host: Option<String>,
    linux_port: Option<String>,
    linux_user: Option<String>,
}

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
    /// Path to .env file for secrets (used when env_source is not set).
    pub env_file: Option<String>,
    /// Env source for secrets ("1focus" or "file").
    pub env_source: Option<String>,
    /// Specific env keys to fetch when env_source = "1focus".
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Fetch from project-scoped env vars instead of personal (default).
    #[serde(default)]
    pub env_project: bool,
    /// Environment name for 1focus (defaults to "production").
    pub environment: Option<String>,
    /// Service token for fetching env vars on host (set via f env token create).
    pub service_token: Option<String>,
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
    /// Default values for env vars (key/value).
    #[serde(default)]
    pub env_defaults: HashMap<String, String>,
    /// Secret keys to bootstrap directly in Cloudflare.
    #[serde(default)]
    pub bootstrap_secrets: Vec<String>,
    /// Optional Jazz sync peer for bootstrap (env store).
    pub bootstrap_jazz_peer: Option<String>,
    /// Optional Jazz worker account name for bootstrap (env store).
    pub bootstrap_jazz_name: Option<String>,
    /// Optional Jazz sync peer for bootstrap (auth store).
    pub bootstrap_jazz_auth_peer: Option<String>,
    /// Optional Jazz worker account name for bootstrap (auth store).
    pub bootstrap_jazz_auth_name: Option<String>,
    /// Env apply mode: "always", "auto", or "never".
    pub env_apply: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvApplyMode {
    Always,
    Auto,
    Never,
}

fn env_apply_mode_from_str(value: Option<&str>) -> EnvApplyMode {
    match value.map(|s| s.to_ascii_lowercase()) {
        Some(ref v) if v == "always" => EnvApplyMode::Always,
        Some(ref v) if v == "auto" => EnvApplyMode::Auto,
        Some(ref v) if v == "never" => EnvApplyMode::Never,
        _ => EnvApplyMode::Never,
    }
}

fn is_tls_connect_error(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    msg.contains("certificate was not trusted")
        || msg.contains("client error (Connect)")
        || msg.contains("failed to connect to 1focus")
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
            // Auto-detect platform from flow.toml, or run deploy task if configured.
            if let Some(cfg) = flow_config.as_ref() {
                if let Some(task_name) = cfg.flow.deploy_task.as_deref() {
                    if tasks::find_task(cfg, task_name).is_some() {
                        return tasks::run(TaskRunOpts {
                            config: config_path,
                            delegate_to_hub: false,
                            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
                            hub_port: 9050,
                            name: task_name.to_string(),
                            args: Vec::new(),
                        });
                    }
                    bail!(
                        "deploy_task '{}' not found. Available tasks: {}",
                        task_name,
                        available_tasks(cfg)
                    );
                }

                if cfg.host.is_some() || cfg.cloudflare.is_some() || cfg.railway.is_some() {
                    return auto_deploy(&project_root, Some(cfg));
                }
                if tasks::find_task(cfg, "deploy").is_some() {
                    return tasks::run(TaskRunOpts {
                        config: config_path,
                        delegate_to_hub: false,
                        hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
                        hub_port: 9050,
                        name: "deploy".to_string(),
                        args: Vec::new(),
                    });
                }
                bail!(
                    "No deployment config found in flow.toml and no 'deploy' task is defined.\n\n\
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

            bail!("No flow.toml found. Run `f setup` first.")
        }
        Some(DeployAction::Host { remote_build, setup }) => {
            deploy_host(&project_root, flow_config.as_ref(), remote_build, setup)
        }
        Some(DeployAction::Cloudflare { secrets, dev }) => {
            deploy_cloudflare(&project_root, flow_config.as_ref(), secrets, dev)
        }
        Some(DeployAction::Setup) => setup_cloudflare(&project_root, flow_config.as_ref()),
        Some(DeployAction::Railway) => deploy_railway(&project_root, flow_config.as_ref()),
        Some(DeployAction::Config) => configure_deploy(),
        Some(DeployAction::Release(opts)) => release::run(opts),
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

fn configure_deploy() -> Result<()> {
    println!("Deploy config (Linux host via SSH).");

    let _ = ensure_deploy_helper();
    let existing = load_deploy_config()?.host;
    let infra_default = infra_linux_connection_string();

    if let Some(conn) = existing.as_ref() {
        println!(
            "Current host: {}@{}:{}",
            conn.user, conn.host, conn.port
        );
    }
    if let Some(default_conn) = infra_default.as_ref() {
        if existing.is_none() {
            println!("Detected infra host: {default_conn}");
        }
    }

    let default_conn = existing
        .as_ref()
        .map(|conn| format!("{}@{}:{}", conn.user, conn.host, conn.port))
        .or(infra_default);

    let prompt = "SSH host (user@host:port)";
    let input = prompt_line(prompt, default_conn.as_deref())?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        println!("No changes.");
        return Ok(());
    }

    let conn = HostConnection::parse(trimmed)?;
    let mut cfg = load_deploy_config()?;
    cfg.host = Some(conn.clone());
    save_deploy_config(&cfg)?;

    println!("✓ Host set: {}@{}:{}", conn.user, conn.host, conn.port);
    println!("Next: run `f setup release` to scaffold host config, then `f deploy`.");
    Ok(())
}

pub fn ensure_deploy_helper() -> Result<Option<PathBuf>> {
    if let Ok(bin_override) = std::env::var(DEPLOY_HELPER_ENV_BIN) {
        let path = crate::config::expand_path(&bin_override);
        if path.exists() {
            return Ok(Some(path));
        }
    }

    if let Ok(path) = which::which(DEPLOY_HELPER_BIN) {
        return Ok(Some(path));
    }

    let repo = deploy_helper_repo();
    if !repo.exists() {
        println!(
            "Deploy helper not found. Set {} or install it to continue.",
            DEPLOY_HELPER_ENV_BIN
        );
        return Ok(None);
    }

    println!("Installing deploy helper...");
    let status = Command::new("cargo")
        .args(["build", "--release"])
        .current_dir(&repo)
        .status()
        .context("failed to build deploy helper")?;

    if !status.success() {
        bail!("deploy helper build failed");
    }

    let bin_path = repo.join("target/release").join(DEPLOY_HELPER_BIN);
    let install_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local/bin");
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("failed to create {}", install_dir.display()))?;
    let install_path = install_dir.join(DEPLOY_HELPER_BIN);
    fs::copy(&bin_path, &install_path)
        .with_context(|| format!("failed to copy {}", install_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&install_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&install_path, perms)?;
    }

    println!("Deploy helper installed.");
    Ok(Some(install_path))
}

fn deploy_helper_repo() -> PathBuf {
    if let Ok(repo_override) = std::env::var(DEPLOY_HELPER_ENV_REPO) {
        return crate::config::expand_path(&repo_override);
    }
    crate::config::expand_path(DEPLOY_HELPER_REPO_DEFAULT)
}

fn infra_linux_connection_string() -> Option<String> {
    let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    let paths = [
        base.join("infra").join("config.json"),
        crate::config::global_config_dir().join("infra/config.json"),
    ];

    for path in paths {
        if !path.exists() {
            continue;
        }
        let content = fs::read_to_string(&path).ok()?;
        let cfg: InfraConfig = serde_json::from_str(&content).ok()?;
        let user = cfg.linux_user?;
        let host = cfg.linux_host?;
        let port = cfg.linux_port.unwrap_or_else(|| "22".to_string());
        return Some(format!("{}@{}:{}", user, host, port));
    }

    None
}

pub fn default_linux_connection_string() -> Option<String> {
    infra_linux_connection_string()
}

fn prompt_line(message: &str, default: Option<&str>) -> Result<String> {
    if let Some(default) = default {
        print!("{message} [{default}]: ");
    } else {
        print!("{message}: ");
    }
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(trimmed.to_string())
}

fn available_tasks(cfg: &crate::config::Config) -> String {
    let mut names: Vec<_> = cfg.tasks.iter().map(|task| task.name.clone()).collect();
    names.sort();
    names.join(", ")
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

    // 2. Handle env vars
    let use_1focus = is_1focus_source(host_cfg.env_source.as_deref());
    let has_service_token = host_cfg.service_token.is_some();

    if use_1focus && has_service_token {
        // Service token mode: install fetch script, host fetches env vars on startup
        let service_token = host_cfg.service_token.as_ref().unwrap();
        let env_name = host_cfg.environment.as_deref().unwrap_or("production");
        let project_name = project_root.file_name().unwrap().to_str().unwrap();

        println!("==> Installing env-fetch script (host will fetch on startup)...");
        install_env_fetch_script(conn, dest, service_token, project_name, env_name, &host_cfg.env_keys)?;
    } else if use_1focus {
        // Deploy-time fetch mode: fetch now and copy to host
        let env_name = host_cfg.environment.as_deref().unwrap_or("production");
        let keys = &host_cfg.env_keys;
        let use_project = host_cfg.env_project;

        if !keys.is_empty() {
            let source = if use_project { &format!("project/{}", env_name) } else { "personal" };
            println!("==> Fetching env vars from 1focus ({})...", source);

            let result = if use_project {
                crate::env::fetch_project_env_vars(env_name, keys)
            } else {
                crate::env::fetch_personal_env_vars(keys)
            };

            match result {
                Ok(vars) if !vars.is_empty() => {
                    // Generate .env content
                    let mut content = String::new();
                    content.push_str(&format!("# Source: 1focus {} (fetched at deploy)\n", source));
                    let mut sorted_keys: Vec<_> = vars.keys().collect();
                    sorted_keys.sort();
                    for key in sorted_keys {
                        let value = &vars[key];
                        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                        content.push_str(&format!("{}=\"{}\"\n", key, escaped));
                    }

                    // Write to temp file and scp
                    let temp_env = std::env::temp_dir().join(format!(".env.{}", std::process::id()));
                    fs::write(&temp_env, &content)?;
                    let remote_env = format!("{}/.env", dest);
                    println!("==> Copying {} env vars to remote...", vars.len());
                    scp_file(&temp_env, conn, &remote_env)?;
                    let _ = fs::remove_file(&temp_env);
                }
                Ok(_) => {
                    eprintln!("⚠ No env vars found in 1focus for {}", source);
                }
                Err(err) => {
                    eprintln!("⚠ Failed to fetch env vars from 1focus: {}", err);
                }
            }
        }
    } else if let Some(env_file) = &host_cfg.env_file {
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

    let env_apply_mode = if set_secrets {
        EnvApplyMode::Always
    } else {
        env_apply_mode_from_str(cf_cfg.env_apply.as_deref())
    };
    let should_apply = matches!(env_apply_mode, EnvApplyMode::Always | EnvApplyMode::Auto);
    let use_1focus = is_1focus_source(cf_cfg.env_source.as_deref());

    let onefocus_env = env_name.unwrap_or("production");
    let mut onefocus_vars: HashMap<String, String> = HashMap::new();
    let mut onefocus_loaded = false;

    if use_1focus {
        let keys = collect_cloudflare_env_keys(cf_cfg);
        if !cf_cfg.env_defaults.is_empty() {
            for key in &keys {
                if let Some(value) = cf_cfg.env_defaults.get(key) {
                    if !value.trim().is_empty() {
                        onefocus_vars.insert(key.clone(), value.clone());
                    }
                }
            }
        }

        if !keys.is_empty() {
            match crate::env::fetch_project_env_vars(onefocus_env, &keys) {
                Ok(vars) => {
                    if !vars.is_empty() {
                        onefocus_loaded = true;
                    }
                    onefocus_vars.extend(vars);
                }
                Err(err) => {
                    if env_apply_mode == EnvApplyMode::Auto {
                        if is_tls_connect_error(&err) {
                            eprintln!(
                                "⚠ Unable to reach 1focus (TLS/connect). Skipping env sync for now."
                            );
                        } else {
                            eprintln!("⚠ Env sync skipped: {err}");
                        }
                    } else if env_apply_mode == EnvApplyMode::Always {
                        eprintln!("⚠ Env sync skipped: {err}");
                    } else {
                        eprintln!("⚠ Env sync skipped: {err}");
                    }
                }
            }
        }
    }

    if should_apply {
        if use_1focus {
            if onefocus_loaded {
                apply_cloudflare_env_map(project_root, cf_cfg, &onefocus_vars)?;
            } else if env_apply_mode == EnvApplyMode::Always {
                eprintln!(
                    "⚠ No env vars found in 1focus for environment '{}' (using defaults only).",
                    onefocus_env
                );
            }
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
    let mut deploy_cmd = Command::new("sh");
    deploy_cmd
        .arg("-c")
        .arg(cmd)
        .current_dir(&worker_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if use_1focus && !onefocus_vars.is_empty() {
        deploy_cmd.envs(&onefocus_vars);
    }

    let status = deploy_cmd.status()?;

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

pub fn set_cloudflare_secrets(
    project_root: &Path,
    config: Option<&Config>,
    secrets: &HashMap<String, String>,
) -> Result<()> {
    let cf_cfg = config
        .and_then(|c| c.cloudflare.as_ref())
        .context("No [cloudflare] section in flow.toml")?;
    let worker_path = cf_cfg
        .path
        .as_ref()
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.to_path_buf());
    ensure_wrangler_config(&worker_path)?;

    let env_name = cf_cfg.environment.as_deref();
    let mut keys: Vec<_> = secrets.keys().cloned().collect();
    keys.sort();
    for key in keys {
        if let Some(value) = secrets.get(&key) {
            println!("  Setting secret {}...", key);
            set_wrangler_secret_value(&worker_path, env_name, &key, value)?;
        }
    }

    Ok(())
}

fn apply_cloudflare_env_from_config(project_root: &Path, cf_cfg: &CloudflareConfig) -> Result<()> {
    if !is_1focus_source(cf_cfg.env_source.as_deref()) {
        bail!("cloudflare.env_source must be set to \"1focus\" to apply envs");
    }

    let onefocus_env = cf_cfg
        .environment
        .as_deref()
        .unwrap_or("production");
    let keys = collect_cloudflare_env_keys(cf_cfg);
    let vars = crate::env::fetch_project_env_vars(onefocus_env, &keys)?;
    if vars.is_empty() {
        bail!("No env vars found in 1focus for environment '{}'", onefocus_env);
    }

    apply_cloudflare_env_map(project_root, cf_cfg, &vars)?;
    Ok(())
}

fn collect_cloudflare_env_keys(cf_cfg: &CloudflareConfig) -> Vec<String> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for key in cf_cfg.env_keys.iter().chain(cf_cfg.env_vars.iter()) {
        if seen.insert(key.clone()) {
            keys.push(key.clone());
        }
    }
    keys
}

fn apply_cloudflare_env_map(
    project_root: &Path,
    cf_cfg: &CloudflareConfig,
    vars: &HashMap<String, String>,
) -> Result<()> {
    let worker_path = cf_cfg
        .path
        .as_ref()
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.to_path_buf());
    ensure_wrangler_config(&worker_path)?;

    let wrangler_env = cf_cfg.environment.as_deref();
    let var_keys: HashSet<String> = cf_cfg.env_vars.iter().cloned().collect();
    println!("==> Applying {} env var(s) from 1focus...", vars.len());
    set_wrangler_env_map(&worker_path, wrangler_env, vars, &var_keys)?;
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

fn wrangler_command(worker_path: &Path) -> Command {
    let local_bin = worker_path.join("node_modules").join(".bin").join("wrangler");
    let mut cmd = if local_bin.exists() {
        Command::new(local_bin)
    } else if worker_path.join("package.json").exists() {
        let mut cmd = Command::new("pnpm");
        cmd.args(["exec", "wrangler"]);
        cmd
    } else {
        Command::new("wrangler")
    };
    cmd.current_dir(worker_path);
    cmd
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
    let mut cmd = wrangler_command(worker_path);
    cmd.args(["vars", "set", key, value]);
    if let Some(env) = env_name {
        cmd.args(["--env", env]);
    }
    let status = cmd
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
    let mut cmd = wrangler_command(worker_path);
    cmd.args(["secret", "put", key]);
    if let Some(env) = env_name {
        cmd.args(["--env", env]);
    }
    let mut child = cmd
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

    if is_1focus_source(cf_cfg.env_source.as_deref()) {
        let worker_path = if let Some(path) = cf_cfg.path.as_ref() {
            project_root.join(path)
        } else {
            let workers = discover_wrangler_configs(project_root)?;
            if workers.is_empty() {
                println!("No Cloudflare Worker config found (wrangler.toml/json).");
                println!("Run `wrangler init` first, then try: f deploy setup");
                return Ok(());
            }
            if workers.len() > 1 {
                bail!(
                    "Multiple Cloudflare worker configs found. Set [cloudflare].path in flow.toml."
                );
            }
            workers[0].clone()
        };

        ensure_wrangler_config(&worker_path)?;
        println!("Using Cloudflare worker: {}", worker_path.display());

        if !cf_cfg.bootstrap_secrets.is_empty() {
            println!("==> Bootstrapping secrets (optional)...");
            crate::env::run(Some(EnvAction::Bootstrap))?;
        }

        let env_name = cf_cfg
            .environment
            .clone()
            .unwrap_or_else(|| "production".to_string());
        let keys = collect_cloudflare_env_keys(cf_cfg);
        let env_store_ok = if keys.is_empty() {
            true
        } else {
            match crate::env::fetch_project_env_vars(&env_name, &keys) {
                Ok(vars) => !vars.is_empty(),
                Err(err) => {
                    eprintln!("⚠ Env store unavailable: {err}");
                    false
                }
            }
        };

        if env_store_ok {
            crate::env::run(Some(EnvAction::Guide { environment: env_name }))?;
            crate::env::run(Some(EnvAction::Apply))?;
        } else {
            eprintln!("⚠ Skipping env guide/apply (1focus unavailable).");
        }

        println!("\n✓ Cloudflare deploy setup complete.");
        return Ok(());
    }

    let defaults = CloudflareSetupDefaults {
        worker_path: cf_cfg
            .path
            .as_ref()
            .map(|p| project_root.join(p)),
        env_file: if is_1focus_source(cf_cfg.env_source.as_deref()) {
            None
        } else {
            cf_cfg.env_file
                .as_ref()
                .map(|p| project_root.join(p))
        },
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
        if is_1focus_source(cf_cfg.env_source.as_deref()) {
            if !cf_cfg.bootstrap_secrets.is_empty() {
                println!("==> Bootstrapping secrets (optional)...");
                crate::env::run(Some(EnvAction::Bootstrap))?;
            }

            let env_name = result
                .environment
                .clone()
                .unwrap_or_else(|| "production".to_string());
            crate::env::run(Some(EnvAction::Guide { environment: env_name }))?;
            crate::env::run(Some(EnvAction::Apply))?;
        } else if let Some(env_file) = result.env_file.as_ref() {
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
fn show_logs(project_root: &Path, config: Option<&Config>, follow: bool, lines: usize) -> Result<()> {
    if let Some(cf_cfg) = config.and_then(|c| c.cloudflare.as_ref()) {
        return show_cloudflare_logs(project_root, cf_cfg, follow, lines);
    }

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

fn show_cloudflare_logs(
    project_root: &Path,
    cf_cfg: &CloudflareConfig,
    follow: bool,
    lines: usize,
) -> Result<()> {
    let worker_path = cf_cfg
        .path
        .as_ref()
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.to_path_buf());
    ensure_wrangler_config(&worker_path)?;

    if !follow {
        eprintln!("Note: wrangler tail streams logs until you stop it (Ctrl+C).");
        let _ = lines;
    }

    let mut cmd = wrangler_command(&worker_path);
    cmd.arg("tail").args(["--format", "pretty"]);
    if let Some(env) = cf_cfg.environment.as_deref() {
        cmd.args(["--env", env]);
    }

    let status = cmd
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        bail!("Cloudflare log tail failed");
    }

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

/// Install the env-fetch script on the host.
/// This script fetches env vars from 1focus using a service token on startup.
fn install_env_fetch_script(
    conn: &HostConnection,
    dest: &str,
    service_token: &str,
    project_name: &str,
    environment: &str,
    keys: &[String],
) -> Result<()> {
    // Build the keys query parameter
    let keys_param = if keys.is_empty() {
        String::new()
    } else {
        format!("&keys={}", keys.join(","))
    };

    // Create the fetch script
    // The script fetches env vars from 1focus API and writes to .env
    let script = format!(
        r##"#!/bin/bash
# Auto-generated by flow - fetches env vars from 1focus on startup
# This token can ONLY read env vars for project: {project_name}

set -e

TOKEN_FILE="{dest}/.1focus-token"
ENV_FILE="{dest}/.env"
API_URL="https://1focus.ai/api/env/{project_name}?environment={environment}{keys_param}"

if [ ! -f "$TOKEN_FILE" ]; then
    echo "ERROR: Service token not found at $TOKEN_FILE" >&2
    exit 1
fi

TOKEN=$(cat "$TOKEN_FILE")

# Fetch env vars from 1focus
RESPONSE=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API_URL")

if [ $? -ne 0 ]; then
    echo "ERROR: Failed to fetch env vars from 1focus" >&2
    exit 1
fi

# Parse JSON and write to .env file
echo "# Environment: {environment} (fetched from 1focus)" > "$ENV_FILE"
echo "$RESPONSE" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for k, v in sorted(data.get('env', {{}}).items()):
    escaped = v.replace('\\', '\\\\').replace('\"', '\\\"')
    print(f'{{{{k}}}}=\"{{{{escaped}}}}\"')
" >> "$ENV_FILE"

chmod 600 "$ENV_FILE"
echo "Fetched env vars for {project_name} ({environment})"
"##
    );

    // Write script to temp file and copy
    let temp_script = std::env::temp_dir().join(format!("fetch-env-{}.sh", std::process::id()));
    fs::write(&temp_script, &script)?;
    scp_file(&temp_script, conn, &format!("{}/fetch-env.sh", dest))?;
    let _ = fs::remove_file(&temp_script);

    // Make executable
    ssh_run(conn, &format!("chmod +x {}/fetch-env.sh", dest))?;

    // Store the service token securely
    let temp_token = std::env::temp_dir().join(format!(".1focus-token-{}", std::process::id()));
    fs::write(&temp_token, service_token)?;
    scp_file(&temp_token, conn, &format!("{}/.1focus-token", dest))?;
    let _ = fs::remove_file(&temp_token);

    // Secure the token file (only readable by root)
    ssh_run(conn, &format!("chmod 600 {}/.1focus-token", dest))?;

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
    let exec_start = normalize_exec_start(workdir, exec_start);

    // Determine if we're using 1focus with service token (fetch on startup)
    let use_1focus = is_1focus_source(config.env_source.as_deref());
    let has_service_token = config.service_token.is_some();

    let env_file_line = if use_1focus || config.env_file.is_some() {
        format!("EnvironmentFile={}/.env", workdir)
    } else {
        String::new()
    };

    // Add ExecStartPre to fetch env vars if using service token
    let exec_start_pre = if use_1focus && has_service_token {
        format!("ExecStartPre={}/fetch-env.sh", workdir)
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
{exec_start_pre}
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

fn normalize_exec_start(workdir: &str, exec_start: &str) -> String {
    let trimmed = exec_start.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut parts =
        shell_words::split(trimmed).unwrap_or_else(|_| trimmed.split_whitespace().map(|s| s.to_string()).collect());
    if parts.is_empty() {
        return trimmed.to_string();
    }

    let cmd = parts[0].as_str();
    if cmd.starts_with('/') {
        return trimmed.to_string();
    }

    if cmd.starts_with("./") || cmd.starts_with("../") || cmd.contains('/') {
        let abs = Path::new(workdir)
            .join(cmd)
            .to_string_lossy()
            .to_string();
        parts[0] = abs;
        return shell_words::join(parts);
    }

    let mut env_parts = Vec::with_capacity(parts.len() + 1);
    env_parts.push("/usr/bin/env".to_string());
    env_parts.extend(parts);
    shell_words::join(env_parts)
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
