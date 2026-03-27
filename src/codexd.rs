use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ai, ai_project_manifest, codex_session_docs, config, daemon, supervisor, sync_plan};

const CODEXD_NAME: &str = "codexd";

#[cfg(unix)]
#[derive(Debug)]
struct FileLockGuard {
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
impl Drop for FileLockGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.fd, libc::LOCK_UN) };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexdRequest {
    Ping,
    Recent {
        target_path: String,
        exact_cwd: bool,
        limit: usize,
        query: Option<String>,
    },
    SessionHint {
        session_hint: String,
        limit: usize,
    },
    Find {
        target_path: Option<String>,
        exact_cwd: bool,
        query: String,
        limit: usize,
        #[serde(default)]
        scope: ai::CodexFindScope,
    },
    ProjectAiManifest {
        target_path: String,
        refresh: bool,
    },
    ProjectAiRecent {
        limit: usize,
    },
    RecentSyncPlans {
        repo_root: String,
        limit: usize,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct CodexdResponse {
    ok: bool,
    message: Option<String>,
    #[serde(default)]
    rows: Vec<ai::CodexRecoverRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<Value>,
}

pub fn builtin_daemon_config() -> Result<config::DaemonConfig> {
    let exe = std::env::current_exe().context("failed to resolve current executable for codexd")?;
    Ok(config::DaemonConfig {
        name: CODEXD_NAME.to_string(),
        binary: exe.display().to_string(),
        command: Some("codex".to_string()),
        args: vec![
            "daemon".to_string(),
            "serve".to_string(),
            "--socket".to_string(),
            socket_path()?.display().to_string(),
        ],
        health_url: None,
        health_socket: Some(socket_path()?.display().to_string()),
        port: None,
        host: None,
        working_dir: None,
        env: Default::default(),
        autostart: false,
        autostop: false,
        boot: false,
        restart: Some(config::DaemonRestartPolicy::Always),
        retry: None,
        ready_delay: Some(100),
        ready_output: None,
        description: Some("Flow-managed Codex query daemon".to_string()),
    })
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?.join("codexd.sock"))
}

fn lock_path() -> Result<PathBuf> {
    Ok(config::ensure_global_state_dir()?.join("codexd.lock"))
}

#[cfg(unix)]
fn acquire_process_lock(file: &std::fs::File) -> Result<FileLockGuard> {
    let fd = file.as_raw_fd();
    let status = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if status == 0 {
        return Ok(FileLockGuard { fd });
    }
    let err = std::io::Error::last_os_error();
    let raw = err.raw_os_error();
    if raw == Some(libc::EWOULDBLOCK) || raw == Some(libc::EAGAIN) {
        bail!("codexd already holds {}", lock_path()?.display());
    }
    Err(err).context("failed to lock codexd process lock")
}

#[cfg(not(unix))]
fn acquire_process_lock(_file: &std::fs::File) -> Result<()> {
    Ok(())
}

pub fn ping() -> Result<()> {
    let response = send_request(&CodexdRequest::Ping)?;
    if response.ok {
        Ok(())
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd ping failed".to_string())
        )
    }
}

pub fn is_running() -> bool {
    ping().is_ok()
}

pub fn ensure_running() -> Result<()> {
    if is_running() {
        return Ok(());
    }
    supervisor::ensure_daemon_running(CODEXD_NAME, None, false)
}

pub fn start() -> Result<()> {
    supervisor::ensure_daemon_running(CODEXD_NAME, None, true)
}

pub fn stop() -> Result<()> {
    supervisor::stop_daemon_managed(CODEXD_NAME, None, true)
}

pub fn status() -> Result<()> {
    daemon::show_status_for_with_path(CODEXD_NAME, None)
}

pub fn serve(socket_override: Option<&Path>) -> Result<()> {
    let socket = socket_override.map(PathBuf::from).unwrap_or(socket_path()?);
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let lock_path = lock_path()?;
    let mut lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    let _process_lock = acquire_process_lock(&lock_file)?;
    lock_file
        .set_len(0)
        .with_context(|| format!("failed to reset {}", lock_path.display()))?;
    writeln!(lock_file, "{}", std::process::id())
        .with_context(|| format!("failed to write {}", lock_path.display()))?;
    lock_file
        .flush()
        .with_context(|| format!("failed to flush {}", lock_path.display()))?;
    if socket.exists() {
        fs::remove_file(&socket)
            .with_context(|| format!("failed to remove stale socket {}", socket.display()))?;
    }

    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("failed to bind codexd socket {}", socket.display()))?;

    start_background_maintenance_loop();

    loop {
        let (stream, _) = match listener.accept() {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("WARN codexd accept failed: {err}");
                continue;
            }
        };
        if let Err(err) = handle_client(stream) {
            eprintln!("WARN codexd request failed: {err:#}");
        }
    }
}

fn background_poll_secs() -> u64 {
    std::env::var("FLOW_CODEXD_BACKGROUND_POLL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(5, 300))
        .unwrap_or(20)
}

fn background_maintenance_interval_secs(env_key: &str, default_secs: u64) -> u64 {
    std::env::var(env_key)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| value.clamp(30, 3600))
        .unwrap_or(default_secs)
}

fn start_background_maintenance_loop() {
    let poll_secs = background_poll_secs();
    let docs_review_every = Duration::from_secs(background_maintenance_interval_secs(
        "FLOW_CODEXD_DOC_REVIEW_EVERY_SECS",
        300,
    ));
    let project_ai_refresh_every = Duration::from_secs(background_maintenance_interval_secs(
        "FLOW_CODEXD_PROJECT_AI_REFRESH_EVERY_SECS",
        600,
    ));
    let _ = thread::Builder::new()
        .name("flow-codexd-maint".to_string())
        .spawn(move || {
            let mut last_docs_review = Instant::now()
                .checked_sub(docs_review_every)
                .unwrap_or_else(Instant::now);
            let mut last_project_ai_refresh = Instant::now()
                .checked_sub(project_ai_refresh_every)
                .unwrap_or_else(Instant::now);
            loop {
                if let Err(err) = ai::run_codex_background_maintenance() {
                    eprintln!("WARN codexd maintenance failed: {err:#}");
                }
                if let Err(err) = ai::maybe_run_codex_learning_refresh() {
                    eprintln!("WARN codexd learning refresh failed: {err:#}");
                }
                if let Err(err) = ai::maybe_run_codex_telemetry_export(200) {
                    eprintln!("WARN codexd telemetry export failed: {err:#}");
                }
                if let Err(err) = sync_plan::drain_queued_requests(1) {
                    eprintln!("WARN codexd sync plan queue drain failed: {err:#}");
                }
                if last_docs_review.elapsed() >= docs_review_every {
                    if let Err(err) = codex_session_docs::review_pending_entries(12) {
                        eprintln!("WARN codexd docs review pass failed: {err:#}");
                    }
                    last_docs_review = Instant::now();
                }
                if last_project_ai_refresh.elapsed() >= project_ai_refresh_every {
                    if let Err(err) = ai_project_manifest::refresh_recent(12) {
                        eprintln!("WARN codexd project-ai refresh failed: {err:#}");
                    }
                    last_project_ai_refresh = Instant::now();
                }
                thread::sleep(Duration::from_secs(poll_secs));
            }
        });
}

pub(crate) fn query_recent(
    target_path: &Path,
    exact_cwd: bool,
    limit: usize,
    query: Option<&str>,
) -> Result<Vec<ai::CodexRecoverRow>> {
    ensure_running()?;
    let response = send_request(&CodexdRequest::Recent {
        target_path: target_path.display().to_string(),
        exact_cwd,
        limit,
        query: query.map(str::to_string),
    })?;
    if response.ok {
        Ok(response.rows)
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd recent query failed".to_string())
        )
    }
}

pub(crate) fn query_session_hint(
    session_hint: &str,
    limit: usize,
) -> Result<Vec<ai::CodexRecoverRow>> {
    ensure_running()?;
    let response = send_request(&CodexdRequest::SessionHint {
        session_hint: session_hint.to_string(),
        limit,
    })?;
    if response.ok {
        Ok(response.rows)
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd session hint query failed".to_string())
        )
    }
}

pub(crate) fn query_find(
    target_path: Option<&Path>,
    exact_cwd: bool,
    query: &str,
    limit: usize,
    scope: ai::CodexFindScope,
) -> Result<Vec<ai::CodexRecoverRow>> {
    ensure_running()?;
    let response = send_request_with_restart(CodexdRequest::Find {
        target_path: target_path.map(|path| path.display().to_string()),
        exact_cwd,
        query: query.to_string(),
        limit,
        scope,
    })?;
    if response.ok {
        Ok(response.rows)
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd find query failed".to_string())
        )
    }
}

pub(crate) fn query_project_ai_manifest(
    target_path: &Path,
    refresh: bool,
) -> Result<ai_project_manifest::AiProjectManifest> {
    ensure_running()?;
    let response = send_request(&CodexdRequest::ProjectAiManifest {
        target_path: target_path.display().to_string(),
        refresh,
    })?;
    if response.ok {
        let payload = response
            .payload
            .context("codexd project-ai manifest response was missing payload")?;
        serde_json::from_value(payload).context("failed to decode codexd project-ai manifest")
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd project-ai manifest query failed".to_string())
        )
    }
}

pub(crate) fn query_recent_project_ai(
    limit: usize,
) -> Result<Vec<ai_project_manifest::AiProjectManifest>> {
    ensure_running()?;
    let response = send_request(&CodexdRequest::ProjectAiRecent { limit })?;
    if response.ok {
        let payload = response
            .payload
            .context("codexd project-ai recent response was missing payload")?;
        serde_json::from_value(payload).context("failed to decode codexd project-ai recent list")
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd project-ai recent query failed".to_string())
        )
    }
}

pub(crate) fn recent_sync_plans(
    repo_root: &Path,
    limit: usize,
) -> Result<Vec<sync_plan::SyncPlanRunRecord>> {
    ensure_running()?;
    let response = send_request_with_restart(CodexdRequest::RecentSyncPlans {
        repo_root: repo_root.display().to_string(),
        limit,
    })?;
    if response.ok {
        let payload = response
            .payload
            .context("codexd recent sync plans response was missing payload")?;
        serde_json::from_value(payload).context("failed to decode codexd recent sync plans")
    } else {
        bail!(
            "{}",
            response
                .message
                .unwrap_or_else(|| "codexd recent sync plans query failed".to_string())
        )
    }
}

fn send_request(request: &CodexdRequest) -> Result<CodexdResponse> {
    let mut stream =
        UnixStream::connect(socket_path()?).context("failed to connect to codexd socket")?;
    let payload = serde_json::to_string(request).context("failed to encode codexd request")?;
    stream
        .write_all(payload.as_bytes())
        .context("failed to write codexd request")?;
    stream
        .write_all(b"\n")
        .context("failed to terminate codexd request")?;
    stream.flush().context("failed to flush codexd request")?;

    let mut reader = BufReader::new(stream);
    let mut line = Vec::with_capacity(1024);
    reader
        .read_until(b'\n', &mut line)
        .context("failed to read codexd response")?;
    let trimmed = trim_ascii_whitespace(&line);
    if trimmed.is_empty() {
        bail!("codexd returned an empty response");
    }
    serde_json::from_slice(trimmed).context("failed to decode codexd response")
}

fn send_request_with_restart(request: CodexdRequest) -> Result<CodexdResponse> {
    match send_request(&request) {
        Ok(response) => Ok(response),
        Err(initial_err) => {
            let _ = stop();
            start().context("failed to restart codexd for upgraded request")?;
            send_request(&request)
                .with_context(|| format!("codexd request failed before restart: {initial_err:#}"))
        }
    }
}

fn handle_client(stream: UnixStream) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = Vec::with_capacity(1024);
    reader.read_until(b'\n', &mut line)?;
    let trimmed = trim_ascii_whitespace(&line);
    if trimmed.is_empty() {
        return Ok(());
    }

    let request: CodexdRequest =
        serde_json::from_slice(trimmed).context("failed to decode codexd request")?;
    let response = handle_request(request);

    let mut writer = &stream;
    let payload = serde_json::to_string(&response).context("failed to encode codexd response")?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn handle_request(request: CodexdRequest) -> CodexdResponse {
    match request {
        CodexdRequest::Ping => CodexdResponse {
            ok: true,
            message: Some("pong".to_string()),
            rows: vec![],
            payload: None,
        },
        CodexdRequest::Recent {
            target_path,
            exact_cwd,
            limit,
            query,
        } => match ai::read_recent_codex_threads_local(
            Path::new(&target_path),
            exact_cwd,
            limit,
            query.as_deref(),
        ) {
            Ok(rows) => CodexdResponse {
                ok: true,
                message: None,
                rows,
                payload: None,
            },
            Err(err) => CodexdResponse {
                ok: false,
                message: Some(format!("{err:#}")),
                rows: vec![],
                payload: None,
            },
        },
        CodexdRequest::SessionHint {
            session_hint,
            limit,
        } => match ai::read_codex_threads_by_session_hint_local(&session_hint, limit) {
            Ok(rows) => CodexdResponse {
                ok: true,
                message: None,
                rows,
                payload: None,
            },
            Err(err) => CodexdResponse {
                ok: false,
                message: Some(format!("{err:#}")),
                rows: vec![],
                payload: None,
            },
        },
        CodexdRequest::Find {
            target_path,
            exact_cwd,
            query,
            limit,
            scope,
        } => match ai::search_codex_threads_for_find_local(
            target_path.as_deref().map(Path::new),
            exact_cwd,
            &query,
            limit,
            scope,
        ) {
            Ok(rows) => CodexdResponse {
                ok: true,
                message: None,
                rows,
                payload: None,
            },
            Err(err) => CodexdResponse {
                ok: false,
                message: Some(format!("{err:#}")),
                rows: vec![],
                payload: None,
            },
        },
        CodexdRequest::ProjectAiManifest {
            target_path,
            refresh,
        } => match ai_project_manifest::load_for_target(Path::new(&target_path), refresh) {
            Ok(manifest) => CodexdResponse {
                ok: true,
                message: None,
                rows: vec![],
                payload: Some(serde_json::json!(manifest)),
            },
            Err(err) => CodexdResponse {
                ok: false,
                message: Some(format!("{err:#}")),
                rows: vec![],
                payload: None,
            },
        },
        CodexdRequest::ProjectAiRecent { limit } => match ai_project_manifest::recent(limit) {
            Ok(manifests) => CodexdResponse {
                ok: true,
                message: None,
                rows: vec![],
                payload: Some(serde_json::json!(manifests)),
            },
            Err(err) => CodexdResponse {
                ok: false,
                message: Some(format!("{err:#}")),
                rows: vec![],
                payload: None,
            },
        },
        CodexdRequest::RecentSyncPlans { repo_root, limit } => {
            match sync_plan::recent_runs(Path::new(&repo_root), limit) {
                Ok(runs) => CodexdResponse {
                    ok: true,
                    message: None,
                    rows: vec![],
                    payload: Some(serde_json::json!(runs)),
                },
                Err(err) => CodexdResponse {
                    ok: false,
                    message: Some(format!("{err:#}")),
                    rows: vec![],
                    payload: None,
                },
            }
        }
    }
}

#[inline]
fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let mut start = 0usize;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn builtin_daemon_config_uses_socket_health() {
        let cfg = builtin_daemon_config().expect("builtin codexd config");
        let socket = socket_path().expect("codexd socket");

        assert_eq!(cfg.name, CODEXD_NAME);
        assert_eq!(cfg.command.as_deref(), Some("codex"));
        assert_eq!(
            cfg.effective_health_socket().as_deref(),
            Some(socket.as_path())
        );
        let expected_label = format!("unix:{}", socket.display());
        assert_eq!(
            cfg.health_target_label().as_deref(),
            Some(expected_label.as_str())
        );
        assert!(
            cfg.args
                .windows(2)
                .any(|window| window == ["daemon", "serve"])
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_lock_rejects_second_holder() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("codexd.lock");
        let first = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("first lock file");
        let _guard = acquire_process_lock(&first).expect("first lock");

        let second = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .expect("second lock file");
        let err = acquire_process_lock(&second).expect_err("second lock should fail");
        assert!(format!("{err:#}").contains("codexd already holds"));
    }
}
