//! Environment variable management via 1focus.
//!
//! Fetches, sets, and manages environment variables for projects
//! using the 1focus API.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::cli::EnvAction;
use crate::config;
use crate::deploy;
use crate::env_setup::{EnvSetupDefaults, run_env_setup};
use crate::sync;

const DEFAULT_API_URL: &str = "https://1focus.ai";

/// Auth config stored in ~/.config/flow/auth.toml
#[derive(Debug, Serialize, Deserialize, Default)]
struct AuthConfig {
    token: Option<String>,
    api_url: Option<String>,
}

/// An env var with optional description.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct EnvVar {
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Response from /api/env/:projectName
#[derive(Debug, Deserialize)]
struct EnvResponse {
    env: HashMap<String, String>,
    #[serde(default)]
    descriptions: HashMap<String, String>,
    project: String,
    environment: String,
}

/// Response from POST /api/env/:projectName
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SetEnvResponse {
    success: bool,
    project: String,
    environment: String,
}

/// Response from /api/env/personal
#[derive(Debug, Deserialize)]
struct PersonalEnvResponse {
    env: HashMap<String, String>,
}

/// Get the auth config path.
fn get_auth_config_path() -> PathBuf {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flow");
    config_dir.join("auth.toml")
}

/// Load auth config.
fn load_auth_config() -> Result<AuthConfig> {
    let path = get_auth_config_path();
    if !path.exists() {
        return Ok(AuthConfig::default());
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).context("failed to parse auth.toml")
}

/// Save auth config.
fn save_auth_config(config: &AuthConfig) -> Result<()> {
    let path = get_auth_config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = toml::to_string_pretty(config)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Get the project name from flow.toml.
fn get_project_name() -> Result<String> {
    let cwd = std::env::current_dir()?;
    let flow_toml = cwd.join("flow.toml");

    if flow_toml.exists() {
        let cfg = config::load(&flow_toml)?;
        if let Some(name) = cfg.project_name {
            return Ok(name);
        }
    }

    // Fall back to directory name
    let name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unnamed".to_string());

    Ok(name)
}

/// Get API URL from config or default.
fn get_api_url(auth: &AuthConfig) -> String {
    auth.api_url
        .clone()
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

fn find_flow_toml(start: &PathBuf) -> Option<PathBuf> {
    let mut current = start.clone();
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

fn is_1focus_source(source: Option<&str>) -> bool {
    matches!(
        source.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("1focus") | Some("1f") | Some("onefocus")
    )
}

pub fn get_personal_env_var(key: &str) -> Result<Option<String>> {
    let auth = load_auth_config()?;
    let token = match auth.token.as_ref() {
        Some(t) => t,
        None => return Ok(None),
    };

    let api_url = get_api_url(&auth);
    let url = format!("{}/api/env/personal?keys={}", api_url, key);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to 1focus")?;

    if resp.status() == 401 {
        return Ok(None);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let data: PersonalEnvResponse = resp.json().context("failed to parse response")?;
    Ok(data.env.get(key).cloned())
}

/// Run the env subcommand.
pub fn run(action: Option<EnvAction>) -> Result<()> {
    // No action = run sync (base env + agents.md setup)
    let Some(action) = action else {
        return sync::run();
    };

    match action {
        EnvAction::Login => login()?,
        EnvAction::Pull { environment } => pull(&environment)?,
        EnvAction::Push { environment } => push(&environment)?,
        EnvAction::Guide { environment } => guide(&environment)?,
        EnvAction::Apply => {
            let cwd = std::env::current_dir()?;
            let flow_path = find_flow_toml(&cwd)
                .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run `f init` first."))?;
            let project_root = flow_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(cwd);
            let flow_config = config::load(&flow_path)?;
            deploy::apply_cloudflare_env(&project_root, Some(&flow_config))?;
        }
        EnvAction::Setup { env_file, environment } => setup(env_file, environment)?,
        EnvAction::List { environment } => list(&environment)?,
        EnvAction::Set { pair, environment, description } => set_var(&pair, &environment, description.as_deref())?,
        EnvAction::Delete { keys, environment } => delete_vars(&keys, &environment)?,
        EnvAction::Status => status()?,
        EnvAction::Get { keys, personal, environment, format } => {
            get_vars(&keys, personal, &environment, &format)?
        }
        EnvAction::Run { personal, environment, keys, command } => {
            run_with_env(personal, &environment, &keys, &command)?
        }
    }

    Ok(())
}

/// Login / set token.
fn login() -> Result<()> {
    let mut auth = load_auth_config()?;

    println!("1focus Environment Manager");
    println!("─────────────────────────────");
    println!();
    println!("To get a token:");
    println!("  1. Go to {} and sign in", DEFAULT_API_URL);
    println!("  2. Go to Settings → API Tokens");
    println!("  3. Create a new token");
    println!();

    print!("Enter your API token: ");
    io::stdout().flush()?;

    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        bail!("Token cannot be empty");
    }

    if !token.starts_with("1f_") {
        println!("Warning: Token doesn't start with '1f_' - are you sure this is correct?");
    }

    auth.token = Some(token);
    save_auth_config(&auth)?;

    println!();
    println!("✓ Token saved to {}", get_auth_config_path().display());
    println!();
    println!("You can now use:");
    println!("  f env pull    - Fetch env vars for this project");
    println!("  f env push    - Push local .env to 1focus");
    println!("  f env list    - List env vars");

    Ok(())
}

/// Pull env vars from 1focus and write to .env.
fn pull(environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    println!("Fetching envs for '{}' ({})...", project, environment);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!(
        "{}/api/env/{}?environment={}",
        api_url, project, environment
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to 1focus")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        bail!(
            "Project '{}' not found. Create it with `f env push` first.",
            project
        );
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let data: EnvResponse = resp.json().context("failed to parse response")?;

    if data.env.is_empty() {
        println!("No env vars found for '{}' ({})", project, environment);
        return Ok(());
    }

    // Write to .env
    let mut content = String::new();
    content.push_str(&format!(
        "# Environment: {} (pulled from 1focus)\n",
        environment
    ));
    content.push_str(&format!("# Project: {}\n", project));
    content.push_str("#\n");

    let mut keys: Vec<_> = data.env.keys().collect();
    keys.sort();

    for key in keys {
        let value = &data.env[key];
        // Escape quotes in value
        let escaped = value.replace('\"', "\\\"");
        content.push_str(&format!("{}=\"{}\"\n", key, escaped));
    }

    let env_path = std::env::current_dir()?.join(".env");
    fs::write(&env_path, &content)?;

    println!("✓ Wrote {} env vars to .env", data.env.len());

    Ok(())
}

/// Push local .env to 1focus.
fn push(environment: &str) -> Result<()> {
    let env_path = std::env::current_dir()?.join(".env");
    if !env_path.exists() {
        bail!(".env file not found");
    }

    let content = fs::read_to_string(&env_path)?;
    let vars = parse_env_file(&content);

    if vars.is_empty() {
        println!("No env vars found in .env");
        return Ok(());
    }

    push_vars(environment, vars)
}

fn push_vars(environment: &str, vars: HashMap<String, String>) -> Result<()> {
    if vars.is_empty() {
        println!("No env vars selected.");
        return Ok(());
    }

    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;
    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    println!(
        "Pushing {} env vars to '{}' ({})...",
        vars.len(),
        project,
        environment
    );

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/{}", api_url, project);
    let body = serde_json::json!({
        "vars": vars,
        "environment": environment,
    });

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to 1focus")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let _: SetEnvResponse = resp.json().context("failed to parse response")?;

    println!("✓ Pushed {} env vars to 1focus", vars.len());

    Ok(())
}

fn guide(environment: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let flow_path = find_flow_toml(&cwd)
        .ok_or_else(|| anyhow::anyhow!("flow.toml not found. Run `f init` first."))?;
    let cfg = config::load(&flow_path)?;

    let cf_cfg = cfg
        .cloudflare
        .as_ref()
        .context("No [cloudflare] section in flow.toml")?;

    let mut required = Vec::new();
    let mut seen = HashSet::new();
    for key in cf_cfg.env_keys.iter().chain(cf_cfg.env_vars.iter()) {
        if seen.insert(key.clone()) {
            required.push(key.clone());
        }
    }

    if required.is_empty() {
        bail!("No env keys configured. Add cloudflare.env_keys or cloudflare.env_vars to flow.toml.");
    }

    println!("Checking required env vars for '{}'...", environment);
    let existing = fetch_project_env_vars(environment, &required)?;
    let var_keys: HashSet<String> = cf_cfg.env_vars.iter().cloned().collect();

    let mut missing = Vec::new();
    for key in &required {
        if existing.get(key).map(|v| !v.trim().is_empty()).unwrap_or(false) {
            println!("  ✓ {}", key);
        } else {
            println!("  ✗ {} (missing)", key);
            missing.push(key.clone());
        }
    }

    if missing.is_empty() {
        println!("✓ All required env vars are set.");
        return Ok(());
    }

    println!();
    println!("Enter missing values (leave empty to skip).");
    for key in missing {
        let value = if var_keys.contains(&key) {
            prompt_line(&format!("{}: ", key))?
        } else {
            prompt_secret(&format!("{}: ", key))?
        };

        if let Some(value) = value {
            set_project_env_var(&key, &value, environment, None)?;
        }
    }

    Ok(())
}

fn prompt_line(label: &str) -> Result<Option<String>> {
    print!("{}", label);
    io::stdout().flush()?;

    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn prompt_secret(label: &str) -> Result<Option<String>> {
    let value = rpassword::prompt_password(label)?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn setup(env_file: Option<PathBuf>, environment: Option<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let flow_path = find_flow_toml(&cwd);
    let (project_root, flow_cfg) = if let Some(path) = flow_path {
        let cfg = config::load(&path)?;
        let root = path.parent().unwrap_or(&cwd).to_path_buf();
        (root, Some(cfg))
    } else {
        (cwd, None)
    };

    let cf_cfg = flow_cfg.as_ref().and_then(|cfg| cfg.cloudflare.as_ref());
    let default_env = environment
        .clone()
        .or_else(|| cf_cfg.and_then(|cfg| cfg.environment.clone()));

    if env_file.is_none() {
        if let Some(cfg) = cf_cfg {
            if is_1focus_source(cfg.env_source.as_deref()) {
                let env = default_env.unwrap_or_else(|| "production".to_string());
                return guide(&env);
            }
        }
    }

    let defaults = EnvSetupDefaults {
        env_file,
        environment: default_env,
    };

    let Some(result) = run_env_setup(&project_root, defaults)? else {
        return Ok(());
    };

    if !result.apply {
        println!("Env setup canceled.");
        return Ok(());
    }

    let Some(env_file) = result.env_file else {
        println!("No env file selected; nothing to push.");
        return Ok(());
    };

    let content = fs::read_to_string(&env_file)
        .with_context(|| format!("failed to read {}", env_file.display()))?;
    let vars = parse_env_file(&content);

    if vars.is_empty() {
        println!("No env vars found in {}", env_file.display());
        return Ok(());
    }

    if result.selected_keys.is_empty() {
        println!("No keys selected; nothing to push.");
        return Ok(());
    }

    let mut selected = HashMap::new();
    for key in result.selected_keys {
        if let Some(value) = vars.get(&key) {
            selected.insert(key, value.clone());
        }
    }

    if selected.is_empty() {
        println!("No matching keys found in {}", env_file.display());
        return Ok(());
    }

    push_vars(&result.environment, selected)
}

/// List env vars for this project.
fn list(environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!(
        "{}/api/env/{}?environment={}",
        api_url, project, environment
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to 1focus")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        bail!("Project '{}' not found.", project);
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let data: EnvResponse = resp.json().context("failed to parse response")?;

    println!("Project: {}", data.project);
    println!("Environment: {}", data.environment);
    println!("─────────────────────────────");

    if data.env.is_empty() {
        println!("No env vars set.");
        return Ok(());
    }

    let mut keys: Vec<_> = data.env.keys().collect();
    keys.sort();

    for key in keys {
        let value = &data.env[key];
        // Mask the value (show first 4 chars if long enough)
        let masked = if value.len() > 8 {
            format!("{}...", &value[..4])
        } else {
            "****".to_string()
        };

        // Show description if available
        if let Some(desc) = data.descriptions.get(key) {
            println!("  {} = {}  # {}", key, masked, desc);
        } else {
            println!("  {} = {}", key, masked);
        }
    }

    println!();
    println!("{} env var(s)", data.env.len());

    Ok(())
}

/// Set a single env var.
fn set_var(pair: &str, environment: &str, description: Option<&str>) -> Result<()> {
    let (key, value) = pair
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Invalid format. Use KEY=VALUE"))?;

    let key = key.trim();
    let value = value.trim();

    set_project_env_var_internal(key, value, environment, description)
}

pub fn set_project_env_var(
    key: &str,
    value: &str,
    environment: &str,
    description: Option<&str>,
) -> Result<()> {
    set_project_env_var_internal(key, value, environment, description)
}

fn set_project_env_var_internal(
    key: &str,
    value: &str,
    environment: &str,
    description: Option<&str>,
) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    if key.is_empty() {
        bail!("Key cannot be empty");
    }

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/{}", api_url, project);
    let mut vars = HashMap::new();
    vars.insert(key.to_string(), value.to_string());

    let mut body = serde_json::json!({
        "vars": vars,
        "environment": environment,
    });

    // Add description if provided
    if let Some(desc) = description {
        let mut descriptions = HashMap::new();
        descriptions.insert(key.to_string(), desc.to_string());
        body["descriptions"] = serde_json::json!(descriptions);
    }

    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to 1focus")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    let masked = if value.len() > 8 {
        format!("{}...", &value[..4])
    } else {
        "****".to_string()
    };

    if let Some(desc) = description {
        println!("✓ Set {}={} ({}) - {}", key, masked, environment, desc);
    } else {
        println!("✓ Set {}={} ({})", key, masked, environment);
    }

    Ok(())
}

/// Delete env vars.
fn delete_vars(keys: &[String], environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    if keys.is_empty() {
        bail!("No keys specified");
    }

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/{}", api_url, project);
    let body = serde_json::json!({
        "keys": keys,
        "environment": environment,
    });

    let resp = client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(&body)
        .send()
        .context("failed to connect to 1focus")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    println!("✓ Deleted {} key(s) from {}", keys.len(), environment);

    Ok(())
}

/// Show current auth status.
fn status() -> Result<()> {
    let auth = load_auth_config()?;

    println!("1focus Environment Manager");
    println!("─────────────────────────────");

    if let Some(ref token) = auth.token {
        let masked = format!("{}...", &token[..7.min(token.len())]);
        println!("Token: {}", masked);
        println!("API:   {}", get_api_url(&auth));

        if let Ok(project) = get_project_name() {
            println!("Project: {}", project);
        }

        println!();
        println!("Commands:");
        println!("  f env pull    - Fetch env vars");
        println!("  f env push    - Push .env to 1focus");
        println!("  f env guide   - Guided env setup from flow.toml");
        println!("  f env apply   - Apply 1focus envs to Cloudflare");
        println!("  f env setup   - Interactive env setup");
        println!("  f env list    - List env vars");
        println!("  f env set K=V - Set env var");
    } else {
        println!("Status: Not logged in");
        println!();
        println!("Run `f env login` to authenticate.");
    }

    Ok(())
}

/// Parse a .env file into key-value pairs.
pub(crate) fn parse_env_file(content: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse KEY=VALUE
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            // Remove surrounding quotes
            let value = value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value);

            if !key.is_empty() {
                vars.insert(key.to_string(), value.to_string());
            }
        }
    }

    vars
}

/// Fetch env vars from 1focus (personal or project).
fn fetch_env_vars(
    personal: bool,
    environment: &str,
    keys: &[String],
) -> Result<HashMap<String, String>> {
    let auth = load_auth_config()?;
    let token = auth
        .token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let api_url = get_api_url(&auth);
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = if personal {
        if keys.is_empty() {
            format!("{}/api/env/personal", api_url)
        } else {
            format!("{}/api/env/personal?keys={}", api_url, keys.join(","))
        }
    } else {
        let project = get_project_name()?;
        let base = format!("{}/api/env/{}?environment={}", api_url, project, environment);
        if keys.is_empty() {
            base
        } else {
            format!("{}&keys={}", base, keys.join(","))
        }
    };

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to 1focus")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        if personal {
            bail!("Personal env vars not found.");
        } else {
            bail!("Project not found. Create it with `f env push` first.");
        }
    }

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    if personal {
        let data: PersonalEnvResponse = resp.json().context("failed to parse response")?;
        Ok(data.env)
    } else {
        let data: EnvResponse = resp.json().context("failed to parse response")?;
        Ok(data.env)
    }
}

pub fn fetch_project_env_vars(environment: &str, keys: &[String]) -> Result<HashMap<String, String>> {
    fetch_env_vars(false, environment, keys)
}

/// Get specific env vars and print to stdout.
fn get_vars(keys: &[String], personal: bool, environment: &str, format: &str) -> Result<()> {
    let vars = fetch_env_vars(personal, environment, keys)?;

    if vars.is_empty() {
        bail!("No env vars found");
    }

    match format {
        "json" => {
            let json = serde_json::to_string_pretty(&vars)?;
            println!("{}", json);
        }
        "value" => {
            if keys.len() != 1 {
                bail!("'value' format requires exactly one key");
            }
            let key = &keys[0];
            if let Some(value) = vars.get(key) {
                print!("{}", value); // No newline for piping
            } else {
                bail!("Key '{}' not found", key);
            }
        }
        "env" | _ => {
            // Default: KEY=VALUE format
            let mut sorted_keys: Vec<_> = vars.keys().collect();
            sorted_keys.sort();
            for key in sorted_keys {
                let value = &vars[key];
                // Escape for shell
                let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                println!("{}=\"{}\"", key, escaped);
            }
        }
    }

    Ok(())
}

/// Run a command with env vars injected from 1focus.
fn run_with_env(
    personal: bool,
    environment: &str,
    keys: &[String],
    command: &[String],
) -> Result<()> {
    use std::process::Command;

    if command.is_empty() {
        bail!("No command specified");
    }

    let vars = fetch_env_vars(personal, environment, keys)?;

    let (cmd, args) = command.split_first().unwrap();

    let mut child = Command::new(cmd);
    child.args(args);

    // Inject env vars
    for (key, value) in &vars {
        child.env(key, value);
    }

    let status = child.status().with_context(|| format!("failed to run '{}'", cmd))?;

    std::process::exit(status.code().unwrap_or(1));
}
