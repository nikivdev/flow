use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use shellexpand::tilde;
use url::Url;
use uuid::Uuid;

use crate::cli::{DbAction, DbCommand, JazzStorageAction, JazzStorageKind, PostgresAction};
use crate::{config, env};

const DEFAULT_JAZZ_API_KEY_MIRROR: &str = "jazz-gitedit-prod";
const DEFAULT_JAZZ_SERVER_MIRROR: &str = "https://cloud.jazz.tools";
const DEFAULT_JAZZ_API_KEY_ENV: &str = "cloud@myflow.sh";
const DEFAULT_JAZZ_SERVER_ENV: &str = "https://cloud.jazz.tools";
const DEFAULT_JAZZ_API_KEY_APP: &str = "cloud@myflow.sh";
const DEFAULT_JAZZ_SERVER_APP: &str = "https://cloud.jazz.tools";
const DEFAULT_POSTGRES_PROJECT: &str = "~/org/la/la/server";

#[derive(Debug, Clone)]
pub(crate) struct JazzCreateOutput {
    pub(crate) account_id: String,
    pub(crate) agent_secret: String,
}

#[derive(Debug, Clone)]
pub(crate) struct JazzAppCredentials {
    pub(crate) app_id: String,
    pub(crate) backend_secret: String,
    pub(crate) admin_secret: String,
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

    for key in [
        "DATABASE_URL",
        "PLANETSCALE_DATABASE_URL",
        "PSCALE_DATABASE_URL",
    ] {
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
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
        })
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
        JazzStorageKind::AppStore => format!("{}-jazz-app", sanitize_name(&project)),
    };
    let name = name.unwrap_or(default_name);

    let default_server_url = match kind {
        JazzStorageKind::Mirror => DEFAULT_JAZZ_SERVER_MIRROR,
        JazzStorageKind::EnvStore => DEFAULT_JAZZ_SERVER_ENV,
        JazzStorageKind::AppStore => DEFAULT_JAZZ_SERVER_APP,
    };

    let (server_url, peer_api_key) = match peer {
        Some(peer) => {
            let api_key = extract_api_key(&peer);
            (normalize_server_url(&peer), api_key)
        }
        None => (default_server_url.to_string(), None),
    };
    let effective_api_key = api_key.or(peer_api_key);

    let creds = create_jazz_app_credentials(&name)?;

    match kind {
        JazzStorageKind::Mirror => {
            env::set_project_env_var(
                "JAZZ_MIRROR_APP_ID",
                &creds.app_id,
                environment,
                Some("Jazz2 mirror app id"),
            )?;
            env::set_project_env_var(
                "JAZZ_MIRROR_BACKEND_SECRET",
                &creds.backend_secret,
                environment,
                Some("Jazz2 mirror backend secret"),
            )?;
            env::set_project_env_var(
                "JAZZ_MIRROR_ADMIN_SECRET",
                &creds.admin_secret,
                environment,
                Some("Jazz2 mirror admin secret"),
            )?;
        }
        JazzStorageKind::EnvStore => {
            env::set_project_env_var(
                "JAZZ_APP_ID",
                &creds.app_id,
                environment,
                Some("Jazz2 app id"),
            )?;
            env::set_project_env_var(
                "JAZZ_BACKEND_SECRET",
                &creds.backend_secret,
                environment,
                Some("Jazz2 backend secret"),
            )?;
            env::set_project_env_var(
                "JAZZ_ADMIN_SECRET",
                &creds.admin_secret,
                environment,
                Some("Jazz2 admin secret"),
            )?;
        }
        JazzStorageKind::AppStore => {
            env::set_project_env_var(
                "JAZZ_APP_APP_ID",
                &creds.app_id,
                environment,
                Some("Jazz2 app-store app id"),
            )?;
            env::set_project_env_var(
                "JAZZ_APP_BACKEND_SECRET",
                &creds.backend_secret,
                environment,
                Some("Jazz2 app-store backend secret"),
            )?;
            env::set_project_env_var(
                "JAZZ_APP_ADMIN_SECRET",
                &creds.admin_secret,
                environment,
                Some("Jazz2 app-store admin secret"),
            )?;
        }
    }

    if effective_api_key.is_some() {
        let key = effective_api_key.unwrap_or_else(|| match kind {
            JazzStorageKind::Mirror => DEFAULT_JAZZ_API_KEY_MIRROR.to_string(),
            JazzStorageKind::EnvStore => DEFAULT_JAZZ_API_KEY_ENV.to_string(),
            JazzStorageKind::AppStore => DEFAULT_JAZZ_API_KEY_APP.to_string(),
        });
        env::set_project_env_var(
            "JAZZ_API_KEY",
            &key,
            environment,
            Some("Jazz2 API key for hosted sync"),
        )?;
    }

    if server_url != default_server_url {
        let (key, desc) = match kind {
            JazzStorageKind::Mirror => (
                "JAZZ_MIRROR_SERVER_URL",
                "Custom Jazz2 server URL for mirror app",
            ),
            JazzStorageKind::EnvStore => ("JAZZ_SERVER_URL", "Custom Jazz2 server URL"),
            JazzStorageKind::AppStore => (
                "JAZZ_APP_SERVER_URL",
                "Custom Jazz2 server URL for app-store app",
            ),
        };
        env::set_project_env_var(key, &server_url, environment, Some(desc))?;
    }

    println!("✓ Jazz2 storage initialized for {}", project);
    Ok(())
}

pub(crate) fn create_jazz_app_credentials(name: &str) -> Result<JazzAppCredentials> {
    println!(
        "Creating Jazz2 app credentials '{}' (this can take a minute)...",
        name
    );

    let output = if let Some(path) = find_in_path("jazz-tools") {
        println!(
            "Running: {} create app --name {}",
            path.display(),
            name
        );
        {
            let mut cmd = Command::new(path);
            cmd.args(["create", "app", "--name", name]);
            run_command_with_output(cmd)
        }
        .context("failed to spawn jazz-tools")?
    } else {
        println!(
            "Running: npx --yes jazz-tools@alpha create app --name {}",
            name
        );
        {
            let mut cmd = Command::new("npx");
            cmd.args(["--yes", "jazz-tools@alpha", "create", "app", "--name", name]);
            run_command_with_output(cmd)
        }
        .context("failed to spawn npx")?
    };

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "jazz2 app create failed: {}{}",
            stdout.trim(),
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let app_id = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("jazz-tools did not return an app id"))?;

    Ok(JazzAppCredentials {
        app_id,
        backend_secret: generate_secret("backend"),
        admin_secret: generate_secret("admin"),
    })
}

pub(crate) fn create_jazz_worker_account(_peer: &str, name: &str) -> Result<JazzCreateOutput> {
    let creds = create_jazz_app_credentials(name)?;
    Ok(JazzCreateOutput {
        account_id: creds.app_id,
        agent_secret: creds.backend_secret,
    })
}

fn run_command_with_output(mut cmd: Command) -> Result<Output> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

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

fn normalize_server_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return DEFAULT_JAZZ_SERVER_ENV.to_string();
    }
    if let Ok(mut parsed) = Url::parse(trimmed) {
        match parsed.scheme() {
            "wss" => {
                let _ = parsed.set_scheme("https");
            }
            "ws" => {
                let _ = parsed.set_scheme("http");
            }
            _ => {}
        }
        parsed.set_query(None);
        parsed.set_fragment(None);
        return parsed.to_string().trim_end_matches('/').to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

fn extract_api_key(value: &str) -> Option<String> {
    let parsed = Url::parse(value).ok()?;
    parsed
        .query_pairs()
        .find_map(|(k, v)| (k == "key").then(|| v.to_string()))
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

fn generate_secret(prefix: &str) -> String {
    format!(
        "{}-{}{}",
        prefix,
        Uuid::new_v4().as_simple(),
        Uuid::new_v4().as_simple()
    )
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
