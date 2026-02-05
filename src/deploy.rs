//! Deploy projects to hosts and cloud platforms.
//!
//! Supports:
//! - Linux hosts via SSH (with systemd + nginx)
//! - Cloudflare Workers
//! - Railway

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use rpassword::prompt_password;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{DeployAction, DeployCommand, EnvAction, TaskRunOpts};
use crate::config::Config;
use crate::deploy_setup::{
    CloudflareSetupDefaults, CloudflareSetupResult, discover_wrangler_configs, run_cloudflare_setup,
};
use crate::env::parse_env_file;
use crate::release;
use crate::services;
use crate::tasks;

const DEPLOY_HELPER_BIN: &str = "infra";
const DEPLOY_HELPER_REPO_DEFAULT: &str = "~/infra";
const DEPLOY_HELPER_ENV_BIN: &str = "FLOW_DEPLOY_HELPER_BIN";
const DEPLOY_HELPER_ENV_REPO: &str = "FLOW_DEPLOY_HELPER_REPO";
const DEPLOY_LOG_STATE_FILE: &str = ".flow/deploy-log.json";

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DeployLogState {
    last_deploy_unix: Option<i64>,
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
    /// Env source for secrets ("cloud" or "file").
    pub env_source: Option<String>,
    /// Specific env keys to fetch when env_source = "cloud".
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Fetch from project-scoped env vars instead of personal (default).
    #[serde(default)]
    pub env_project: bool,
    /// Environment name for cloud (defaults to "production").
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
    /// Env source for secrets ("cloud" or "file").
    pub env_source: Option<String>,
    /// Specific env keys to fetch when env_source = "cloud".
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Env keys to set as non-secret vars when env_source = "cloud".
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

/// Production deploy overrides from flow.toml [prod] section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProdConfig {
    /// Custom domain to serve (e.g., app.example.com).
    pub domain: Option<String>,
    /// Explicit route pattern (e.g., app.example.com/*).
    pub route: Option<String>,
}

/// Web deployment config from flow.toml [web] section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebConfig {
    /// Path to web app directory (relative to project root).
    pub path: Option<String>,
    /// Domain for the site (used to derive route).
    pub domain: Option<String>,
    /// Explicit route to add in wrangler config (e.g., example.com/*).
    pub route: Option<String>,
    /// Env source for secrets ("cloud" or "file").
    pub env_source: Option<String>,
    /// Specific env keys to fetch when env_source = "cloud".
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Env keys to set as non-secret vars when env_source = "cloud".
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// Default values for env vars (key/value).
    #[serde(default)]
    pub env_defaults: HashMap<String, String>,
    /// Env apply mode: "always", "auto", or "never".
    pub env_apply: Option<String>,
    /// Wrangler environment name (e.g., staging).
    #[serde(default, alias = "env")]
    pub environment: Option<String>,
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
        || msg.contains("failed to connect to cloud")
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

fn deploy_log_state_path(project_root: &Path) -> PathBuf {
    project_root.join(DEPLOY_LOG_STATE_FILE)
}

fn load_deploy_log_state(project_root: &Path) -> DeployLogState {
    let path = deploy_log_state_path(project_root);
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(state) = serde_json::from_str::<DeployLogState>(&content) {
            return state;
        }
    }
    DeployLogState::default()
}

fn save_deploy_log_state(project_root: &Path, state: &DeployLogState) -> Result<()> {
    let path = deploy_log_state_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)?;
    fs::write(path, content)?;
    Ok(())
}

fn record_deploy_marker(project_root: &Path) -> Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut state = load_deploy_log_state(project_root);
    state.last_deploy_unix = Some(now);
    save_deploy_log_state(project_root, &state)
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
        Some(DeployAction::Host {
            remote_build,
            setup,
        }) => deploy_host(&project_root, flow_config.as_ref(), remote_build, setup),
        Some(DeployAction::Cloudflare { secrets, dev }) => {
            deploy_cloudflare(&project_root, flow_config.as_ref(), secrets, dev)
        }
        Some(DeployAction::Web) => deploy_web(&project_root, flow_config.as_ref()),
        Some(DeployAction::Setup) => setup_cloudflare(&project_root, flow_config.as_ref()),
        Some(DeployAction::Railway) => deploy_railway(&project_root, flow_config.as_ref()),
        Some(DeployAction::Config) => configure_deploy(),
        Some(DeployAction::Release(opts)) => release::run_task(opts),
        Some(DeployAction::Status) => show_status(&project_root, flow_config.as_ref()),
        Some(DeployAction::Logs {
            follow,
            since_deploy,
            all,
            lines,
        }) => show_logs(
            &project_root,
            flow_config.as_ref(),
            follow,
            since_deploy,
            all,
            lines,
        ),
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

/// Run a production deploy (skips flow.deploy_task and prefers deploy-prod/prod tasks).
pub fn run_prod(cmd: DeployCommand) -> Result<()> {
    let project_root = std::env::current_dir()?;
    let config_path = project_root.join("flow.toml");
    let flow_config = if config_path.exists() {
        Some(crate::config::load(&config_path)?)
    } else {
        None
    };

    match cmd.action {
        None => {
            let cfg = flow_config
                .as_ref()
                .context("No flow.toml found. Run `f init` first.")?;

            if tasks::find_task(cfg, "deploy-prod").is_some() {
                return tasks::run(TaskRunOpts {
                    config: config_path.clone(),
                    delegate_to_hub: false,
                    hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
                    hub_port: 9050,
                    name: "deploy-prod".to_string(),
                    args: Vec::new(),
                });
            }

            if tasks::find_task(cfg, "prod").is_some() {
                return tasks::run(TaskRunOpts {
                    config: config_path.clone(),
                    delegate_to_hub: false,
                    hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
                    hub_port: 9050,
                    name: "prod".to_string(),
                    args: Vec::new(),
                });
            }

            if cfg.host.is_some()
                || cfg.cloudflare.is_some()
                || cfg.railway.is_some()
                || cfg.web.is_some()
            {
                if cfg.host.is_some() {
                    println!("Detected [host] config, deploying to Linux host...");
                    return deploy_host(&project_root, Some(cfg), false, false);
                }

                if cfg.cloudflare.is_some() {
                    println!("Detected [cloudflare] config, deploying to Cloudflare...");
                    if let Err(err) = ensure_prod_cloudflare_routes(&project_root, cfg) {
                        eprintln!("WARN prod route setup skipped: {err}");
                    }
                    return deploy_cloudflare(&project_root, Some(cfg), false, false);
                }

                if cfg.railway.is_some() {
                    println!("Detected [railway] config, deploying to Railway...");
                    return deploy_railway(&project_root, Some(cfg));
                }

                if cfg.web.is_some() {
                    println!("Detected [web] config, deploying web...");
                    return deploy_web(&project_root, Some(cfg));
                }
            }

            bail!(
                "No production deploy config found in flow.toml.\n\n\
                Add one of:\n\
                [host]\n\
                dest = \"/opt/myapp\"\n\
                run = \"./server\"\n\n\
                [cloudflare]\n\
                path = \"worker\"\n\n\
                [railway]\n\
                project = \"my-project\"\n\n\
                [web]\n\
                path = \"packages/web\"\n\n\
                Or define a deploy-prod/prod task."
            );
        }
        Some(DeployAction::Host {
            remote_build,
            setup,
        }) => deploy_host(&project_root, flow_config.as_ref(), remote_build, setup),
        Some(DeployAction::Cloudflare { secrets, dev }) => {
            if let Some(cfg) = flow_config.as_ref() {
                if let Err(err) = ensure_prod_cloudflare_routes(&project_root, cfg) {
                    eprintln!("WARN prod route setup skipped: {err}");
                }
            }
            deploy_cloudflare(&project_root, flow_config.as_ref(), secrets, dev)
        }
        Some(DeployAction::Web) => deploy_web(&project_root, flow_config.as_ref()),
        Some(DeployAction::Setup) => setup_cloudflare(&project_root, flow_config.as_ref()),
        Some(DeployAction::Railway) => deploy_railway(&project_root, flow_config.as_ref()),
        Some(DeployAction::Config) => configure_deploy(),
        Some(DeployAction::Release(opts)) => release::run_task(opts),
        Some(DeployAction::Status) => show_status(&project_root, flow_config.as_ref()),
        Some(DeployAction::Logs {
            follow,
            since_deploy,
            all,
            lines,
        }) => show_logs(
            &project_root,
            flow_config.as_ref(),
            follow,
            since_deploy,
            all,
            lines,
        ),
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
        println!("Current host: {}@{}:{}", conn.user, conn.host, conn.port);
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

fn prompt_yes_no(message: &str, default_yes: bool) -> Result<bool> {
    let prompt = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("{message} {prompt}: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(answer == "y" || answer == "yes")
}

fn prompt_secret(message: &str) -> Result<String> {
    let value = prompt_password(message)?;
    Ok(value)
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

fn deploy_web(project_root: &Path, config: Option<&Config>) -> Result<()> {
    let (web_root, flow_path, mut cfg) = resolve_deploy_root(project_root, config)?;

    let mut changed = false;
    if ensure_web_config(&flow_path, &web_root, &cfg)? {
        changed = true;
        cfg = crate::config::load(&flow_path)?;
    }

    let web_cfg = cfg.web.as_ref().context("No [web] section in flow.toml")?;

    if ensure_web_domain_or_route(&flow_path, web_cfg)? {
        changed = true;
        cfg = crate::config::load(&flow_path)?;
    }
    let web_cfg = cfg.web.as_ref().context("No [web] section in flow.toml")?;

    if ensure_web_env_source(&flow_path, web_cfg)? {
        changed = true;
        cfg = crate::config::load(&flow_path)?;
    }
    let web_cfg = cfg.web.as_ref().context("No [web] section in flow.toml")?;

    if ensure_web_routes(&web_root, web_cfg)? {
        changed = true;
    }

    if changed {
        println!("Updated web deployment config.");
    }

    ensure_cloudflare_api_token()?;
    ensure_web_dns(web_cfg)?;

    if let Err(err) = apply_web_env(&web_root, web_cfg) {
        eprintln!("WARN env apply skipped: {err}");
        eprintln!("Hint: run `f env setup` to store missing web env vars.");
    }

    if tasks::find_task(&cfg, "deploy-web").is_some() {
        return tasks::run(TaskRunOpts {
            config: flow_path,
            delegate_to_hub: false,
            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
            hub_port: 9050,
            name: "deploy-web".to_string(),
            args: Vec::new(),
        });
    }

    if tasks::find_task(&cfg, "deploy").is_some() {
        eprintln!("WARN deploy-web task not found; running deploy.");
        return tasks::run(TaskRunOpts {
            config: flow_path,
            delegate_to_hub: false,
            hub_host: std::net::IpAddr::from([127, 0, 0, 1]),
            hub_port: 9050,
            name: "deploy".to_string(),
            args: Vec::new(),
        });
    }

    bail!("No deploy task found. Add 'deploy-web' or 'deploy' to flow.toml.");
}

fn resolve_deploy_root(
    project_root: &Path,
    config: Option<&Config>,
) -> Result<(PathBuf, PathBuf, Config)> {
    let Some(flow_path) = find_flow_toml_from(project_root) else {
        bail!("flow.toml not found. Run from your repo root.");
    };

    let root = flow_path.parent().unwrap_or(project_root).to_path_buf();
    let cfg = if root == project_root {
        match config {
            Some(existing) => existing.clone(),
            None => crate::config::load(&flow_path)?,
        }
    } else {
        crate::config::load(&flow_path)?
    };

    Ok((root, flow_path, cfg))
}

fn find_flow_toml_from(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        let candidate = current.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
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
    let use_cloud = is_cloud_source(host_cfg.env_source.as_deref());
    let use_flow = is_flow_source(host_cfg.env_source.as_deref());
    let has_service_token = host_cfg.service_token.is_some();

    if use_cloud && has_service_token {
        // Service token mode: install fetch script, host fetches env vars on startup
        let service_token = host_cfg.service_token.as_ref().unwrap();
        let env_name = host_cfg.environment.as_deref().unwrap_or("production");
        let project_name = project_root.file_name().unwrap().to_str().unwrap();

        println!("==> Installing env-fetch script (host will fetch on startup)...");
        install_env_fetch_script(
            conn,
            dest,
            service_token,
            project_name,
            env_name,
            &host_cfg.env_keys,
        )?;
    } else if use_cloud || use_flow {
        // Deploy-time fetch mode: fetch now and copy to host
        let env_name = host_cfg.environment.as_deref().unwrap_or("production");
        let keys = &host_cfg.env_keys;
        let use_project = host_cfg.env_project;

        if !keys.is_empty() {
            let source = if use_project {
                format!("project/{}", env_name)
            } else {
                "personal".to_string()
            };
            let source_label = if use_cloud { "cloud" } else { "flow" };
            println!(
                "==> Fetching env vars from {} ({})...",
                source_label, source
            );

            let fetch = || {
                if use_project {
                    crate::env::fetch_project_env_vars(env_name, keys)
                } else {
                    crate::env::fetch_personal_env_vars(keys)
                }
            };

            let result = if use_flow && host_cfg.env_source.as_deref() == Some("local") {
                with_local_env_backend(fetch)
            } else {
                fetch()
            };

            match result {
                Ok(mut vars) if !vars.is_empty() => {
                    if !keys.is_empty() {
                        let key_set: HashSet<_> = keys.iter().collect();
                        vars.retain(|k, _| key_set.contains(k));
                    }

                    // Generate .env content
                    let mut content = String::new();
                    content.push_str(&format!(
                        "# Source: {} {} (fetched at deploy)\n",
                        source_label, source
                    ));
                    let mut sorted_keys: Vec<_> = vars.keys().collect();
                    sorted_keys.sort();
                    for key in sorted_keys {
                        let value = &vars[key];
                        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                        content.push_str(&format!("{}=\"{}\"\n", key, escaped));
                    }

                    // Write to temp file and scp
                    let temp_env =
                        std::env::temp_dir().join(format!(".env.{}", std::process::id()));
                    fs::write(&temp_env, &content)?;
                    let remote_env = format!("{}/.env", dest);
                    println!("==> Copying {} env vars to remote...", vars.len());
                    scp_file(&temp_env, conn, &remote_env)?;
                    let _ = fs::remove_file(&temp_env);
                }
                Ok(_) => {
                    eprintln!("⚠ No env vars found in {} for {}", source_label, source);
                }
                Err(err) => {
                    eprintln!("⚠ Failed to fetch env vars from {}: {}", source_label, err);
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

    if let Err(err) = record_deploy_marker(project_root) {
        eprintln!("⚠ Failed to record deploy timestamp: {err}");
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
    let source = cf_cfg.env_source.as_deref();
    let use_cloud = is_cloud_source(source);
    let use_flow = is_flow_source(source);
    let use_env_store = use_cloud || use_flow;
    let source_label = if use_cloud { "cloud" } else { "flow" };

    let cloud_env = env_name.unwrap_or("production");
    let mut cloud_vars: HashMap<String, String> = HashMap::new();
    let mut cloud_loaded = false;

    if use_env_store {
        let keys = collect_cloudflare_env_keys(cf_cfg);
        if !cf_cfg.env_defaults.is_empty() {
            for key in &keys {
                if let Some(value) = cf_cfg.env_defaults.get(key) {
                    if !value.trim().is_empty() {
                        cloud_vars.insert(key.clone(), value.clone());
                    }
                }
            }
        }

        if !keys.is_empty() {
            let fetch = || crate::env::fetch_project_env_vars(cloud_env, &keys);
            let result = if use_flow && source == Some("local") {
                with_local_env_backend(fetch)
            } else {
                fetch()
            };
            match result {
                Ok(vars) => {
                    if !vars.is_empty() {
                        cloud_loaded = true;
                    }
                    cloud_vars.extend(vars);
                }
                Err(err) => {
                    if env_apply_mode == EnvApplyMode::Auto {
                        if is_tls_connect_error(&err) {
                            eprintln!(
                                "⚠ Unable to reach cloud (TLS/connect). Skipping env sync for now."
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
        if use_env_store {
            if cloud_loaded {
                apply_cloudflare_env_map(project_root, cf_cfg, &cloud_vars)?;
            } else if env_apply_mode == EnvApplyMode::Always {
                eprintln!(
                    "⚠ No env vars found in {} for environment '{}' (using defaults only).",
                    source_label, cloud_env
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

    if use_env_store && !cloud_vars.is_empty() {
        deploy_cmd.envs(&cloud_vars);
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
    let source = cf_cfg.env_source.as_deref();
    if !is_cloud_source(source) && !is_flow_source(source) {
        bail!(
            "cloudflare.env_source must be set to \"cloud\", \"flow\", or \"local\" to apply envs"
        );
    }

    let cloud_env = cf_cfg.environment.as_deref().unwrap_or("production");
    let keys = collect_cloudflare_env_keys(cf_cfg);
    let fetch = || crate::env::fetch_project_env_vars(cloud_env, &keys);
    let vars = if is_flow_source(source) && source == Some("local") {
        with_local_env_backend(fetch)?
    } else {
        fetch()?
    };
    if vars.is_empty() {
        bail!(
            "No env vars found in env store for environment '{}'",
            cloud_env
        );
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
    println!("==> Applying {} env var(s) from env store...", vars.len());
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
    let local_bin = worker_path
        .join("node_modules")
        .join(".bin")
        .join("wrangler");
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

fn is_cloud_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("cloud") | Some("remote") | Some("myflow")
    )
}

fn is_flow_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("flow") | Some("local")
    )
}

fn maybe_bootstrap_secrets(
    worker_path: &Path,
    cf_cfg: &CloudflareConfig,
    env_name: &str,
) -> Result<()> {
    if cf_cfg.bootstrap_secrets.is_empty() {
        return Ok(());
    }

    let mut env_store_missing = false;
    let existing = match crate::env::fetch_project_env_vars(env_name, &cf_cfg.bootstrap_secrets) {
        Ok(vars) => vars,
        Err(err) => {
            let msg = format!("{err:#}");
            if msg.contains("Project not found.") || msg.contains("Personal env vars not found.") {
                env_store_missing = true;
                HashMap::new()
            } else {
                eprintln!("⚠ Unable to check bootstrap secrets: {err}");
                println!("Run `f env bootstrap` later if needed.");
                return Ok(());
            }
        }
    };

    let missing: Vec<String> = cf_cfg
        .bootstrap_secrets
        .iter()
        .filter(|key| {
            existing
                .get(*key)
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        })
        .cloned()
        .collect();

    if missing.is_empty() {
        println!("Bootstrap secrets already configured; skipping.");
        return Ok(());
    }

    if let Ok(present) = list_cloudflare_secret_keys(worker_path, cf_cfg.environment.as_deref()) {
        if missing.iter().all(|key| present.contains(key)) {
            println!(
                "Bootstrap secrets missing in cloud but already present in Cloudflare; skipping."
            );
            println!("Run `f env bootstrap` if you want to rotate/store them in cloud.");
            return Ok(());
        }
    }

    if env_store_missing {
        println!("cloud env space not found yet; bootstrap will initialize it.");
    }

    println!("Bootstrap secrets missing: {}", missing.join(", "));
    println!("==> Bootstrapping secrets (optional)...");
    crate::env::run(Some(EnvAction::Bootstrap))?;
    Ok(())
}

fn list_cloudflare_secret_keys(
    worker_path: &Path,
    env_name: Option<&str>,
) -> Result<HashSet<String>> {
    let mut cmd = wrangler_command(worker_path);
    cmd.args(["secret", "list", "--json"]);
    if let Some(env) = env_name {
        cmd.args(["--env", env]);
    }
    let output = cmd.output()?;
    if !output.status.success() {
        bail!("wrangler secret list failed");
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("failed to parse wrangler secret list output")?;
    let mut keys = HashSet::new();
    if let Some(items) = value.as_array() {
        for item in items {
            if let Some(name) = item.get("name").and_then(|val| val.as_str()) {
                keys.insert(name.to_string());
            }
        }
    }
    Ok(keys)
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

    if is_cloud_source(cf_cfg.env_source.as_deref()) {
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

        let env_name = cf_cfg
            .environment
            .clone()
            .unwrap_or_else(|| "production".to_string());
        maybe_bootstrap_secrets(&worker_path, cf_cfg, &env_name)?;
        let keys = collect_cloudflare_env_keys(cf_cfg);
        let env_store_ok = if keys.is_empty() {
            true
        } else {
            match crate::env::fetch_project_env_vars(&env_name, &keys) {
                Ok(_) => true,
                Err(err) => {
                    let msg = format!("{err:#}");
                    if msg.contains("Project not found.") {
                        println!("Project not found yet; it will be created on first set.");
                        true
                    } else {
                        eprintln!("⚠ Env store unavailable: {err}");
                        false
                    }
                }
            }
        };

        if env_store_ok {
            if let Some(flow_cfg) = config {
                services::maybe_run_stripe_setup(project_root, flow_cfg, &env_name)?;
            }
            crate::env::run(Some(EnvAction::Guide {
                environment: env_name,
            }))?;
            crate::env::run(Some(EnvAction::Apply))?;
        } else {
            eprintln!("⚠ Skipping env guide/apply (cloud unavailable).");
        }

        println!("\n✓ Cloudflare deploy setup complete.");
        return Ok(());
    }

    let defaults = CloudflareSetupDefaults {
        worker_path: cf_cfg.path.as_ref().map(|p| project_root.join(p)),
        env_file: if is_cloud_source(cf_cfg.env_source.as_deref()) {
            None
        } else {
            cf_cfg.env_file.as_ref().map(|p| project_root.join(p))
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
        if is_cloud_source(cf_cfg.env_source.as_deref()) {
            let env_name = result
                .environment
                .clone()
                .unwrap_or_else(|| "production".to_string());
            maybe_bootstrap_secrets(&result.worker_path, cf_cfg, &env_name)?;
            crate::env::run(Some(EnvAction::Guide {
                environment: env_name,
            }))?;
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
                let output = ssh_capture(
                    conn,
                    &format!(
                        "systemctl is-active {} 2>/dev/null || echo inactive",
                        service
                    ),
                )?;
                println!("  Service '{}': {}", service, output.trim());
            }
        }
    } else {
        println!("Host: not configured");
    }

    Ok(())
}

/// Show deployment logs.
fn show_logs(
    project_root: &Path,
    config: Option<&Config>,
    follow: bool,
    since_deploy: bool,
    all: bool,
    lines: usize,
) -> Result<()> {
    if let Some(cf_cfg) = config.and_then(|c| c.cloudflare.as_ref()) {
        return show_cloudflare_logs(project_root, cf_cfg, follow, lines);
    }

    let deploy_config = load_deploy_config()?;
    let conn = deploy_config.host.as_ref().context("No host configured")?;

    let service = config
        .and_then(|c| c.host.as_ref())
        .and_then(|h| h.service.as_ref())
        .context("No service name in [host] config")?;

    let use_since_deploy = since_deploy && !all;
    let since_flag = if use_since_deploy {
        let state = load_deploy_log_state(project_root);
        if let Some(ts) = state.last_deploy_unix {
            format!("--since '@{}'", ts)
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    let follow_flag = if follow { "-f" } else { "" };
    let cmd = format!(
        "journalctl -u {} -n {} {} {} --no-pager",
        service, lines, follow_flag, since_flag
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
/// This script fetches env vars from cloud using a service token on startup.
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
    // The script fetches env vars from cloud API and writes to .env
    let script = format!(
        r##"#!/bin/bash
# Auto-generated by flow - fetches env vars from cloud on startup
# This token can ONLY read env vars for project: {project_name}

set -e

TOKEN_FILE="{dest}/.cloud-token"
ENV_FILE="{dest}/.env"
API_URL="https://myflow.sh/api/env/{project_name}?environment={environment}{keys_param}"

if [ ! -f "$TOKEN_FILE" ]; then
    echo "ERROR: Service token not found at $TOKEN_FILE" >&2
    exit 1
fi

TOKEN=$(cat "$TOKEN_FILE")

# Fetch env vars from cloud
RESPONSE=$(curl -sf -H "Authorization: Bearer $TOKEN" "$API_URL")

if [ $? -ne 0 ]; then
    echo "ERROR: Failed to fetch env vars from cloud" >&2
    exit 1
fi

# Parse JSON and write to .env file
echo "# Environment: {environment} (fetched from cloud)" > "$ENV_FILE"
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
    let temp_token = std::env::temp_dir().join(format!(".cloud-token-{}", std::process::id()));
    fs::write(&temp_token, service_token)?;
    scp_file(&temp_token, conn, &format!("{}/.cloud-token", dest))?;
    let _ = fs::remove_file(&temp_token);

    // Secure the token file (only readable by root)
    ssh_run(conn, &format!("chmod 600 {}/.cloud-token", dest))?;

    Ok(())
}

/// Check if systemd service exists.
fn service_exists(conn: &HostConnection, name: &str) -> Result<bool> {
    let output = ssh_capture(
        conn,
        &format!(
            "systemctl list-unit-files {} 2>/dev/null | grep -c {} || true",
            name, name
        ),
    )?;
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

    // Determine if we're using cloud with service token (fetch on startup)
    let use_cloud = is_cloud_source(config.env_source.as_deref());
    let has_service_token = config.service_token.is_some();

    let env_file_line = if use_cloud || config.env_file.is_some() {
        format!("EnvironmentFile={}/.env", workdir)
    } else {
        String::new()
    };

    // Add ExecStartPre to fetch env vars if using service token
    let exec_start_pre = if use_cloud && has_service_token {
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

    let mut parts = shell_words::split(trimmed)
        .unwrap_or_else(|_| trimmed.split_whitespace().map(|s| s.to_string()).collect());
    if parts.is_empty() {
        return trimmed.to_string();
    }

    let cmd = parts[0].as_str();
    if cmd.starts_with('/') {
        return trimmed.to_string();
    }

    if cmd.starts_with("./") || cmd.starts_with("../") || cmd.contains('/') {
        let abs = Path::new(workdir).join(cmd).to_string_lossy().to_string();
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
    let allowlist = selected_keys.map(|keys| keys.iter().cloned().collect::<HashSet<String>>());

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
        if !lines.is_empty()
            && !lines
                .last()
                .map(|line| line.trim().is_empty())
                .unwrap_or(false)
        {
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

fn ensure_web_config(flow_path: &Path, project_root: &Path, cfg: &Config) -> Result<bool> {
    let existing_path = cfg.web.as_ref().and_then(|web| web.path.clone());
    if existing_path.is_some() {
        return Ok(false);
    }

    let web_path = match detect_web_path(project_root)? {
        Some(path) => path,
        None => {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "No [web] section found and unable to infer web path. Add [web] path = \"...\"."
                );
            }
            let input = prompt_line("Web path (relative to repo root)", None)?;
            if input.trim().is_empty() {
                bail!("Web path required. Add [web] path = \"...\" in flow.toml.");
            }
            input
        }
    };

    ensure_web_path(flow_path, &web_path)
}

fn ensure_web_domain_or_route(flow_path: &Path, web_cfg: &WebConfig) -> Result<bool> {
    if web_cfg.domain.is_some() || web_cfg.route.is_some() {
        return Ok(false);
    }
    if !std::io::stdin().is_terminal() {
        bail!("web.domain or web.route is required in flow.toml.");
    }

    println!("Web routing setup");
    println!("-----------------");
    let domain = prompt_line("Domain (e.g., example.com)", None)?;
    if !domain.trim().is_empty() {
        return ensure_web_key(flow_path, "domain", &domain);
    }

    let route = prompt_line("Route (e.g., example.com/*)", None)?;
    if route.trim().is_empty() {
        bail!("web.domain or web.route is required to deploy web.");
    }
    ensure_web_key(flow_path, "route", &route)
}

fn ensure_web_env_source(flow_path: &Path, web_cfg: &WebConfig) -> Result<bool> {
    if web_cfg.env_source.is_some() {
        return Ok(false);
    }
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }

    if prompt_yes_no("Use cloud for web env vars?", true)? {
        let mut changed = false;
        if ensure_web_key(flow_path, "env_source", "cloud")? {
            changed = true;
        }
        if ensure_web_key(flow_path, "env_apply", "always")? {
            changed = true;
        }
        return Ok(changed);
    }

    if prompt_yes_no("Use local env store instead?", true)? {
        let mut changed = false;
        if ensure_web_key(flow_path, "env_source", "local")? {
            changed = true;
        }
        if ensure_web_key(flow_path, "env_apply", "always")? {
            changed = true;
        }
        return Ok(changed);
    }

    Ok(false)
}

fn detect_web_path(project_root: &Path) -> Result<Option<String>> {
    let packages_web = project_root.join("packages").join("web");
    if packages_web.join("wrangler.jsonc").exists()
        || packages_web.join("wrangler.json").exists()
        || packages_web.join("wrangler.toml").exists()
    {
        return Ok(Some("packages/web".to_string()));
    }

    if project_root.join("wrangler.jsonc").exists()
        || project_root.join("wrangler.json").exists()
        || project_root.join("wrangler.toml").exists()
    {
        return Ok(Some(".".to_string()));
    }

    let configs = discover_wrangler_configs(project_root)?;
    if configs.len() == 1 {
        let rel = relative_path(project_root, &configs[0]);
        if rel.is_empty() {
            return Ok(Some(".".to_string()));
        }
        return Ok(Some(rel));
    }

    Ok(None)
}

fn ensure_web_path(flow_path: &Path, web_path: &str) -> Result<bool> {
    ensure_web_key(flow_path, "path", web_path)
}

fn ensure_web_key(flow_path: &Path, key: &str, value: &str) -> Result<bool> {
    let contents = fs::read_to_string(flow_path)?;
    let mut lines: Vec<String> = contents.lines().map(|line| line.to_string()).collect();
    let had_trailing_newline = contents.ends_with('\n');

    let mut changed = false;
    if let Some(start) = lines.iter().position(|line| line.trim() == "[web]") {
        let end = find_section_end(&lines, start + 1);
        let mut section_lines = lines[start + 1..end].to_vec();
        if !section_has_key(&section_lines, key) {
            section_lines.push(format!("{key} = \"{}\"", value.trim()));
            changed = true;
        }

        let mut updated = Vec::new();
        updated.extend_from_slice(&lines[..start + 1]);
        updated.extend(section_lines);
        updated.extend_from_slice(&lines[end..]);
        lines = updated;
    } else {
        if !lines.is_empty()
            && !lines
                .last()
                .map(|line| line.trim().is_empty())
                .unwrap_or(false)
        {
            lines.push(String::new());
        }
        lines.push("[web]".to_string());
        lines.push(format!("{key} = \"{}\"", value.trim()));
        changed = true;
    }

    if changed {
        let mut updated = lines.join("\n");
        if had_trailing_newline {
            updated.push('\n');
        }
        fs::write(flow_path, updated)?;
    }

    Ok(changed)
}

fn section_has_key(lines: &[String], key: &str) -> bool {
    let key_prefix = format!("{key} ");
    let key_eq = format!("{key}=");
    lines.iter().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with(&key_prefix) || trimmed.starts_with(&key_eq)
    })
}

fn ensure_web_routes(project_root: &Path, web_cfg: &WebConfig) -> Result<bool> {
    let Some(route) = resolve_web_route(web_cfg) else {
        eprintln!("WARN web route not set. Add web.route or web.domain in flow.toml.");
        return Ok(false);
    };

    let web_path = web_cfg.path.as_deref().unwrap_or(".");
    let web_root = project_root.join(web_path);
    ensure_wrangler_config(&web_root)?;

    let Some(config_path) = find_wrangler_route_file(&web_root) else {
        eprintln!(
            "WARN No wrangler.json/jsonc found in {}; add route manually.",
            web_root.display()
        );
        return Ok(false);
    };

    ensure_wrangler_routes_jsonc(&config_path, &route)
}

fn ensure_web_dns(web_cfg: &WebConfig) -> Result<()> {
    let Some(domain) = resolve_web_domain(web_cfg) else {
        return Ok(());
    };
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }

    println!("DNS setup");
    println!("---------");
    println!("Domain: {}", domain);
    if !prompt_yes_no("Manage DNS record in Cloudflare?", true)? {
        return Ok(());
    }

    let token = std::env::var("CLOUDFLARE_API_TOKEN")
        .context("Cloudflare API token missing. Run `f env new` -> Cloudflare token.")?;
    let client = cloudflare_api_client()?;
    let lookup_domain = domain.trim_start_matches("*.");
    let Some((zone_id, zone_name)) = find_cloudflare_zone(&client, &token, lookup_domain)? else {
        eprintln!("WARN No Cloudflare zone found for {}.", lookup_domain);
        return Ok(());
    };

    let record_type = prompt_line("DNS record type (A or CNAME)", Some("A"))?;
    let record_type = record_type.trim().to_ascii_uppercase();
    if record_type.is_empty() {
        bail!("DNS record type required.");
    }

    let default_target = if record_type == "CNAME" {
        zone_name.clone()
    } else {
        "192.0.2.1".to_string()
    };
    let target = prompt_line("DNS record target", Some(&default_target))?;
    let target = target.trim();
    if target.is_empty() {
        bail!("DNS record target required.");
    }
    let proxied = prompt_yes_no("Proxy through Cloudflare?", true)?;

    upsert_cloudflare_dns_record(
        &client,
        &token,
        &zone_id,
        &domain,
        &record_type,
        target,
        proxied,
    )?;
    println!("OK DNS record configured for {}", domain);
    Ok(())
}

fn resolve_web_route(web_cfg: &WebConfig) -> Option<String> {
    if let Some(route) = web_cfg.route.as_ref() {
        return Some(route.clone());
    }
    web_cfg
        .domain
        .as_ref()
        .map(|domain| format!("{}/*", domain.trim()))
}

fn resolve_web_domain(web_cfg: &WebConfig) -> Option<String> {
    if let Some(domain) = web_cfg.domain.as_ref() {
        let trimmed = domain.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(trimmed.to_string());
    }
    let route = web_cfg.route.as_ref()?.trim();
    if route.is_empty() {
        return None;
    }
    let route = route
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = route.split('/').next().unwrap_or(route).trim();
    if host.is_empty() || host == "*" {
        return None;
    }
    let host = host.trim_end_matches("/*").trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

fn resolve_prod_route(prod_cfg: &ProdConfig) -> Option<String> {
    if let Some(route) = prod_cfg.route.as_ref() {
        let route = route.trim();
        if !route.is_empty() {
            return Some(route.to_string());
        }
    }
    let domain = prod_cfg.domain.as_ref()?.trim();
    if domain.is_empty() {
        return None;
    }
    let domain = domain
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if domain.is_empty() || domain == "*" {
        return None;
    }
    Some(format!("{}/*", domain))
}

fn ensure_prod_cloudflare_routes(project_root: &Path, config: &Config) -> Result<()> {
    let Some(prod_cfg) = config.prod.as_ref() else {
        return Ok(());
    };
    let Some(cf_cfg) = config.cloudflare.as_ref() else {
        return Ok(());
    };
    let Some(route) = resolve_prod_route(prod_cfg) else {
        return Ok(());
    };

    let worker_root = cf_cfg
        .path
        .as_ref()
        .map(|p| project_root.join(p))
        .unwrap_or_else(|| project_root.to_path_buf());

    let Some(config_path) = find_wrangler_route_file(&worker_root) else {
        eprintln!(
            "WARN No wrangler.json/jsonc found in {}; add route '{}' manually.",
            worker_root.display(),
            route
        );
        return Ok(());
    };

    if ensure_wrangler_routes_jsonc(&config_path, &route)? {
        println!("Added prod route '{}' to {}", route, config_path.display());
    }
    if ensure_wrangler_bool_jsonc(&config_path, "workers_dev", true)? {
        println!("Enabled workers_dev in {}", config_path.display());
    }
    if ensure_wrangler_bool_jsonc(&config_path, "preview_urls", true)? {
        println!("Enabled preview_urls in {}", config_path.display());
    }

    Ok(())
}

fn find_wrangler_route_file(web_root: &Path) -> Option<PathBuf> {
    let jsonc = web_root.join("wrangler.jsonc");
    if jsonc.exists() {
        return Some(jsonc);
    }
    let json = web_root.join("wrangler.json");
    if json.exists() {
        return Some(json);
    }
    None
}

fn apply_web_env(project_root: &Path, web_cfg: &WebConfig) -> Result<()> {
    let env_apply_mode = env_apply_mode_from_str(web_cfg.env_apply.as_deref());
    if env_apply_mode == EnvApplyMode::Never {
        return Ok(());
    }
    let source = web_cfg.env_source.as_deref();
    if !is_cloud_source(source) && !is_local_source(source) {
        return Ok(());
    }

    if is_local_source(source) {
        unsafe {
            std::env::set_var("FLOW_ENV_BACKEND", "local");
        }
    }

    let keys = collect_web_env_keys(web_cfg);
    if keys.is_empty() {
        return Ok(());
    }

    let env_name = web_cfg.environment.as_deref().unwrap_or("production");
    let mut vars: HashMap<String, String> = HashMap::new();
    for key in &keys {
        if let Some(value) = web_cfg.env_defaults.get(key) {
            if !value.trim().is_empty() {
                vars.insert(key.clone(), value.clone());
            }
        }
    }

    match crate::env::fetch_project_env_vars(env_name, &keys) {
        Ok(fetched) => {
            vars.extend(fetched);
        }
        Err(err) => {
            if env_apply_mode == EnvApplyMode::Auto {
                eprintln!("WARN env sync skipped: {err}");
                return Ok(());
            }
            return Err(err);
        }
    }

    let web_path = web_cfg.path.as_deref().unwrap_or(".");
    let web_root = project_root.join(web_path);
    ensure_wrangler_config(&web_root)?;

    let var_keys: HashSet<String> = web_cfg.env_vars.iter().cloned().collect();
    set_wrangler_env_map(&web_root, web_cfg.environment.as_deref(), &vars, &var_keys)?;
    Ok(())
}

fn collect_web_env_keys(web_cfg: &WebConfig) -> Vec<String> {
    let mut keys = Vec::new();
    let mut seen = HashSet::new();
    for key in web_cfg.env_keys.iter().chain(web_cfg.env_vars.iter()) {
        if seen.insert(key.clone()) {
            keys.push(key.clone());
        }
    }
    keys
}

fn is_local_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("local")
    )
}

fn ensure_cloudflare_api_token() -> Result<()> {
    if std::env::var("CLOUDFLARE_API_TOKEN")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return Ok(());
    }

    let key = "CLOUDFLARE_API_TOKEN".to_string();
    let mut token = fetch_personal_env_value(&key)?;
    if token.is_none() && std::io::stdin().is_terminal() {
        println!("Cloudflare API token required for deploy.");
        println!("How to get it:");
        println!("  - Open https://dash.cloudflare.com/profile/api-tokens");
        println!("  - Create a token (Template: Edit Cloudflare Workers or Custom)");
        println!("  - Permissions: Workers Scripts:Edit, Workers Routes:Edit, Pages:Edit");
        println!("  - Add Zone:Read + DNS:Edit for your domain");
        println!("  - Copy the token value");
        println!();

        if !prompt_yes_no("Save token now?", true)? {
            bail!("Cloudflare token required to deploy.");
        }
        let default_store = if wants_local_env_backend() {
            "local"
        } else {
            "cloud"
        };
        let store = prompt_line("Store token in (cloud/local)", Some(default_store))?;
        let store = store.trim().to_ascii_lowercase();
        let store_local = matches!(store.as_str(), "local" | "l");
        let store_cloud = matches!(store.as_str(), "cloud" | "c");
        if !store_local && !store_cloud {
            bail!("Store token in cloud or local.");
        }

        let input = prompt_secret("Enter Cloudflare API token (input hidden): ")?;
        if input.trim().is_empty() {
            bail!("Cloudflare token required to deploy.");
        }
        if store_local {
            with_local_env_backend(|| crate::env::set_personal_env_var(&key, input.trim()))?;
        } else {
            crate::env::set_personal_env_var(&key, input.trim())?;
        }
        token = Some(input);
        println!("Saved {} to env store.", key);
    }

    let Some(token) = token else {
        bail!(
            "Cloudflare API token required. Store it as personal env key {}.",
            key
        );
    };

    unsafe {
        std::env::set_var("CLOUDFLARE_API_TOKEN", token.trim());
    }

    Ok(())
}

fn wants_local_env_backend() -> bool {
    if let Some(backend) = crate::config::preferred_env_backend() {
        return backend == "local";
    }
    if let Ok(value) = std::env::var("FLOW_ENV_BACKEND") {
        return value.trim().eq_ignore_ascii_case("local");
    }
    std::env::var("FLOW_ENV_LOCAL")
        .ok()
        .map(|value| value.trim() == "1" || value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn with_local_env_backend<T>(action: impl FnOnce() -> Result<T>) -> Result<T> {
    let previous = std::env::var("FLOW_ENV_BACKEND").ok();
    unsafe {
        std::env::set_var("FLOW_ENV_BACKEND", "local");
    }
    let result = action();
    unsafe {
        match previous {
            Some(value) => std::env::set_var("FLOW_ENV_BACKEND", value),
            None => std::env::remove_var("FLOW_ENV_BACKEND"),
        }
    }
    result
}

fn cloudflare_api_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("failed to build Cloudflare API client")
}

fn find_cloudflare_zone(
    client: &Client,
    token: &str,
    domain: &str,
) -> Result<Option<(String, String)>> {
    for candidate in cloudflare_zone_candidates(domain) {
        let resp = client
            .get("https://api.cloudflare.com/client/v4/zones")
            .bearer_auth(token)
            .query(&[("name", candidate.as_str()), ("status", "active")])
            .send()
            .context("failed to query Cloudflare zones")?;
        let json: Value = resp
            .json()
            .context("failed to parse Cloudflare zones response")?;
        cloudflare_api_check(&json, "listing zones")?;
        if let Some(zone) = json["result"].as_array().and_then(|arr| arr.first()) {
            if let (Some(id), Some(name)) = (zone["id"].as_str(), zone["name"].as_str()) {
                return Ok(Some((id.to_string(), name.to_string())));
            }
        }
    }
    Ok(None)
}

fn cloudflare_zone_candidates(domain: &str) -> Vec<String> {
    let trimmed = domain.trim().trim_end_matches('.');
    let parts: Vec<&str> = trimmed.split('.').filter(|part| !part.is_empty()).collect();
    if parts.len() < 2 {
        return vec![trimmed.to_string()];
    }
    let mut candidates = Vec::new();
    for i in 0..parts.len() - 1 {
        let candidate = parts[i..].join(".");
        if candidate.split('.').count() >= 2 {
            candidates.push(candidate);
        }
    }
    candidates
}

fn upsert_cloudflare_dns_record(
    client: &Client,
    token: &str,
    zone_id: &str,
    domain: &str,
    record_type: &str,
    target: &str,
    proxied: bool,
) -> Result<()> {
    if let Some(existing) =
        fetch_cloudflare_dns_record(client, token, zone_id, domain, record_type)?
    {
        if existing.content == target && existing.proxied == proxied {
            println!("OK DNS record already set for {}", domain);
            return Ok(());
        }
        let url = format!(
            "https://api.cloudflare.com/client/v4/zones/{}/dns_records/{}",
            zone_id, existing.id
        );
        let resp = client
            .put(&url)
            .bearer_auth(token)
            .json(&serde_json::json!({
                "type": record_type,
                "name": domain,
                "content": target,
                "proxied": proxied,
                "ttl": 1,
            }))
            .send()
            .context("failed to update Cloudflare DNS record")?;
        let json: Value = resp.json().context("failed to parse DNS update response")?;
        cloudflare_api_check(&json, "updating DNS record")?;
        return Ok(());
    }

    let url = format!(
        "https://api.cloudflare.com/client/v4/zones/{}/dns_records",
        zone_id
    );
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "type": record_type,
            "name": domain,
            "content": target,
            "proxied": proxied,
            "ttl": 1,
        }))
        .send()
        .context("failed to create Cloudflare DNS record")?;
    let json: Value = resp.json().context("failed to parse DNS create response")?;
    cloudflare_api_check(&json, "creating DNS record")?;
    Ok(())
}

struct CloudflareDnsRecord {
    id: String,
    content: String,
    proxied: bool,
}

fn fetch_cloudflare_dns_record(
    client: &Client,
    token: &str,
    zone_id: &str,
    domain: &str,
    record_type: &str,
) -> Result<Option<CloudflareDnsRecord>> {
    let url = format!(
        "https://api.cloudflare.com/client/v4/zones/{}/dns_records",
        zone_id
    );
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .query(&[("type", record_type), ("name", domain)])
        .send()
        .context("failed to query Cloudflare DNS records")?;
    let json: Value = resp.json().context("failed to parse DNS record response")?;
    cloudflare_api_check(&json, "listing DNS records")?;
    let Some(record) = json["result"].as_array().and_then(|arr| arr.first()) else {
        return Ok(None);
    };
    let id = record["id"].as_str().unwrap_or_default().to_string();
    if id.is_empty() {
        return Ok(None);
    }
    let content = record["content"].as_str().unwrap_or_default().to_string();
    let proxied = record["proxied"].as_bool().unwrap_or(false);
    Ok(Some(CloudflareDnsRecord {
        id,
        content,
        proxied,
    }))
}

fn cloudflare_api_check(payload: &Value, action: &str) -> Result<()> {
    if payload["success"].as_bool().unwrap_or(false) {
        return Ok(());
    }
    let message = payload["errors"]
        .as_array()
        .and_then(|errs| errs.first())
        .and_then(|err| err.get("message"))
        .and_then(|value| value.as_str())
        .unwrap_or("Unknown error");
    bail!("Cloudflare API error while {}: {}", action, message)
}

fn fetch_personal_env_value(key: &str) -> Result<Option<String>> {
    let keys = vec![key.to_string()];
    match crate::env::fetch_personal_env_vars(&keys) {
        Ok(vars) => Ok(vars.get(key).cloned()),
        Err(err) => {
            if is_not_logged_in_err(&err) || is_cloud_unavailable(&err) {
                return Ok(None);
            }
            Err(err)
        }
    }
}

fn is_not_logged_in_err(err: &anyhow::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("not logged in")
}

fn is_cloud_unavailable(err: &anyhow::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("failed to connect to cloud")
}

fn ensure_wrangler_routes_jsonc(path: &Path, route: &str) -> Result<bool> {
    let contents = fs::read_to_string(path)?;
    if contents.contains(route) {
        return Ok(false);
    }
    if contents.contains("\"routes\"") {
        eprintln!(
            "WARN {} has routes configured; add '{}' manually if needed.",
            path.display(),
            route
        );
        return Ok(false);
    }

    let insert_block = format!("\"routes\": [\n  \"{}\"\n]", route);

    let mut lines: Vec<String> = contents.lines().map(|line| line.to_string()).collect();
    let had_trailing_newline = contents.ends_with('\n');
    if let Some(pos) = lines.iter().rposition(|line| line.trim() == "}") {
        let needs_comma = lines
            .iter()
            .take(pos)
            .rfind(|line| !line.trim().is_empty())
            .map(|line| !line.trim_end().ends_with(',') && !line.trim_end().ends_with('{'))
            .unwrap_or(false);
        if needs_comma {
            if let Some(last) = lines
                .iter_mut()
                .take(pos)
                .rfind(|line| !line.trim().is_empty())
            {
                if !last.trim_end().ends_with(',') {
                    last.push(',');
                }
            }
        }
        let mut block_lines: Vec<String> = insert_block
            .lines()
            .map(|line| format!("  {line}"))
            .collect();
        lines.splice(pos..pos, block_lines.drain(..));
        let mut updated = lines.join("\n");
        if had_trailing_newline {
            updated.push('\n');
        }
        fs::write(path, updated)?;
        return Ok(true);
    }

    Ok(false)
}

fn ensure_wrangler_bool_jsonc(path: &Path, key: &str, value: bool) -> Result<bool> {
    let contents = fs::read_to_string(path)?;
    let needle = format!("\"{key}\"");
    if contents.contains(&needle) {
        return Ok(false);
    }

    let insert_block = format!("\"{key}\": {}", if value { "true" } else { "false" });

    let mut lines: Vec<String> = contents.lines().map(|line| line.to_string()).collect();
    let had_trailing_newline = contents.ends_with('\n');
    if let Some(pos) = lines.iter().rposition(|line| line.trim() == "}") {
        let needs_comma = lines
            .iter()
            .take(pos)
            .rfind(|line| !line.trim().is_empty())
            .map(|line| !line.trim_end().ends_with(',') && !line.trim_end().ends_with('{'))
            .unwrap_or(false);
        if needs_comma {
            if let Some(last) = lines
                .iter_mut()
                .take(pos)
                .rfind(|line| !line.trim().is_empty())
            {
                if !last.trim_end().ends_with(',') {
                    last.push(',');
                }
            }
        }
        let mut block_lines: Vec<String> = insert_block
            .lines()
            .map(|line| format!("  {line}"))
            .collect();
        lines.splice(pos..pos, block_lines.drain(..));
        let mut updated = lines.join("\n");
        if had_trailing_newline {
            updated.push('\n');
        }
        fs::write(path, updated)?;
        return Ok(true);
    }

    Ok(false)
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
                bail!(
                    "No URL configured in [cloudflare]. Add 'url = \"https://...\"' or use --url."
                );
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
