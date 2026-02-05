//! HTTP reverse proxy server.
//!
//! A lightweight proxy that forwards requests to backends and records traces.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, Response, StatusCode};
use axum::routing::any;
use axum::Router;
use tokio::sync::RwLock;

use super::summary::SummaryState;
use super::trace::{hash_path, now_ns, TraceBuffer, TraceRecord};

/// A backend target
#[derive(Debug, Clone)]
pub struct Backend {
    pub name: String,
    pub addr: SocketAddr,
    pub index: u8,
}

/// Routing configuration
pub struct ProxyRouter {
    /// Host header -> backend index
    pub host_routes: HashMap<String, usize>,
    /// Path prefix -> backend index (checked in order)
    pub path_routes: Vec<(String, usize)>,
    /// Default backend (if no route matches)
    pub default: Option<usize>,
    /// All backends
    pub backends: Vec<Backend>,
}

impl ProxyRouter {
    pub fn new(backends: Vec<Backend>) -> Self {
        Self {
            host_routes: HashMap::new(),
            path_routes: Vec::new(),
            default: if backends.is_empty() { None } else { Some(0) },
            backends,
        }
    }

    pub fn add_host_route(&mut self, host: String, backend_idx: usize) {
        self.host_routes.insert(host, backend_idx);
    }

    pub fn add_path_route(&mut self, prefix: String, backend_idx: usize) {
        self.path_routes.push((prefix, backend_idx));
    }

    pub fn route(&self, host: Option<&str>, path: &str) -> Option<&Backend> {
        // 1. Check host header
        if let Some(host_str) = host {
            // Strip port if present
            let host_name = host_str.split(':').next().unwrap_or(host_str);
            if let Some(&idx) = self.host_routes.get(host_name) {
                return self.backends.get(idx);
            }
        }

        // 2. Check path prefix
        for (prefix, idx) in &self.path_routes {
            if path.starts_with(prefix) {
                return self.backends.get(*idx);
            }
        }

        // 3. Default
        self.default.and_then(|idx| self.backends.get(idx))
    }

    pub fn backend_names(&self) -> Vec<String> {
        self.backends.iter().map(|b| b.name.clone()).collect()
    }
}

/// Proxy server state
pub struct ProxyServer {
    pub router: RwLock<ProxyRouter>,
    pub trace_buffer: Arc<TraceBuffer>,
    pub summary_state: Arc<SummaryState>,
    pub client: reqwest::Client,
    pub trace_id_counter: AtomicU64,
}

impl ProxyServer {
    pub fn new(
        router: ProxyRouter,
        trace_buffer: Arc<TraceBuffer>,
        summary_state: Arc<SummaryState>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            router: RwLock::new(router),
            trace_buffer,
            summary_state,
            client,
            trace_id_counter: AtomicU64::new(1),
        }
    }

    /// Generate a new trace ID
    pub fn next_trace_id(&self) -> u128 {
        self.trace_id_counter.fetch_add(1, Ordering::Relaxed) as u128
    }
}

/// Handle proxied requests
async fn proxy_handler(
    State(server): State<Arc<ProxyServer>>,
    req: Request<Body>,
) -> Response<Body> {
    let start = Instant::now();
    let start_ns = now_ns();
    let req_id = server.trace_buffer.next_req_id();

    // Get or generate trace ID
    let trace_id = req
        .headers()
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u128>().ok())
        .unwrap_or_else(|| server.next_trace_id());

    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let method_str = method.as_str();
    let host = req
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Route to backend
    let router = server.router.read().await;
    let backend = match router.route(host.as_deref(), &path) {
        Some(b) => b.clone(),
        None => {
            drop(router);
            // No route found
            let mut record = TraceRecord::new();
            record.set_timestamp(start_ns);
            record.set_req_id(req_id);
            record.set_latency_status(
                start.elapsed().as_micros() as u32,
                502,
                method_str.into(),
                0,
            );
            record.set_path(&path);
            record.set_path_hash(hash_path(&path));
            server.trace_buffer.record(&record);

            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from("No backend configured"))
                .unwrap();
        }
    };
    drop(router);

    // Build upstream URL
    let upstream_url = format!(
        "http://{}{}{}",
        backend.addr,
        path,
        uri.query().map(|q| format!("?{}", q)).unwrap_or_default()
    );

    // Forward request headers
    let upstream_start = Instant::now();
    let mut upstream_req = server.client.request(method.clone(), &upstream_url);

    // Copy headers (except host)
    for (name, value) in req.headers() {
        if name != "host" {
            if let Ok(v) = value.to_str() {
                upstream_req = upstream_req.header(name.as_str(), v);
            }
        }
    }

    // Add trace ID header
    upstream_req = upstream_req.header("x-trace-id", trace_id.to_string());

    // Get request body
    let body_bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .ok();
    let bytes_in = body_bytes.as_ref().map(|b| b.len()).unwrap_or(0) as u32;

    // Send body if present
    if let Some(body) = body_bytes {
        if !body.is_empty() {
            upstream_req = upstream_req.body(body.to_vec());
        }
    }

    // Execute request
    let result = upstream_req.send().await;
    let upstream_latency_us = upstream_start.elapsed().as_micros() as u32;

    let (status, body, bytes_out) = match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            let bytes_out = body.len() as u32;

            // Store error body for AI analysis
            if status >= 400 {
                server.summary_state.store_error_body(req_id, body.clone());
            }

            (status, body, bytes_out)
        }
        Err(e) => {
            let error_body = format!("{{\"error\": \"{}\"}}", e);
            server
                .summary_state
                .store_error_body(req_id, error_body.clone());
            (502, error_body, 0)
        }
    };

    let total_latency_us = start.elapsed().as_micros() as u32;

    // Record trace
    let mut record = TraceRecord::new();
    record.set_timestamp(start_ns);
    record.set_req_id(req_id);
    record.set_latency_status(total_latency_us, status, method_str.into(), 0);
    record.set_bytes(bytes_in, bytes_out);
    record.set_target_and_trace_id(backend.index, path.len().min(255) as u8, trace_id);
    record.set_path_hash(hash_path(&path));
    record.set_upstream_latency(upstream_latency_us);
    record.set_path(&path);
    server.trace_buffer.record(&record);

    // Build response
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("x-trace-id", trace_id.to_string())
        .header("x-proxy-latency-ms", (total_latency_us / 1000).to_string())
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

/// Health check endpoint
async fn health_handler(State(server): State<Arc<ProxyServer>>) -> Response<Body> {
    let router = server.router.read().await;
    let backends: Vec<_> = router
        .backends
        .iter()
        .map(|b| {
            serde_json::json!({
                "name": b.name,
                "addr": b.addr.to_string(),
            })
        })
        .collect();

    let stats = serde_json::json!({
        "status": "ok",
        "total_requests": server.trace_buffer.write_index(),
        "backends": backends,
    });

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string_pretty(&stats).unwrap()))
        .unwrap()
}

/// Create the axum router
pub fn create_router(server: Arc<ProxyServer>) -> Router {
    Router::new()
        .route("/_proxy/health", axum::routing::get(health_handler))
        .fallback(any(proxy_handler))
        .with_state(server)
}

/// Run the proxy server
pub async fn run_server(addr: SocketAddr, server: Arc<ProxyServer>) -> Result<()> {
    let app = create_router(server);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Failed to bind proxy server")?;

    tracing::info!("Proxy server listening on {}", addr);

    axum::serve(listener, app)
        .await
        .context("Proxy server error")?;

    Ok(())
}
