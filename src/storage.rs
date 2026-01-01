use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{JazzStorageAction, StorageAction, StorageCommand};
use crate::{config, env};

const DEFAULT_JAZZ_API_KEY: &str = "jazz-gitedit-prod";
const DEFAULT_JAZZ_PEER: &str = "wss://cloud.jazz.tools/?key=jazz-gitedit-prod";

#[derive(Debug, Deserialize)]
struct JazzCreateOutput {
    #[serde(rename = "accountID")]
    account_id: String,
    #[serde(rename = "agentSecret")]
    agent_secret: String,
}

pub fn run(cmd: StorageCommand) -> Result<()> {
    match cmd.action {
        StorageAction::Jazz(jazz) => run_jazz(jazz.action),
    }
}

fn run_jazz(action: JazzStorageAction) -> Result<()> {
    match action {
        JazzStorageAction::New {
            name,
            peer,
            api_key,
            environment,
        } => jazz_new(name, peer, api_key, &environment),
    }
}

fn jazz_new(
    name: Option<String>,
    peer: Option<String>,
    api_key: Option<String>,
    environment: &str,
) -> Result<()> {
    let project = get_project_name()?;
    let default_name = format!("{}-jazz-mirror", sanitize_name(&project));
    let name = name.unwrap_or(default_name);

    let peer = match (peer, api_key.as_deref()) {
        (Some(peer), _) => peer,
        (None, Some(key)) => format!("wss://cloud.jazz.tools/?key={}", key),
        (None, None) => DEFAULT_JAZZ_PEER.to_string(),
    };

    let creds = create_jazz_worker_account(&peer, &name)?;

    env::set_project_env_var(
        "JAZZ_MIRROR_ACCOUNT_ID",
        &creds.account_id,
        environment,
        Some("Jazz mirror worker account ID"),
    )?;
    env::set_project_env_var(
        "JAZZ_MIRROR_ACCOUNT_SECRET",
        &creds.agent_secret,
        environment,
        Some("Jazz mirror worker account secret"),
    )?;

    if api_key.is_some() {
        let key = api_key.unwrap_or_else(|| DEFAULT_JAZZ_API_KEY.to_string());
        env::set_project_env_var(
            "JAZZ_API_KEY",
            &key,
            environment,
            Some("Jazz API key for cloud sync"),
        )?;
    }

    if peer != DEFAULT_JAZZ_PEER {
        env::set_project_env_var(
            "JAZZ_MIRROR_SYNC_SERVER",
            &peer,
            environment,
            Some("Custom Jazz sync server for mirror worker"),
        )?;
    }

    println!("✓ Jazz storage initialized for {}", project);
    Ok(())
}

fn create_jazz_worker_account(peer: &str, name: &str) -> Result<JazzCreateOutput> {
    let output = if let Some(path) = find_in_path("jazz-run") {
        Command::new(path)
            .args([
                "account",
                "create",
                "--peer",
                peer,
                "--name",
                name,
                "--json",
            ])
            .output()
            .context("failed to spawn jazz-run")?
    } else {
        Command::new("npx")
            .args([
                "--yes",
                "jazz-run",
                "account",
                "create",
                "--peer",
                peer,
                "--name",
                name,
                "--json",
            ])
            .output()
            .context("failed to spawn npx")?
    };

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "jazz account create failed: {}{}",
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = extract_json_object(&stdout)
        .ok_or_else(|| anyhow::anyhow!("jazz-run did not return JSON output"))?;
    let creds: JazzCreateOutput =
        serde_json::from_str(json).context("failed to parse jazz-run JSON output")?;

    Ok(creds)
}

fn extract_json_object(output: &str) -> Option<&str> {
    let start = output.find('{')?;
    let end = output.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(output[start..=end].trim())
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "flow-jazz-mirror".to_string()
    } else {
        trimmed
    }
}

fn get_project_name() -> Result<String> {
    let cwd = std::env::current_dir()?;
    let flow_toml = cwd.join("flow.toml");

    if flow_toml.exists() {
        if let Ok(cfg) = config::load(&flow_toml) {
            if let Some(name) = cfg.project_name {
                return Ok(name);
            }
        }
    }

    Ok(cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("flow")
        .to_string())
}
