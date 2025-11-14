use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;

use crate::{
    cli::{SecretsAction, SecretsCommand, SecretsFormat, SecretsListOpts, SecretsPullOpts},
    config::{self, Config, StorageConfig, StorageEnvConfig},
};

pub fn run(cmd: SecretsCommand) -> Result<()> {
    match cmd.action {
        SecretsAction::List(opts) => list(opts),
        SecretsAction::Pull(opts) => pull(opts),
    }
}

fn list(opts: SecretsListOpts) -> Result<()> {
    let (config_path, cfg) = load_config(opts.config)?;
    let secrets = cfg.storage.ok_or_else(|| {
        anyhow::anyhow!("no [storage] block defined in {}", config_path.display())
    })?;

    if secrets.envs.is_empty() {
        println!(
            "No secret environments defined in {}",
            config_path.display()
        );
        return Ok(());
    }

    println!(
        "Environments defined in {} (provider: {}):",
        config_path.display(),
        secrets.provider
    );
    for env_cfg in &secrets.envs {
        println!("\n- {}", env_cfg.name);
        if let Some(desc) = &env_cfg.description {
            println!("  Description: {}", desc);
        }
        if env_cfg.variables.is_empty() {
            println!("  Variables: (unspecified)");
        } else {
            let summary: Vec<String> = env_cfg
                .variables
                .iter()
                .map(|var| match &var.default {
                    Some(default) if !default.is_empty() => {
                        format!("{} (default: {})", var.key, default)
                    }
                    Some(_) => format!("{} (default: empty)", var.key),
                    None => var.key.clone(),
                })
                .collect();
            println!("  Variables: {}", summary.join(", "));
        }
    }

    Ok(())
}

fn pull(opts: SecretsPullOpts) -> Result<()> {
    let (config_path, cfg) = load_config(opts.config)?;
    let secrets = cfg.storage.ok_or_else(|| {
        anyhow::anyhow!("no [storage] block defined in {}", config_path.display())
    })?;

    let env_cfg = secrets
        .envs
        .iter()
        .find(|env| env.name == opts.env)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "unknown storage environment '{}' (available: {})",
                opts.env,
                secrets
                    .envs
                    .iter()
                    .map(|env| env.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let values = fetch_remote_secrets(&secrets, env_cfg, opts.hub.clone())?;
    let ordered = order_variables(env_cfg, &values);
    let rendered = render_secrets(&ordered, opts.format);

    if let Some(path) = opts.output {
        write_output(&path, &rendered)?;
        println!("Saved {} secrets to {}", env_cfg.name, path.display());
    } else {
        println!("{}", rendered);
    }

    Ok(())
}

fn fetch_remote_secrets(
    cfg: &StorageConfig,
    env_cfg: &StorageEnvConfig,
    hub_override: Option<String>,
) -> Result<HashMap<String, String>> {
    let api_key = env::var(&cfg.env_var).with_context(|| {
        format!(
            "environment variable {} is not set; required to authenticate with secrets provider",
            cfg.env_var
        )
    })?;

    let base_url = hub_override
        .or_else(|| Some(cfg.hub_url.clone()))
        .unwrap_or_else(|| "https://flow.1focus.ai".to_string());
    let base = base_url.trim_end_matches('/');
    let url = format!("{}/api/secrets/{}/{}", base, cfg.provider, env_cfg.name);

    let client = Client::builder()
        .build()
        .context("failed to build HTTP client")?;
    let response = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .with_context(|| "failed to call storage hub")?
        .error_for_status()
        .with_context(|| "storage hub returned an error response")?;

    let body: HashMap<String, String> = response
        .json()
        .with_context(|| "failed to parse storage hub response")?;

    for var in &env_cfg.variables {
        if !body.contains_key(var) {
            bail!(
                "storage hub response missing required variable '{}' for environment '{}'",
                var,
                env_cfg.name
            );
        }
    }

    Ok(body)
}

fn order_variables(
    env_cfg: &StorageEnvConfig,
    values: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut ordered = Vec::new();
    for key in &env_cfg.variables {
        if let Some(value) = values.get(key) {
            ordered.push((key.clone(), value.clone()));
        }
    }
    for (key, value) in values {
        if env_cfg.variables.iter().any(|v| v == key) {
            continue;
        }
        ordered.push((key.clone(), value.clone()));
    }
    ordered
}

fn render_secrets(vars: &[(String, String)], format: SecretsFormat) -> String {
    match format {
        SecretsFormat::Shell => vars
            .iter()
            .map(|(k, v)| format!("export {}={}", k, shell_quote(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        SecretsFormat::Dotenv => vars
            .iter()
            .map(|(k, v)| format!("{}={}", k, dotenv_quote(v)))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        "''".to_string()
    } else if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
    {
        value.to_string()
    } else {
        let escaped = value.replace('\'', "'\\''");
        format!("'{}'", escaped)
    }
}

fn dotenv_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'.' | b'-' | b'/'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn write_output(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    fs::write(path, contents.as_bytes())
        .with_context(|| format!("failed to write secrets to {}", path.display()))?;
    Ok(())
}

fn load_config(path: PathBuf) -> Result<(PathBuf, Config)> {
    let config_path = resolve_path(path)?;
    let cfg = config::load(&config_path).with_context(|| {
        format!(
            "failed to load configuration from {}",
            config_path.display()
        )
    })?;
    Ok((config_path, cfg))
}

fn resolve_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}
