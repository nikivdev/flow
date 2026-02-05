//! proxyx - Zero-cost traced reverse proxy for Flow.
//!
//! This module provides a lightweight reverse proxy with always-on observability
//! designed for macOS development. Key features:
//!
//! - **Zero-cost tracing** via mmap ring buffer (no allocations per request)
//! - **Agent-readable summary** JSON file for AI assistants
//! - **Trace ID propagation** across services
//! - **Flow integration** via flow.toml configuration

pub mod server;
pub mod summary;
pub mod trace;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use server::{Backend, ProxyRouter, ProxyServer};
use summary::{SummaryState, SummaryWriter};
use trace::TraceBuffer;

/// Proxy configuration from flow.toml
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ProxyConfig {
    /// Listen address (e.g., ":8080" or "127.0.0.1:8080")
    #[serde(default = "default_listen")]
    pub listen: String,

    /// Trace ring buffer size (e.g., "16MB")
    #[serde(default = "default_trace_size")]
    pub trace_size: String,

    /// Trace directory
    #[serde(default)]
    pub trace_dir: Option<String>,

    /// Write agent-readable summary JSON
    #[serde(default = "default_true")]
    pub trace_summary: bool,

    /// Summary update interval (e.g., "1s")
    #[serde(default = "default_summary_interval")]
    pub summary_interval: String,

    /// Slow request threshold in milliseconds
    #[serde(default = "default_slow_threshold")]
    pub slow_threshold_ms: u32,
}

fn default_listen() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_trace_size() -> String {
    "16MB".to_string()
}

fn default_true() -> bool {
    true
}

fn default_summary_interval() -> String {
    "1s".to_string()
}

fn default_slow_threshold() -> u32 {
    500
}

/// Individual proxy target configuration
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyTargetConfig {
    /// Unique name for this proxy
    pub name: String,

    /// Target address (e.g., "localhost:3000")
    pub target: String,

    /// Optional host-based routing
    #[serde(default)]
    pub host: Option<String>,

    /// Optional path prefix routing
    #[serde(default)]
    pub path: Option<String>,

    /// Capture request/response bodies
    #[serde(default)]
    pub capture_body: bool,

    /// Max body size to capture
    #[serde(default = "default_capture_max")]
    pub capture_body_max: String,

    /// Health check path
    #[serde(default)]
    pub health: Option<String>,

    /// Paths to exclude from tracing
    #[serde(default)]
    pub exclude_paths: Vec<String>,
}

fn default_capture_max() -> String {
    "64KB".to_string()
}

/// Parse size string (e.g., "16MB") to bytes
pub fn parse_size(s: &str) -> usize {
    let s = s.trim().to_uppercase();
    if let Some(num) = s.strip_suffix("GB") {
        num.trim().parse::<usize>().unwrap_or(0) * 1024 * 1024 * 1024
    } else if let Some(num) = s.strip_suffix("MB") {
        num.trim().parse::<usize>().unwrap_or(0) * 1024 * 1024
    } else if let Some(num) = s.strip_suffix("KB") {
        num.trim().parse::<usize>().unwrap_or(0) * 1024
    } else if let Some(num) = s.strip_suffix("B") {
        num.trim().parse::<usize>().unwrap_or(0)
    } else {
        s.parse::<usize>().unwrap_or(16 * 1024 * 1024)
    }
}

/// Parse duration string (e.g., "1s", "500ms") to Duration
pub fn parse_duration(s: &str) -> Duration {
    let s = s.trim().to_lowercase();
    if let Some(num) = s.strip_suffix("ms") {
        Duration::from_millis(num.trim().parse().unwrap_or(1000))
    } else if let Some(num) = s.strip_suffix('s') {
        Duration::from_secs(num.trim().parse().unwrap_or(1))
    } else if let Some(num) = s.strip_suffix('m') {
        Duration::from_secs(num.trim().parse::<u64>().unwrap_or(1) * 60)
    } else {
        Duration::from_secs(s.parse().unwrap_or(1))
    }
}

/// Start the proxy server with the given configuration
pub async fn start(
    config: ProxyConfig,
    targets: Vec<ProxyTargetConfig>,
) -> Result<()> {
    // Parse listen address
    let listen_addr: SocketAddr = if config.listen.starts_with(':') {
        format!("127.0.0.1{}", config.listen).parse()
    } else {
        config.listen.parse()
    }
    .context("Invalid listen address")?;

    // Initialize trace buffer
    let trace_dir = config
        .trace_dir
        .as_ref()
        .map(|s| PathBuf::from(shellexpand::tilde(s).to_string()))
        .unwrap_or_else(trace::default_trace_dir);

    let trace_size = parse_size(&config.trace_size);

    let trace_buffer = TraceBuffer::init(&trace_dir, trace_size)
        .context("Failed to initialize trace buffer")?;
    let trace_buffer = Arc::new(trace_buffer);

    // Build backends
    let mut backends = Vec::new();
    for (idx, target) in targets.iter().enumerate() {
        let addr: SocketAddr = if target.target.contains(':') {
            target.target.parse()
        } else {
            format!("127.0.0.1:{}", target.target).parse()
        }
        .context(format!("Invalid target address: {}", target.target))?;

        backends.push(Backend {
            name: target.name.clone(),
            addr,
            index: idx as u8,
        });
    }

    // Build router
    let mut router = ProxyRouter::new(backends);

    for (idx, target) in targets.iter().enumerate() {
        if let Some(host) = &target.host {
            router.add_host_route(host.clone(), idx);
        }
        if let Some(path) = &target.path {
            router.add_path_route(path.clone(), idx);
        }
    }

    // Create summary state
    let target_names = router.backend_names();
    let summary_state = Arc::new(SummaryState::new(target_names, config.slow_threshold_ms));

    // Create server
    let server = Arc::new(ProxyServer::new(router, trace_buffer.clone(), summary_state.clone()));

    // Start summary writer if enabled
    if config.trace_summary {
        let summary_path = trace_dir.join("trace-summary.json");
        let interval = parse_duration(&config.summary_interval);
        let writer = SummaryWriter::new(
            trace_buffer.clone(),
            summary_state.clone(),
            summary_path.clone(),
            interval,
        );
        writer.spawn();
        tracing::info!("Summary writer started: {:?}", summary_path);
    }

    // Print startup info
    println!("proxyx listening on {}", listen_addr);
    println!("Trace buffer: {:?} ({} bytes)", trace_dir, trace_size);
    println!("Targets:");
    for target in &targets {
        println!("  {} -> {}", target.name, target.target);
    }

    // Run server
    server::run_server(listen_addr, server).await
}

/// CLI command to view recent traces
pub fn trace_last(count: usize) -> Result<()> {
    let trace_dir = trace::default_trace_dir();

    // Find the trace file
    let entries = std::fs::read_dir(&trace_dir)?;
    let trace_file = entries
        .filter_map(|e| e.ok())
        .find(|e| {
            e.file_name()
                .to_str()
                .map(|s| s.starts_with("trace.") && s.ends_with(".bin"))
                .unwrap_or(false)
        })
        .context("No trace file found")?;

    // Memory-map and read
    let file = std::fs::File::open(trace_file.path())?;
    let size = file.metadata()?.len() as usize;

    let buffer = TraceBuffer::init(&trace_dir, size).context("Failed to open trace buffer")?;

    let records = buffer.recent(count);

    println!(
        "{:<12} {:<8} {:<6} {:<40} {:<6} {:<10} {:<10}",
        "TIME", "REQ_ID", "METHOD", "PATH", "STATUS", "LATENCY", "TARGET"
    );
    println!("{}", "-".repeat(100));

    for record in records {
        if record.timestamp() == 0 {
            continue;
        }
        println!(
            "{:<12} {:<8x} {:<6} {:<40} {:<6} {:<10} {:<10}",
            format!("{}ms ago", record.timestamp() / 1_000_000),
            record.req_id(),
            format!("{:?}", record.method()),
            truncate_path(record.path(), 40),
            record.status(),
            format!("{}ms", record.latency_us() / 1000),
            record.target_idx(),
        );
    }

    Ok(())
}

fn truncate_path(path: &str, max_len: usize) -> String {
    if path.len() <= max_len {
        path.to_string()
    } else {
        format!("{}...", &path[..max_len - 3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("16MB"), 16 * 1024 * 1024);
        assert_eq!(parse_size("1GB"), 1024 * 1024 * 1024);
        assert_eq!(parse_size("64KB"), 64 * 1024);
        assert_eq!(parse_size("1024"), 1024);
    }

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("1s"), Duration::from_secs(1));
        assert_eq!(parse_duration("500ms"), Duration::from_millis(500));
        assert_eq!(parse_duration("5m"), Duration::from_secs(300));
    }
}
