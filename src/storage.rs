use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{JazzStorageAction, JazzStorageKind, StorageAction, StorageCommand};
use crate::{config, env};

const DEFAULT_JAZZ_API_KEY_MIRROR: &str = "jazz-gitedit-prod";
const DEFAULT_JAZZ_PEER_MIRROR: &str = "wss://cloud.jazz.tools/?key=jazz-gitedit-prod";
const DEFAULT_JAZZ_API_KEY_ENV: &str = "1focus@1focus.app";
const DEFAULT_JAZZ_PEER_ENV: &str = "wss://cloud.jazz.tools/?key=1focus@1focus.app";

#[derive(Debug, Deserialize)]
pub(crate) struct JazzCreateOutput {
    #[serde(rename = "accountID")]
    pub(crate) account_id: String,
    #[serde(rename = "agentSecret")]
    pub(crate) agent_secret: String,
}

pub fn run(cmd: StorageCommand) -> Result<()> {
    match cmd.action {
        StorageAction::Jazz(jazz) => run_jazz(jazz.action),
    }
}

fn run_jazz(action: JazzStorageAction) -> Result<()> {
    match action {
        JazzStorageAction::New {
            kind,
            name,
            peer,
            api_key,
            environment,
        } => jazz_new(kind, name, peer, api_key, &environment),
    }
}

fn jazz_new(
    kind: JazzStorageKind,
    name: Option<String>,
    peer: Option<String>,
    api_key: Option<String>,
    environment: &str,
) -> Result<()> {
    let project = get_project_name()?;
    let default_name = match kind {
        JazzStorageKind::Mirror => format!("{}-jazz-mirror", sanitize_name(&project)),
        JazzStorageKind::EnvStore => format!("{}-jazz-env", sanitize_name(&project)),
    };
    let name = name.unwrap_or(default_name);

    let default_peer = match kind {
        JazzStorageKind::Mirror => DEFAULT_JAZZ_PEER_MIRROR,
        JazzStorageKind::EnvStore => DEFAULT_JAZZ_PEER_ENV,
    };

    let peer = match (peer, api_key.as_deref()) {
        (Some(peer), _) => peer,
        (None, Some(key)) => format!("wss://cloud.jazz.tools/?key={}", key),
        (None, None) => default_peer.to_string(),
    };

    let creds = create_jazz_worker_account(&peer, &name)?;

    match kind {
        JazzStorageKind::Mirror => {
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
        }
        JazzStorageKind::EnvStore => {
            env::set_project_env_var(
                "JAZZ_WORKER_ACCOUNT",
                &creds.account_id,
                environment,
                Some("Jazz worker account ID"),
            )?;
            env::set_project_env_var(
                "JAZZ_WORKER_SECRET",
                &creds.agent_secret,
                environment,
                Some("Jazz worker account secret"),
            )?;
        }
    }

    if api_key.is_some() {
        let key = api_key.unwrap_or_else(|| match kind {
            JazzStorageKind::Mirror => DEFAULT_JAZZ_API_KEY_MIRROR.to_string(),
            JazzStorageKind::EnvStore => DEFAULT_JAZZ_API_KEY_ENV.to_string(),
        });
        env::set_project_env_var(
            "JAZZ_API_KEY",
            &key,
            environment,
            Some("Jazz API key for cloud sync"),
        )?;
    }

    if peer != default_peer {
        let (key, desc) = match kind {
            JazzStorageKind::Mirror => (
                "JAZZ_MIRROR_SYNC_SERVER",
                "Custom Jazz sync server for mirror worker",
            ),
            JazzStorageKind::EnvStore => (
                "JAZZ_SYNC_SERVER",
                "Custom Jazz sync server for env worker",
            ),
        };
        env::set_project_env_var(key, &peer, environment, Some(desc))?;
    }

    println!("✓ Jazz storage initialized for {}", project);
    Ok(())
}

pub(crate) fn create_jazz_worker_account(peer: &str, name: &str) -> Result<JazzCreateOutput> {
    let redacted_peer = redact_peer(peer);
    println!(
        "Creating Jazz worker account '{}' via {} (this can take a minute)...",
        name, redacted_peer
    );

    let output = if let Some(path) = find_in_path("jazz-run") {
        println!(
            "Running: {} account create --peer {} --name {} --json",
            path.display(),
            redacted_peer,
            name
        );
        {
            let mut cmd = Command::new(path);
            cmd.args([
                "account",
                "create",
                "--peer",
                peer,
                "--name",
                name,
                "--json",
            ]);
            run_command_with_output(cmd)
        }
        .context("failed to spawn jazz-run")?
    } else {
        println!(
            "Running: npx --yes jazz-run account create --peer {} --name {} --json",
            redacted_peer, name
        );
        {
            let mut cmd = Command::new("npx");
            cmd.args([
                "--yes",
                "jazz-run",
                "account",
                "create",
                "--peer",
                peer,
                "--name",
                name,
                "--json",
            ]);
            run_command_with_output(cmd)
        }
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

fn run_command_with_output(mut cmd: Command) -> Result<Output> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to capture stderr"))?;

    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let stderr_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let mut next_log = Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let elapsed = start.elapsed();
        if elapsed >= next_log {
            println!(
                "... still creating Jazz worker account ({}s)",
                elapsed.as_secs()
            );
            next_log += Duration::from_secs(10);
        }
        thread::sleep(Duration::from_millis(200));
    };

    let stdout = stdout_handle.join().unwrap_or_default();
    let stderr = stderr_handle.join().unwrap_or_default();

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn redact_peer(peer: &str) -> String {
    if let Some(idx) = peer.find("key=") {
        let start = idx + 4;
        let end = peer[start..]
            .find('&')
            .map(|offset| start + offset)
            .unwrap_or(peer.len());
        let mut redacted = peer.to_string();
        if start < end && end <= redacted.len() {
            redacted.replace_range(start..end, "***");
        }
        return redacted;
    }
    peer.to_string()
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

pub(crate) fn sanitize_name(name: &str) -> String {
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

pub(crate) fn get_project_name() -> Result<String> {
    let cwd = std::env::current_dir()?;
    if let Some(flow_path) = find_flow_toml(&cwd) {
        if let Ok(cfg) = config::load(&flow_path) {
            if let Some(name) = cfg.project_name {
                return Ok(name);
            }
            if let Some(parent) = flow_path.parent() {
                if let Some(dir_name) = parent.file_name().and_then(|n| n.to_str()) {
                    return Ok(dir_name.to_string());
                }
            }
        }
    }

    Ok(cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("flow")
        .to_string())
}
