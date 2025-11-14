use std::{
    collections::HashMap, convert::Infallible, net::SocketAddr, pin::Pin, sync::Arc, time::Duration,
};

use anyhow::Result;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        IntoResponse, Json,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;

use crate::{
    cli::DaemonOpts,
    config::{self, Config},
    screen::ScreenBroadcaster,
    servers::{LogLine, ManagedServer, ServerSnapshot},
    watchers::WatchManager,
};

#[derive(Clone)]
struct AppState {
    screen: ScreenBroadcaster,
    servers: Arc<HashMap<String, Arc<ManagedServer>>>,
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

    // Prepare managed servers
    const LOG_BUFFER_CAPACITY: usize = 2048;
    let mut servers_map: HashMap<String, Arc<ManagedServer>> = HashMap::new();
    let servers = std::mem::take(&mut cfg.servers);
    for server_cfg in servers.into_iter() {
        let name = server_cfg.name.clone();
        let autostart = server_cfg.autostart;
        let managed = ManagedServer::new(server_cfg, LOG_BUFFER_CAPACITY);

        if autostart {
            let m = managed.clone();
            tokio::spawn(async move {
                if let Err(err) = m.start().await {
                    tracing::error!(?err, "failed to start managed server");
                }
            });
        }

        servers_map.insert(name, managed);
    }

    let watcher_guard = WatchManager::start(&cfg.watchers)?;
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
        servers: Arc::new(servers_map),
    };

    let router = Router::new()
        .route("/health", get(health))
        .route("/screen/latest", get(screen_latest))
        .route("/screen/stream", get(screen_stream))
        .route("/servers", get(servers_list))
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

    drop(watcher_guard);

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
    let futures_iter = state
        .servers
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
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    match state.servers.get(&name).cloned() {
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

async fn server_logs_stream(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Sse<Pin<Box<DynSseStream>>> {
    let (stream, enable_keep_alive) = match state.servers.get(&name).cloned() {
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

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("shutdown signal received");
    }
}
