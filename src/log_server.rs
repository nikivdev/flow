use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{Path as AxumPath, Query, State},
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
use crate::{ai, projects};

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
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string())
        .arg("foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

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
            .route("/logs/ingest", post(logs_ingest))
            .route("/logs/query", get(logs_query))
            .route("/logs/errors/stream", get(logs_errors_stream))
            .route("/pr-edit/status", get(pr_edit_status))
            .route("/pr-edit/rescan", post(pr_edit_rescan))
            // Flow projects + AI sessions
            .route("/projects", get(projects_list_all))
            .route("/projects/{name}/sessions", get(project_sessions))
            .route("/sessions/{id}", get(session_detail))
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

#[derive(Debug, Deserialize)]
struct SessionDetailQuery {
    project: Option<String>,
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
