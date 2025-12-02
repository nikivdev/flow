use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::Query,
    http::{Method, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use crate::cli::ServerOpts;
use crate::log_store::{self, LogEntry, LogQuery};

/// Run the flow HTTP server for log ingestion.
pub fn run(opts: ServerOpts) -> Result<()> {
    // Initialize database and schema on startup
    println!("Initializing database...");
    let conn = log_store::open_log_db().context("failed to initialize log database")?;
    drop(conn); // Close connection, will be reopened per-request
    println!("Database ready.");

    let addr: SocketAddr = format!("{}:{}", opts.host, opts.port)
        .parse()
        .context("invalid host:port")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;

    rt.block_on(async {
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers(Any);

        let router = Router::new()
            .route("/health", get(health))
            .route("/logs/ingest", post(logs_ingest))
            .route("/logs/query", get(logs_query))
            .layer(cors);

        println!("flow server listening on http://{}", addr);

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("failed to bind server")?;

        axum::serve(listener, router)
            .await
            .context("server error")?;

        Ok(())
    })
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
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
