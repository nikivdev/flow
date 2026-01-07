use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use shellexpand::tilde;

use crate::cli::{
    DbAction, DbCommand, JazzStorageAction, JazzStorageKind, PostgresAction,
};
use crate::{config, env};

const DEFAULT_JAZZ_API_KEY_MIRROR: &str = "jazz-gitedit-prod";
const DEFAULT_JAZZ_PEER_MIRROR: &str = "wss://cloud.jazz.tools/?key=jazz-gitedit-prod";
const DEFAULT_JAZZ_API_KEY_ENV: &str = "1focus@1focus.app";
const DEFAULT_JAZZ_PEER_ENV: &str = "wss://cloud.jazz.tools/?key=1focus@1focus.app";
const DEFAULT_POSTGRES_PROJECT: &str = "~/org/la/la/server";

#[derive(Debug, Deserialize)]
pub(crate) struct JazzCreateOutput {
    #[serde(rename = "accountID")]
    pub(crate) account_id: String,
    #[serde(rename = "agentSecret")]
    pub(crate) agent_secret: String,
}

pub fn run(cmd: DbCommand) -> Result<()> {
    match cmd.action {
        DbAction::Jazz(jazz) => run_jazz(jazz.action),
        DbAction::Postgres(pg) => run_postgres(pg.action),
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

fn run_postgres(action: PostgresAction) -> Result<()> {
    match action {
        PostgresAction::Generate { project } => postgres_generate(project),
        PostgresAction::Migrate {
            project,
            database_url,
            generate,
        } => postgres_migrate(project, database_url, generate),
    }
}

fn postgres_generate(project: Option<PathBuf>) -> Result<()> {
    let project_dir = resolve_postgres_project(project)?;
    println!("Running migrations generate in {}", project_dir.display());
    run_bun_script(&project_dir, "db:generate", None)
}

fn postgres_migrate(
    project: Option<PathBuf>,
    database_url: Option<String>,
    generate: bool,
) -> Result<()> {
    let project_dir = resolve_postgres_project(project)?;
    let database_url = resolve_database_url(database_url.as_deref(), &project_dir)?;

    if generate {
        println!("Generating migrations in {}", project_dir.display());
        run_bun_script(&project_dir, "db:generate", Some(&database_url))?;
    }

    println!("Applying migrations in {}", project_dir.display());
    run_bun_script(&project_dir, "db:migrate", Some(&database_url))
}

fn resolve_postgres_project(project: Option<PathBuf>) -> Result<PathBuf> {
    let path = match project {
        Some(path) => path,
        None => PathBuf::from(tilde(DEFAULT_POSTGRES_PROJECT).as_ref()),
    };

    if path.exists() {
        return Ok(path);
    }

    bail!(
        "Postgres project path not found: {} (override with --project)",
        path.display()
    )
}

fn resolve_database_url(database_url: Option<&str>, project_dir: &Path) -> Result<String> {
    if let Some(url) = database_url {
        let trimmed = url.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    for key in ["DATABASE_URL", "PLANETSCALE_DATABASE_URL", "PSCALE_DATABASE_URL"] {
        if let Ok(url) = std::env::var(key) {
            if !url.trim().is_empty() {
                return Ok(url);
            }
        }
    }

    let env_path = project_dir.join(".env");
    if let Some(value) = read_env_value(&env_path, "DATABASE_URL")? {
        return Ok(value);
    }

    bail!(
        "DATABASE_URL not found (set env, PLANETSCALE_DATABASE_URL, or add it to {})",
        env_path.display()
    )
}

fn read_env_value(path: &Path, key: &str) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read env file {}", path.display()))?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let value = strip_quotes(value.trim());
        if !value.is_empty() {
            return Ok(Some(value.to_string()));
        }
    }
    Ok(None)
}

fn strip_quotes(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| trimmed.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(trimmed)
}

fn run_bun_script(project_dir: &Path, script: &str, database_url: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("bun");
    cmd.args(["run", script]);
    cmd.current_dir(project_dir);
    if let Some(url) = database_url {
        cmd.env("DATABASE_URL", url);
    }
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    let status = cmd.status().with_context(|| {
        format!(
            "failed to run bun script '{}' in {}",
            script,
            project_dir.display()
        )
    })?;
    if !status.success() {
        bail!("bun run {} failed with status {}", script, status);
    }
    Ok(())
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
