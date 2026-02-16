use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use anyhow::{Context, Result};
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
use futures::{Stream, StreamExt};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{Any, CorsLayer};

use crate::{
    ai,
    cli::DaemonOpts,
    config::{self, Config, ServerConfig},
    log_store::{self, LogEntry, LogQuery},
    projects,
    running,
    screen::ScreenBroadcaster,
    servers::{LogLine, ManagedServer, ServerSnapshot},
    supervisor,
    terminal,
};

const LOG_BUFFER_CAPACITY: usize = 2048;

type ServerStore = Arc<RwLock<HashMap<String, Arc<ManagedServer>>>>;

/// Unified process snapshot returned by GET /processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProcessSnapshot {
    name: String,
    source: String,
    project: Option<String>,
    command: String,
    status: String,
    pid: Option<u32>,
    port: Option<u16>,
    #[serde(rename = "startedAt")]
    started_at: Option<u128>,
}

#[derive(Clone)]
struct AppState {
    screen: ScreenBroadcaster,
    servers: ServerStore,
}

type DynSseStream = dyn Stream<Item = std::result::Result<Event, Infallible>> + Send;

pub async fn run(opts: DaemonOpts) -> Result<()> {
    let screen = ScreenBroadcaster::with_mock_stream(opts.frame_buffer, opts.fps);

    // Load configuration for managed servers.
    let config_path = opts
        .config
        .clone()
        .unwrap_or_else(config::default_config_path);
    let mut cfg: Config = config::load_or_default(&config_path);
    tracing::info!(
        path = %config_path.display(),
        server_count = cfg.servers.len(),
        "loaded flow config"
    );
    if let Some(version) = cfg.version {
        tracing::debug!(version, "config version detected");
    }

    terminal::maybe_enable_terminal_tracing(&cfg.options);

    let servers_store: ServerStore = Arc::new(RwLock::new(HashMap::new()));
    sync_servers(&servers_store, std::mem::take(&mut cfg.servers)).await;

    if let Some(stream) = cfg.stream.as_ref() {
        tracing::info!(
            provider = %stream.provider,
            hotkey = %stream.hotkey.as_deref().unwrap_or(""),
            toggle_url = %stream.toggle_url.as_deref().unwrap_or(""),
            "stream config detected"
        );
    }

    let state = AppState {
        screen,
        servers: Arc::clone(&servers_store),
    };

    let (reload_tx, mut reload_rx) = mpsc::channel(4);
    if let Err(err) = spawn_config_watcher(&config_path, reload_tx.clone()) {
        tracing::warn!(?err, "failed to watch config for changes");
    }

    let servers_for_reload = Arc::clone(&servers_store);
    let config_path_for_reload = config_path.clone();
    tokio::spawn(async move {
        while reload_rx.recv().await.is_some() {
            if let Err(err) = reload_config(&config_path_for_reload, &servers_for_reload).await {
                tracing::warn!(?err, "config reload failed");
            }
        }
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(Any);

    let router = Router::new()
        .route("/health", get(health))
        .route("/screen/latest", get(screen_latest))
        .route("/screen/stream", get(screen_stream))
        .route("/servers", get(servers_list))
        .route("/logs", get(all_logs))
        .route("/servers/:name/logs", get(server_logs))
        .route("/servers/:name/logs/stream", get(server_logs_stream))
        // Unified process management endpoints
        .route("/processes", get(processes_list))
        .route("/processes/:name/start", post(process_start))
        .route("/processes/:name/stop", post(process_stop))
        .route("/processes/:name/restart", post(process_restart))
        .route("/processes/:name/logs/stream", get(process_logs_stream))
        // Log ingestion endpoints
        .route("/logs/ingest", post(logs_ingest))
        .route("/logs/query", get(logs_query))
        // Flow projects + AI sessions
        .route("/projects", get(projects_list_all))
        .route("/projects/:name/sessions", get(project_sessions))
        .route("/sessions/:id", get(session_detail))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from((opts.host, opts.port));
    tracing::info!(
        "flowd listening on http://{addr} (mock fps: {}, buffer: {}, config: {})",
        opts.fps,
        opts.frame_buffer,
        config_path.display(),
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "message": "flow daemon ready"
    }))
}

async fn screen_latest(State(state): State<AppState>) -> impl IntoResponse {
    match state.screen.latest().await {
        Some(frame) => (StatusCode::OK, Json(frame)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn screen_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.screen.subscribe()).filter_map(|result| async move {
        match result {
            Ok(frame) => match serde_json::to_string(&frame) {
                Ok(payload) => Some(Ok(Event::default().data(payload))),
                Err(err) => {
                    tracing::error!(?err, "failed to serialize screen frame");
                    None
                }
            },
            Err(err) => {
                tracing::warn!(?err, "screen broadcast channel dropped event");
                None
            }
        }
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text(":flowd keep-alive"),
    )
}

async fn servers_list(State(state): State<AppState>) -> impl IntoResponse {
    let servers = state.servers.read().await;
    let futures_iter = servers
        .values()
        .cloned()
        .map(|server| async move { server.snapshot().await });

    let snapshots: Vec<ServerSnapshot> = futures::future::join_all(futures_iter).await;

    (StatusCode::OK, Json(snapshots)).into_response()
}

#[derive(Debug, Deserialize)]
struct LogsQuery {
    #[serde(default = "default_logs_limit")]
    limit: usize,
}

fn default_logs_limit() -> usize {
    512
}

async fn server_logs(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    let server = {
        let guard = state.servers.read().await;
        guard.get(&name).cloned()
    };

    match server {
        Some(server) => {
            let lines: Vec<LogLine> = server.recent_logs(query.limit).await;
            (StatusCode::OK, Json(lines)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("unknown server {name}") })),
        )
            .into_response(),
    }
}

async fn all_logs(
    State(state): State<AppState>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    let servers: Vec<_> = {
        let guard = state.servers.read().await;
        guard.values().cloned().collect()
    };

    let mut entries: Vec<LogLine> = Vec::new();
    for server in servers {
        let mut lines = server.recent_logs(query.limit).await;
        entries.append(&mut lines);
    }

    entries.sort_by_key(|line| line.timestamp_ms);
    if entries.len() > query.limit {
        entries = entries.split_off(entries.len() - query.limit);
    }

    (StatusCode::OK, Json(entries)).into_response()
}

async fn server_logs_stream(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Sse<Pin<Box<DynSseStream>>> {
    let server = {
        let guard = state.servers.read().await;
        guard.get(&name).cloned()
    };

    let (stream, enable_keep_alive) = match server {
        Some(server) => {
            let receiver = server.subscribe();
            let stream = BroadcastStream::new(receiver).filter_map(|result| async move {
                match result {
                    Ok(line) => match serde_json::to_string(&line) {
                        Ok(payload) => Some(Ok(Event::default().data(payload))),
                        Err(err) => {
                            tracing::error!(?err, "failed to serialize log line");
                            None
                        }
                    },
                    Err(err) => {
                        tracing::warn!(?err, "server log broadcast channel dropped event");
                        None
                    }
                }
            });

            (Box::pin(stream) as Pin<Box<DynSseStream>>, true)
        }
        None => {
            let stream = futures::stream::once(async move {
                Ok(Event::default().data(
                    serde_json::to_string(&json!({
                        "error": format!("unknown server {name}")
                    }))
                    .unwrap_or_else(|_| "{\"error\":\"unknown server\"}".to_string()),
                ))
            });

            (Box::pin(stream) as Pin<Box<DynSseStream>>, false)
        }
    };

    let sse = Sse::new(stream);
    if enable_keep_alive {
        sse.keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(5))
                .text(":flowd log keep-alive"),
        )
    } else {
        sse
    }
}

// ============================================================================
// Unified Process Management Endpoints
// ============================================================================

/// GET /processes - Returns all running processes from servers, daemons, and tasks.
async fn processes_list(State(state): State<AppState>) -> impl IntoResponse {
    let mut snapshots: Vec<ProcessSnapshot> = Vec::new();

    // 1. Managed servers from ServerStore
    {
        let servers = state.servers.read().await;
        let futures_iter = servers
            .values()
            .cloned()
            .map(|server| async move { server.snapshot().await });
        let server_snapshots: Vec<ServerSnapshot> = futures::future::join_all(futures_iter).await;
        for s in server_snapshots {
            snapshots.push(ProcessSnapshot {
                name: s.name.clone(),
                source: "server".to_string(),
                project: None,
                command: if s.args.is_empty() {
                    s.command.clone()
                } else {
                    format!("{} {}", s.command, s.args.join(" "))
                },
                status: s.status.clone(),
                pid: s.pid,
                port: s.port,
                started_at: None,
            });
        }
    }

    // 2. Supervisor daemons via IPC
    if let Ok(socket_path) = supervisor::resolve_socket_path(None) {
        if socket_path.exists() {
            let ipc_result = tokio::task::spawn_blocking(move || {
                let request = supervisor::IpcRequest {
                    action: supervisor::SupervisorIpcAction::Status {
                        config_path: None,
                    },
                };
                supervisor::send_request(&socket_path, &request)
            })
            .await;

            if let Ok(Ok(response)) = ipc_result {
                if let Some(daemons) = response.daemons {
                    for d in daemons {
                        // Skip duplicates already covered by managed servers
                        if snapshots.iter().any(|s| s.name == d.name) {
                            continue;
                        }
                        let port = d
                            .health_url
                            .as_deref()
                            .and_then(|url| url.rsplit(':').next())
                            .and_then(|port_path| {
                                port_path.split('/').next().and_then(|p| p.parse().ok())
                            });
                        snapshots.push(ProcessSnapshot {
                            name: d.name,
                            source: "daemon".to_string(),
                            project: None,
                            command: d
                                .description
                                .unwrap_or_default(),
                            status: if d.running { "running".to_string() } else { "stopped".to_string() },
                            pid: d.pid,
                            port,
                            started_at: None,
                        });
                    }
                }
            }
        }
    }

    // 3. Running tasks from running.json
    let task_result = tokio::task::spawn_blocking(|| running::load_running_processes()).await;
    if let Ok(Ok(processes)) = task_result {
        for procs in processes.projects.values() {
            for p in procs {
                snapshots.push(ProcessSnapshot {
                    name: p.task_name.clone(),
                    source: "task".to_string(),
                    project: p.project_name.clone(),
                    command: p.command.clone(),
                    status: "running".to_string(),
                    pid: Some(p.pid),
                    port: None,
                    started_at: Some(p.started_at),
                });
            }
        }
    }

    (StatusCode::OK, Json(snapshots)).into_response()
}

/// POST /processes/:name/start
async fn process_start(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    // Try managed server first
    {
        let servers = state.servers.read().await;
        if let Some(server) = servers.get(&name) {
            return match server.start().await {
                Ok(()) => (StatusCode::OK, Json(json!({ "ok": true, "message": format!("{name} started") }))).into_response(),
                Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": err.to_string() }))).into_response(),
            };
        }
    }

    // Try supervisor daemon
    let daemon_name = name.clone();
    let ipc_result = tokio::task::spawn_blocking(move || {
        let socket_path = supervisor::resolve_socket_path(None)?;
        let request = supervisor::IpcRequest {
            action: supervisor::SupervisorIpcAction::StartDaemon {
                name: daemon_name,
                config_path: None,
            },
        };
        supervisor::send_request(&socket_path, &request)
    })
    .await;

    match ipc_result {
        Ok(Ok(resp)) if resp.ok => {
            (StatusCode::OK, Json(json!({ "ok": true, "message": resp.message }))).into_response()
        }
        Ok(Ok(resp)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": resp.message }))).into_response()
        }
        _ => {
            (StatusCode::NOT_FOUND, Json(json!({ "error": format!("unknown process {name}") }))).into_response()
        }
    }
}

/// POST /processes/:name/stop
async fn process_stop(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    // Try managed server first
    {
        let servers = state.servers.read().await;
        if let Some(server) = servers.get(&name) {
            return match server.stop().await {
                Ok(()) => (StatusCode::OK, Json(json!({ "ok": true, "message": format!("{name} stopped") }))).into_response(),
                Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": err.to_string() }))).into_response(),
            };
        }
    }

    // Try supervisor daemon
    let daemon_name = name.clone();
    let ipc_result = tokio::task::spawn_blocking(move || {
        let socket_path = supervisor::resolve_socket_path(None)?;
        let request = supervisor::IpcRequest {
            action: supervisor::SupervisorIpcAction::StopDaemon {
                name: daemon_name,
                config_path: None,
            },
        };
        supervisor::send_request(&socket_path, &request)
    })
    .await;

    match ipc_result {
        Ok(Ok(resp)) if resp.ok => {
            (StatusCode::OK, Json(json!({ "ok": true, "message": resp.message }))).into_response()
        }
        Ok(Ok(resp)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": resp.message }))).into_response()
        }
        _ => {
            (StatusCode::NOT_FOUND, Json(json!({ "error": format!("unknown process {name}") }))).into_response()
        }
    }
}

/// POST /processes/:name/restart
async fn process_restart(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> impl IntoResponse {
    // Try managed server: stop then start
    {
        let servers = state.servers.read().await;
        if let Some(server) = servers.get(&name) {
            let _ = server.stop().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            return match server.start().await {
                Ok(()) => (StatusCode::OK, Json(json!({ "ok": true, "message": format!("{name} restarted") }))).into_response(),
                Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": err.to_string() }))).into_response(),
            };
        }
    }

    // Try supervisor daemon
    let daemon_name = name.clone();
    let ipc_result = tokio::task::spawn_blocking(move || {
        let socket_path = supervisor::resolve_socket_path(None)?;
        let request = supervisor::IpcRequest {
            action: supervisor::SupervisorIpcAction::RestartDaemon {
                name: daemon_name,
                config_path: None,
            },
        };
        supervisor::send_request(&socket_path, &request)
    })
    .await;

    match ipc_result {
        Ok(Ok(resp)) if resp.ok => {
            (StatusCode::OK, Json(json!({ "ok": true, "message": resp.message }))).into_response()
        }
        Ok(Ok(resp)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": resp.message }))).into_response()
        }
        _ => {
            (StatusCode::NOT_FOUND, Json(json!({ "error": format!("unknown process {name}") }))).into_response()
        }
    }
}

/// GET /processes/:name/logs/stream - SSE log stream for any process type.
async fn process_logs_stream(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Sse<Pin<Box<DynSseStream>>> {
    // Check if it's a managed server — delegate to existing broadcast
    let server = {
        let guard = state.servers.read().await;
        guard.get(&name).cloned()
    };

    if let Some(server) = server {
        let receiver = server.subscribe();
        let stream = BroadcastStream::new(receiver).filter_map(|result| async move {
            match result {
                Ok(line) => match serde_json::to_string(&line) {
                    Ok(payload) => Some(Ok(Event::default().data(payload))),
                    Err(_) => None,
                },
                Err(_) => None,
            }
        });

        return Sse::new(Box::pin(stream) as Pin<Box<DynSseStream>>).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(5))
                .text(":flowd process log keep-alive"),
        );
    }

    // For daemons/tasks: tail log files via polling
    let log_path = find_process_log_path(&name).await;

    let stream: Pin<Box<DynSseStream>> = match log_path {
        Some(path) => {
            let stream = futures::stream::unfold(
                (path, 0u64),
                |(path, last_pos)| async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let metadata = tokio::fs::metadata(&path).await.ok()?;
                    let file_len = metadata.len();
                    if file_len <= last_pos {
                        return Some((Vec::new(), (path, last_pos)));
                    }
                    let mut file = tokio::fs::File::open(&path).await.ok()?;
                    tokio::io::AsyncSeekExt::seek(
                        &mut file,
                        std::io::SeekFrom::Start(last_pos),
                    )
                    .await
                    .ok()?;
                    let mut buf = vec![0u8; (file_len - last_pos).min(65536) as usize];
                    let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf).await.ok()?;
                    buf.truncate(n);
                    let new_pos = last_pos + n as u64;
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let events: Vec<std::result::Result<Event, Infallible>> = text
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|line| {
                            Ok(Event::default().data(
                                serde_json::to_string(&json!({
                                    "line": line,
                                    "stream": "stdout",
                                    "timestamp_ms": running::now_ms(),
                                }))
                                .unwrap_or_default(),
                            ))
                        })
                        .collect();
                    Some((events, (path, new_pos)))
                },
            )
            .flat_map(futures::stream::iter);
            Box::pin(stream)
        }
        None => {
            let stream = futures::stream::once(async move {
                Ok(Event::default().data(
                    json!({ "error": format!("no logs found for {name}") }).to_string(),
                ))
            });
            Box::pin(stream)
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(5))
            .text(":flowd process log keep-alive"),
    )
}

/// Find log file path for a daemon or task by name.
async fn find_process_log_path(name: &str) -> Option<std::path::PathBuf> {
    let state_dir = config::global_state_dir();

    // Check daemon stdout log
    let daemon_log = state_dir.join("daemons").join(name).join("stdout.log");
    if tokio::fs::metadata(&daemon_log).await.is_ok() {
        return Some(daemon_log);
    }

    // Check task log files under ~/.config/flow/logs/
    let logs_dir = state_dir.join("logs");
    if let Ok(mut entries) = tokio::fs::read_dir(&logs_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let project_dir = entry.path();
            let task_log = project_dir.join(format!("{name}.log"));
            if tokio::fs::metadata(&task_log).await.is_ok() {
                return Some(task_log);
            }
        }
    }

    None
}

async fn reload_config(path: &Path, servers: &ServerStore) -> Result<()> {
    let mut cfg = config::load(path)
        .with_context(|| format!("failed to reload config at {}", path.display()))?;
    tracing::info!(path = %path.display(), "config changed; reloading");

    sync_servers(servers, std::mem::take(&mut cfg.servers)).await;

    if let Some(stream) = cfg.stream {
        tracing::info!(
            provider = %stream.provider,
            hotkey = %stream.hotkey.as_deref().unwrap_or(""),
            toggle_url = %stream.toggle_url.as_deref().unwrap_or(""),
            "stream config updated"
        );
    }

    Ok(())
}

async fn sync_servers(store: &ServerStore, configs: Vec<ServerConfig>) {
    let mut desired: HashMap<String, ServerConfig> = HashMap::new();
    for cfg in configs.into_iter() {
        desired.insert(cfg.name.clone(), cfg);
    }

    let mut to_stop: Vec<Arc<ManagedServer>> = Vec::new();
    let mut to_start: Vec<Arc<ManagedServer>> = Vec::new();

    {
        let mut guard = store.write().await;

        guard.retain(|name, server| {
            if !desired.contains_key(name) {
                to_stop.push(server.clone());
                false
            } else {
                true
            }
        });

        for (name, cfg) in desired.into_iter() {
            if let Some(existing) = guard.get(&name) {
                if existing.config() == &cfg {
                    continue;
                }
                to_stop.push(existing.clone());
                guard.remove(&name);
            }

            let managed = ManagedServer::new(cfg.clone(), LOG_BUFFER_CAPACITY);
            if cfg.autostart {
                to_start.push(managed.clone());
            }
            guard.insert(name, managed);
        }
    }

    for server in to_stop {
        if let Err(err) = server.stop().await {
            tracing::warn!(
                ?err,
                name = server.config().name,
                "failed to stop managed server during reload"
            );
        }
    }

    for server in to_start {
        tokio::spawn(async move {
            if let Err(err) = server.start().await {
                tracing::error!(
                    ?err,
                    server = server.config().name,
                    "failed to start managed server"
                );
            }
        });
    }
}

fn spawn_config_watcher(path: &Path, tx: mpsc::Sender<()>) -> notify::Result<()> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let watch_root = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| target.clone());

    std::thread::spawn(move || {
        let (event_tx, event_rx) = std_mpsc::channel();
        let mut debouncer = match new_debouncer(Duration::from_millis(250), event_tx) {
            Ok(debouncer) => debouncer,
            Err(err) => {
                tracing::error!(?err, "failed to initialize config watcher");
                return;
            }
        };

        if let Err(err) = debouncer
            .watcher()
            .watch(&watch_root, RecursiveMode::NonRecursive)
        {
            tracing::error!(?err, path = %watch_root.display(), "failed to watch config directory");
            return;
        }

        while let Ok(result) = event_rx.recv() {
            match result {
                Ok(events) => {
                    let should_reload = events.iter().any(|event| same_file(&target, &event.path));
                    if should_reload && tx.blocking_send(()).is_err() {
                        break;
                    }
                }
                Err(err) => tracing::warn!(?err, "config watcher error"),
            }
        }
    });

    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }

    if let Ok(canon) = b.canonicalize() {
        if canon == a {
            return true;
        }
    }

    a.file_name()
        .is_some_and(|name| Some(name) == b.file_name())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("shutdown signal received");
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
    let result = tokio::task::spawn_blocking(move || {
        let project_root = match query.project {
            Some(path) => std::path::PathBuf::from(path),
            None => anyhow::bail!("missing ?project= query parameter"),
        };
        ai::get_sessions_for_web(&project_root).map(|sessions| {
            sessions.into_iter().find(|s| s.id == session_id)
        })
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

// ============================================================================
// Log Ingestion Endpoints
// ============================================================================

/// Request body for log ingestion - single entry or batch.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IngestRequest {
    Single(LogEntry),
    Batch(Vec<LogEntry>),
}

/// POST /logs/ingest - Ingest log entries into the database.
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

/// GET /logs/query - Query stored logs with filters.
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
