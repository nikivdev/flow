//! Environment variable management via 1focus.
//!
//! Fetches, sets, and manages environment variables for projects
//! using the 1focus API.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

use crate::cli::EnvAction;
use crate::config;

const DEFAULT_API_URL: &str = "https://1f.nikiv.dev";

/// Auth config stored in ~/.config/flow/auth.toml
#[derive(Debug, Serialize, Deserialize, Default)]
struct AuthConfig {
    token: Option<String>,
    api_url: Option<String>,
}

/// Response from /api/env/:projectName
#[derive(Debug, Deserialize)]
struct EnvResponse {
    env: HashMap<String, String>,
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
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
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

/// Run the env subcommand.
pub fn run(action: Option<EnvAction>) -> Result<()> {
    let action = action.unwrap_or(EnvAction::Status);

    match action {
        EnvAction::Login => login()?,
        EnvAction::Pull { environment } => pull(&environment)?,
        EnvAction::Push { environment } => push(&environment)?,
        EnvAction::List { environment } => list(&environment)?,
        EnvAction::Set { pair, environment } => set_var(&pair, &environment)?,
        EnvAction::Delete { keys, environment } => delete_vars(&keys, &environment)?,
        EnvAction::Status => status()?,
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
    let token = auth.token.as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    println!("Fetching envs for '{}' ({})...", project, environment);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/{}?environment={}", api_url, project, environment);
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .context("failed to connect to 1focus")?;

    if resp.status() == 401 {
        bail!("Unauthorized. Check your token with `f env login`.");
    }

    if resp.status() == 404 {
        bail!("Project '{}' not found. Create it with `f env push` first.", project);
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
    content.push_str(&format!("# Environment: {} (pulled from 1focus)\n", environment));
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
    let auth = load_auth_config()?;
    let token = auth.token.as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

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

    println!("Pushing {} env vars to '{}' ({})...", vars.len(), project, environment);

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

/// List env vars for this project.
fn list(environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth.token.as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let project = get_project_name()?;
    let api_url = get_api_url(&auth);

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let url = format!("{}/api/env/{}?environment={}", api_url, project, environment);
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
        println!("  {} = {}", key, masked);
    }

    println!();
    println!("{} env var(s)", data.env.len());

    Ok(())
}

/// Set a single env var.
fn set_var(pair: &str, environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth.token.as_ref()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run `f env login` first."))?;

    let (key, value) = pair.split_once('=')
        .ok_or_else(|| anyhow::anyhow!("Invalid format. Use KEY=VALUE"))?;

    let key = key.trim();
    let value = value.trim();

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

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        bail!("API error {}: {}", status, body);
    }

    println!("✓ Set {}={} ({})", key, if value.len() > 8 { format!("{}...", &value[..4]) } else { "****".to_string() }, environment);

    Ok(())
}

/// Delete env vars.
fn delete_vars(keys: &[String], environment: &str) -> Result<()> {
    let auth = load_auth_config()?;
    let token = auth.token.as_ref()
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
fn parse_env_file(content: &str) -> HashMap<String, String> {
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
                .strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(value);

            if !key.is_empty() {
                vars.insert(key.to_string(), value.to_string());
            }
        }
    }

    vars
}
