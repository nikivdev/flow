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
    http::StatusCode,
    response::{
        IntoResponse, Json,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures::{Stream, StreamExt};
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::BroadcastStream;

use crate::{
    cli::DaemonOpts,
    config::{self, Config, ServerConfig},
    screen::ScreenBroadcaster,
    servers::{LogLine, ManagedServer, ServerSnapshot},
    terminal,
};

const LOG_BUFFER_CAPACITY: usize = 2048;

type ServerStore = Arc<RwLock<HashMap<String, Arc<ManagedServer>>>>;

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

    let router = Router::new()
        .route("/health", get(health))
        .route("/screen/latest", get(screen_latest))
        .route("/screen/stream", get(screen_stream))
        .route("/servers", get(servers_list))
        .route("/logs", get(all_logs))
        .route("/servers/:name/logs", get(server_logs))
        .route("/servers/:name/logs/stream", get(server_logs_stream))
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
