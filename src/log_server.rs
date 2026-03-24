use std::fs;
use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{Json as AxumJson, Path as AxumPath, Query, State},
    http::{Method, StatusCode},
    response::{
        IntoResponse, Json,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::stream::{self, Stream, StreamExt};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::cli::{ServerAction, ServerOpts};
use crate::log_store::{self, LogEntry, LogQuery};
use crate::pr_edit::PrEditService;
use crate::{
    ai, config, daemon_snapshot, explain_commits, ops_overview, projects, skills, workflow,
};

#[derive(Clone)]
struct AppState {
    pr_edit: Arc<tokio::sync::RwLock<Option<Arc<PrEditService>>>>,
    pr_edit_error: Arc<tokio::sync::RwLock<Option<String>>>,
}

/// Run the flow HTTP server for log ingestion.
pub fn run(opts: ServerOpts) -> Result<()> {
    let host = opts.host.clone();
    let port = opts.port;

    match opts.action {
        Some(ServerAction::Stop) => stop_server(),
        Some(ServerAction::Foreground) => run_foreground(&host, port),
        None => ensure_server(&host, port),
    }
}

/// Ensure server is running in background, start if not
fn ensure_server(host: &str, port: u16) -> Result<()> {
    if server_healthy(host, port) {
        println!("Flow server already running at http://{}:{}", host, port);
        return Ok(());
    }

    // Kill stale process if exists
    if let Some(pid) = load_server_pid()? {
        if process_alive(pid) {
            terminate_process(pid).ok();
        }
        remove_server_pid().ok();
    }

    // Start in background
    let exe = std::env::current_exe().context("failed to get current exe")?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(server_stdout_log_path())
        .context("failed to open server stdout log")?;
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(server_stderr_log_path())
        .context("failed to open server stderr log")?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string())
        .arg("foreground")
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let child = cmd.spawn().context("failed to start server process")?;
    persist_server_pid(child.id())?;

    // Wait for health
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if server_healthy(host, port) {
            println!("Flow server started at http://{}:{}", host, port);
            return Ok(());
        }
    }

    println!(
        "Flow server starting at http://{}:{} (may take a moment)",
        host, port
    );
    Ok(())
}

/// Run server in foreground (used by background process)
fn run_foreground(host: &str, port: u16) -> Result<()> {
    // Initialize database and schema on startup
    let conn = log_store::open_log_db().context("failed to initialize log database")?;
    drop(conn);

    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .context("invalid host:port")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    rt.block_on(async {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers(Any);

        // Start PR edit watcher in the background so it can never block the server startup.
        let pr_edit = Arc::new(tokio::sync::RwLock::new(None));
        let pr_edit_error = Arc::new(tokio::sync::RwLock::new(None));
        {
            let pr_edit = Arc::clone(&pr_edit);
            let pr_edit_error = Arc::clone(&pr_edit_error);
            tokio::spawn(async move {
                match PrEditService::start().await {
                    Ok(svc) => {
                        *pr_edit.write().await = Some(svc);
                        *pr_edit_error.write().await = None;
                        tracing::info!("pr-edit watcher started");
                    }
                    Err(err) => {
                        *pr_edit_error.write().await = Some(format!("{err:#}"));
                        tracing::warn!(?err, "failed to start pr-edit watcher");
                    }
                }
            });
        }
        let state = AppState {
            pr_edit,
            pr_edit_error,
        };

        let router = Router::new()
            .route("/health", get(health))
            .route("/codex/skills", get(codex_skills))
            .route("/codex/project-ai", get(codex_project_ai))
            .route("/codex/project-ai/recent", get(codex_project_ai_recent))
            .route("/codex/eval", get(codex_eval))
            .route("/codex/resolve", post(codex_resolve))
            .route("/codex/skills/sync", post(codex_skills_sync))
            .route("/codex/skills/reload", post(codex_skills_reload))
            .route("/daemons", get(daemons))
            .route("/daemons/stale/cleanup", post(daemon_cleanup_stale))
            .route("/daemons/{name}/start", post(daemon_start))
            .route("/daemons/{name}/stop", post(daemon_stop))
            .route("/daemons/{name}/restart", post(daemon_restart))
            .route("/ops/overview", get(ops_visibility_overview))
            .route("/logs/ingest", post(logs_ingest))
            .route("/logs/query", get(logs_query))
            .route("/logs/errors/stream", get(logs_errors_stream))
            .route("/pr-edit/status", get(pr_edit_status))
            .route("/pr-edit/rescan", post(pr_edit_rescan))
            // Flow projects + AI sessions
            .route("/projects", get(projects_list_all))
            .route("/projects/{name}/sessions", get(project_sessions))
            .route("/sessions/{id}", get(session_detail))
            .route("/workflow/overview", get(workflow_overview))
            .route(
                "/projects/{name}/commit-explanations",
                get(project_commit_explanations),
            )
            .route(
                "/projects/{name}/commit-explanations/{sha}",
                get(project_commit_explanation_detail),
            )
            .layer(cors)
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("failed to bind server")?;

        axum::serve(listener, router)
            .await
            .context("server error")?;

        Ok(())
    })
}

fn stop_server() -> Result<()> {
    if let Some(pid) = load_server_pid()? {
        terminate_process(pid).ok();
        remove_server_pid().ok();
        println!("Flow server stopped");
    } else {
        println!("Flow server not running");
    }
    Ok(())
}

fn server_pid_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/server.pid")
}

fn server_stdout_log_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/server.stdout.log")
}

fn server_stderr_log_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow/server.stderr.log")
}

fn load_server_pid() -> Result<Option<u32>> {
    let path = server_pid_path();
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)?;
    let pid: u32 = contents.trim().parse().unwrap_or(0);
    Ok(if pid == 0 { None } else { Some(pid) })
}

fn persist_server_pid(pid: u32) -> Result<()> {
    let path = server_pid_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, pid.to_string())?;
    Ok(())
}

fn remove_server_pid() -> Result<()> {
    let path = server_pid_path();
    if path.exists() {
        fs::remove_file(path).ok();
    }
    Ok(())
}

fn server_healthy(host: &str, port: u16) -> bool {
    let url = format!("http://{}:{}/health", host, port);
    Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()
        .and_then(|c| c.get(&url).send().ok())
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("kill")
        .arg(pid.to_string())
        .status()
        .context("failed to kill process")?;
    if status.success() {
        Ok(())
    } else {
        bail!("kill failed")
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

#[derive(Debug, Deserialize)]
struct CodexSkillsQuery {
    path: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CodexEvalQuery {
    path: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CodexProjectAiQuery {
    path: Option<String>,
    refresh: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CodexProjectAiRecentQuery {
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpsOverviewQuery {
    path: Option<String>,
    activity_limit: Option<usize>,
    log_lines: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexSkillsSyncRequest {
    path: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexSkillsReloadRequest {
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CodexResolveRequest {
    path: Option<String>,
    query: String,
    #[serde(default)]
    exact_cwd: bool,
}

fn resolve_codex_skills_target(path: Option<&str>) -> PathBuf {
    let candidate = path
        .map(config::expand_path)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| config::expand_path("~")));
    if candidate.is_absolute() {
        candidate
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| config::expand_path("~"))
            .join(candidate)
    }
}

async fn codex_skills(Query(query): Query<CodexSkillsQuery>) -> impl IntoResponse {
    let target_path = resolve_codex_skills_target(query.path.as_deref());
    let limit = query.limit.unwrap_or(12).clamp(1, 50);
    let result = tokio::task::spawn_blocking(move || {
        ai::codex_skills_dashboard_snapshot(&target_path, limit)
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex skills task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn codex_project_ai(Query(query): Query<CodexProjectAiQuery>) -> impl IntoResponse {
    let target_path = resolve_codex_skills_target(query.path.as_deref());
    let refresh = query.refresh.unwrap_or(false);
    let result =
        tokio::task::spawn_blocking(move || ai::codex_project_ai_snapshot(&target_path, refresh))
            .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex project-ai task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn codex_project_ai_recent(
    Query(query): Query<CodexProjectAiRecentQuery>,
) -> impl IntoResponse {
    let limit = query.limit.unwrap_or(12).clamp(1, 50);
    let result = tokio::task::spawn_blocking(move || ai::codex_project_ai_recent(limit)).await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex project-ai recent task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn codex_eval(Query(query): Query<CodexEvalQuery>) -> impl IntoResponse {
    let target_path = resolve_codex_skills_target(query.path.as_deref());
    let limit = query.limit.unwrap_or(200).clamp(20, 1000);
    let result =
        tokio::task::spawn_blocking(move || ai::codex_eval_snapshot(&target_path, limit)).await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex eval task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn codex_resolve(AxumJson(payload): AxumJson<CodexResolveRequest>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        ai::codex_resolve_inspector(payload.path, payload.query, payload.exact_cwd)
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex resolve task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn codex_skills_sync(
    AxumJson(payload): AxumJson<CodexSkillsSyncRequest>,
) -> impl IntoResponse {
    let target_path = resolve_codex_skills_target(payload.path.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let installed = ai::codex_skill_source_sync(&target_path, &payload.skills, payload.force)?;
        Ok::<_, anyhow::Error>(json!({
            "targetPath": target_path.display().to_string(),
            "installed": installed,
        }))
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(snapshot)).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex skills sync task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn codex_skills_reload(
    AxumJson(payload): AxumJson<CodexSkillsReloadRequest>,
) -> impl IntoResponse {
    let target_path = resolve_codex_skills_target(payload.path.as_deref());
    let result = tokio::task::spawn_blocking(move || {
        let reloaded = skills::reload_codex_skills_for_cwd(&target_path)?;
        Ok::<_, anyhow::Error>(json!({
            "targetPath": target_path.display().to_string(),
            "reloaded": reloaded,
        }))
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(snapshot)).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("codex skills reload task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn daemons() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| daemon_snapshot::load_daemon_snapshot(None)).await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("daemon snapshot task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn daemon_start(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    daemon_action_response(name, daemon_snapshot::FlowDaemonAction::Start).await
}

async fn daemon_stop(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    daemon_action_response(name, daemon_snapshot::FlowDaemonAction::Stop).await
}

async fn daemon_restart(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    daemon_action_response(name, daemon_snapshot::FlowDaemonAction::Restart).await
}

async fn daemon_cleanup_stale() -> impl IntoResponse {
    let result =
        tokio::task::spawn_blocking(move || daemon_snapshot::cleanup_stale_daemons(None)).await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("daemon cleanup task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn ops_visibility_overview(Query(query): Query<OpsOverviewQuery>) -> impl IntoResponse {
    let target_path = resolve_codex_skills_target(query.path.as_deref());
    let activity_limit = query.activity_limit.unwrap_or(20).clamp(1, 100);
    let log_lines = query.log_lines.unwrap_or(20).clamp(1, 200);
    let result = tokio::task::spawn_blocking(move || {
        ops_overview::load(&target_path, activity_limit, log_lines)
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("ops overview task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn daemon_action_response(
    name: String,
    action: daemon_snapshot::FlowDaemonAction,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        daemon_snapshot::run_daemon_action(&name, action, None)
    })
    .await;

    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("daemon action task failed: {err}") })),
        )
            .into_response(),
    }
}

async fn pr_edit_status(State(state): State<AppState>) -> impl IntoResponse {
    let guard = state.pr_edit.read().await;
    match guard.as_ref() {
        Some(svc) => (StatusCode::OK, Json(svc.status_snapshot().await)).into_response(),
        None => {
            let err = state.pr_edit_error.read().await.clone();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "pr-edit watcher not running", "detail": err })),
            )
                .into_response()
        }
    }
}

async fn pr_edit_rescan(State(state): State<AppState>) -> impl IntoResponse {
    let guard = state.pr_edit.read().await;
    match guard.as_ref() {
        Some(svc) => match svc.rescan().await {
            Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response(),
        },
        None => {
            let err = state.pr_edit_error.read().await.clone();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "pr-edit watcher not running", "detail": err })),
            )
                .into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IngestRequest {
    Single(LogEntry),
    Batch(Vec<LogEntry>),
}

async fn logs_ingest(Json(payload): Json<IngestRequest>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let mut conn = match log_store::open_log_db() {
            Ok(c) => c,
            Err(e) => return Err(e),
        };

        match payload {
            IngestRequest::Single(entry) => {
                let id = log_store::insert_log(&conn, &entry)?;
                Ok(json!({ "inserted": 1, "ids": [id] }))
            }
            IngestRequest::Batch(entries) => {
                let ids = log_store::insert_logs(&mut conn, &entries)?;
                Ok(json!({ "inserted": ids.len(), "ids": ids }))
            }
        }
    })
    .await;

    match result {
        Ok(Ok(response)) => (StatusCode::OK, Json(response)).into_response(),
        Ok(Err(err)) => {
            tracing::error!(?err, "log ingest failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(?err, "log ingest task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

async fn logs_query(Query(query): Query<LogQuery>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let conn = log_store::open_log_db()?;
        log_store::query_logs(&conn, &query)
    })
    .await;

    match result {
        Ok(Ok(entries)) => (StatusCode::OK, Json(entries)).into_response(),
        Ok(Err(err)) => {
            tracing::error!(?err, "log query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": err.to_string() })),
            )
                .into_response()
        }
        Err(err) => {
            tracing::error!(?err, "log query task panicked");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

// ============================================================================
// Flow Projects + AI Sessions
// ============================================================================

/// GET /projects - List all registered Flow projects.
async fn projects_list_all() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| projects::list_projects()).await;
    match result {
        Ok(Ok(entries)) => (StatusCode::OK, Json(json!({ "projects": entries }))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// GET /projects/:name/sessions - List AI sessions for a project.
async fn project_sessions(AxumPath(name): AxumPath<String>) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let project = projects::resolve_project(&name)?;
        let project = project.ok_or_else(|| anyhow::anyhow!("project not found: {}", name))?;
        ai::get_sessions_for_web(&project.project_root)
    })
    .await;

    match result {
        Ok(Ok(sessions)) => (StatusCode::OK, Json(json!({ "sessions": sessions }))).into_response(),
        Ok(Err(err)) => {
            let status = if err.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": err.to_string() }))).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// GET /workflow/overview - List repo/workspace/branch/PR workflow state for registered projects.
async fn workflow_overview() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(workflow::load_workflow_overview).await;
    match result {
        Ok(Ok(snapshot)) => (StatusCode::OK, Json(json!(snapshot))).into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct SessionDetailQuery {
    project: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CommitExplanationsQuery {
    limit: Option<usize>,
}

/// GET /sessions/:id?project=/path/to/root - Get full session conversation.
async fn session_detail(
    AxumPath(session_id): AxumPath<String>,
    Query(query): Query<SessionDetailQuery>,
) -> impl IntoResponse {
    let Some(project) = query
        .project
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "missing ?project= query parameter" })),
        )
            .into_response();
    };
    let project_root = std::path::PathBuf::from(project);

    let result = tokio::task::spawn_blocking(move || {
        ai::get_sessions_for_web(&project_root)
            .map(|sessions| sessions.into_iter().find(|s| s.id == session_id))
    })
    .await;

    match result {
        Ok(Ok(Some(session))) => (StatusCode::OK, Json(json!(session))).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
            .into_response(),
        Ok(Err(err)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// GET /projects/:name/commit-explanations?limit=50 - List commit explanations for a project.
async fn project_commit_explanations(
    AxumPath(name): AxumPath<String>,
    Query(query): Query<CommitExplanationsQuery>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let project = projects::resolve_project(&name)?;
        let project = project.ok_or_else(|| anyhow::anyhow!("project not found: {}", name))?;
        explain_commits::list_explained_commits(&project.project_root, query.limit)
    })
    .await;

    match result {
        Ok(Ok(commits)) => (StatusCode::OK, Json(json!({ "commits": commits }))).into_response(),
        Ok(Err(err)) => {
            let status = if err.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": err.to_string() }))).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// GET /projects/:name/commit-explanations/:sha - Get one commit explanation by SHA/prefix.
async fn project_commit_explanation_detail(
    AxumPath((name, sha)): AxumPath<(String, String)>,
) -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(move || {
        let project = projects::resolve_project(&name)?;
        let project = project.ok_or_else(|| anyhow::anyhow!("project not found: {}", name))?;
        explain_commits::get_explained_commit(&project.project_root, &sha)
    })
    .await;

    match result {
        Ok(Ok(Some(commit))) => (StatusCode::OK, Json(json!(commit))).into_response(),
        Ok(Ok(None)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "commit explanation not found" })),
        )
            .into_response(),
        Ok(Err(err)) => {
            let status = if err.to_string().contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, Json(json!({ "error": err.to_string() }))).into_response()
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

/// SSE stream of error logs - polls DB and emits new errors
async fn logs_errors_stream() -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let last_id = Arc::new(AtomicI64::new(0));

    // Get current max ID to start from
    if let Ok(conn) = log_store::open_log_db() {
        if let Ok(entries) = log_store::query_logs(
            &conn,
            &LogQuery {
                log_type: Some("error".to_string()),
                limit: 1,
                ..Default::default()
            },
        ) {
            if let Some(entry) = entries.first() {
                last_id.store(entry.id, Ordering::SeqCst);
            }
        }
    }

    let stream = stream::unfold(last_id, |last_id| async move {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let current_last = last_id.load(Ordering::SeqCst);
        let new_errors = tokio::task::spawn_blocking(move || {
            let conn = match log_store::open_log_db() {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };

            log_store::query_logs(
                &conn,
                &LogQuery {
                    log_type: Some("error".to_string()),
                    limit: 100,
                    ..Default::default()
                },
            )
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.id > current_last)
            .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let events: Vec<Result<Event, std::convert::Infallible>> = new_errors
            .into_iter()
            .map(|entry| {
                last_id.store(
                    entry.id.max(last_id.load(Ordering::SeqCst)),
                    Ordering::SeqCst,
                );
                let data = serde_json::to_string(&entry).unwrap_or_default();
                Ok(Event::default().data(data))
            })
            .collect();

        Some((stream::iter(events), last_id))
    })
    .flatten();

    Sse::new(stream).keep_alive(KeepAlive::default())
}
