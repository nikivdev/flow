//! Agent-readable trace summary.
//!
//! Writes a JSON file that AI agents (like Claude Code) can read to understand
//! the current state of the application during development.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::trace::{TraceBuffer, TraceRecord};

/// Summary of a single error for AI consumption
#[derive(Debug, Clone, Serialize)]
pub struct ErrorSummary {
    pub time: String,
    pub req_id: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub latency_ms: u32,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

/// Summary of a slow request for AI consumption
#[derive(Debug, Clone, Serialize)]
pub struct SlowRequestSummary {
    pub time: String,
    pub req_id: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub latency_ms: u32,
    pub upstream_latency_ms: u32,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Health status for a target/provider
#[derive(Debug, Clone, Serialize)]
pub struct TargetHealth {
    pub healthy: bool,
    pub total_requests: u64,
    pub error_count: u64,
    pub error_rate: String,
    pub avg_latency_ms: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_time: Option<String>,
}

/// Session statistics
#[derive(Debug, Clone, Serialize)]
pub struct SessionStats {
    pub started: u64,
    pub started_human: String,
    pub uptime_seconds: u64,
    pub total_requests: u64,
    pub total_errors: u64,
    pub error_rate: String,
    pub avg_latency_ms: u32,
    pub p99_latency_ms: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// The complete trace summary (written to JSON)
#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    pub last_updated: u64,
    pub last_updated_human: String,
    pub session: SessionStats,
    pub recent_errors: Vec<ErrorSummary>,
    pub slow_requests: Vec<SlowRequestSummary>,
    pub target_health: HashMap<String, TargetHealth>,
    pub request_patterns: HashMap<String, u64>,
}

/// State for computing summaries
pub struct SummaryState {
    pub targets: Vec<String>,
    pub error_bodies: RwLock<HashMap<u64, String>>,
    pub slow_threshold_ms: u32,
    pub session_start: Instant,
    pub session_start_unix: u64,
}

impl SummaryState {
    pub fn new(targets: Vec<String>, slow_threshold_ms: u32) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            targets,
            error_bodies: RwLock::new(HashMap::new()),
            slow_threshold_ms,
            session_start: Instant::now(),
            session_start_unix: now,
        }
    }

    /// Store an error response body for a request ID
    pub fn store_error_body(&self, req_id: u64, body: String) {
        if let Ok(mut bodies) = self.error_bodies.write() {
            // Keep only last 100 error bodies
            if bodies.len() > 100 {
                // Remove oldest entries (this is O(n) but rare)
                let to_remove: Vec<_> = bodies.keys().take(50).copied().collect();
                for k in to_remove {
                    bodies.remove(&k);
                }
            }
            bodies.insert(req_id, body);
        }
    }

    /// Get error body for a request ID
    pub fn get_error_body(&self, req_id: u64) -> Option<String> {
        self.error_bodies
            .read()
            .ok()
            .and_then(|b| b.get(&req_id).cloned())
    }

    /// Get target name by index
    pub fn target_name(&self, idx: u8) -> &str {
        self.targets
            .get(idx as usize)
            .map(|s| s.as_str())
            .unwrap_or("unknown")
    }
}

/// Compute a summary from the trace buffer
pub fn compute_summary(buffer: &TraceBuffer, state: &SummaryState) -> TraceSummary {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let records = buffer.recent(1000);

    // Compute session stats
    let total_requests = buffer.write_index();
    let total_errors = records.iter().filter(|r| r.is_error()).count() as u64;
    let error_rate = if total_requests > 0 {
        format!(
            "{:.1}%",
            (total_errors as f64 / records.len() as f64) * 100.0
        )
    } else {
        "0%".to_string()
    };

    let latencies: Vec<u32> = records.iter().map(|r| r.latency_us() / 1000).collect();
    let avg_latency_ms = if !latencies.is_empty() {
        (latencies.iter().map(|&l| l as u64).sum::<u64>() / latencies.len() as u64) as u32
    } else {
        0
    };

    let p99_latency_ms = if !latencies.is_empty() {
        let mut sorted = latencies.clone();
        sorted.sort();
        let p99_idx = (sorted.len() as f64 * 0.99) as usize;
        sorted
            .get(p99_idx.min(sorted.len() - 1))
            .copied()
            .unwrap_or(0)
    } else {
        0
    };

    let bytes_in: u64 = records.iter().map(|r| r.bytes_in() as u64).sum();
    let bytes_out: u64 = records.iter().map(|r| r.bytes_out() as u64).sum();

    let uptime = state.session_start.elapsed().as_secs();

    let session = SessionStats {
        started: state.session_start_unix,
        started_human: format_timestamp(state.session_start_unix),
        uptime_seconds: uptime,
        total_requests,
        total_errors,
        error_rate,
        avg_latency_ms,
        p99_latency_ms,
        bytes_in,
        bytes_out,
    };

    // Recent errors (last 10)
    let recent_errors: Vec<ErrorSummary> = records
        .iter()
        .filter(|r| r.is_error())
        .take(10)
        .map(|r| {
            let error_body = state.get_error_body(r.req_id());
            let suggestion = suggest_fix(r, error_body.as_deref());

            ErrorSummary {
                time: format_relative_time(r.timestamp(), buffer.start_time()),
                req_id: format!("{:x}", r.req_id()),
                method: format!("{:?}", r.method()),
                path: r.path().to_string(),
                status: r.status(),
                latency_ms: r.latency_us() / 1000,
                target: state.target_name(r.target_idx()).to_string(),
                error_body,
                suggestion,
            }
        })
        .collect();

    // Slow requests (last 10, > threshold)
    let slow_requests: Vec<SlowRequestSummary> = records
        .iter()
        .filter(|r| r.is_slow(state.slow_threshold_ms))
        .take(10)
        .map(|r| {
            let reason = if r.upstream_latency_us() > state.slow_threshold_ms * 1000 * 80 / 100 {
                Some("Upstream response slow".to_string())
            } else {
                None
            };

            SlowRequestSummary {
                time: format_relative_time(r.timestamp(), buffer.start_time()),
                req_id: format!("{:x}", r.req_id()),
                method: format!("{:?}", r.method()),
                path: r.path().to_string(),
                status: r.status(),
                latency_ms: r.latency_us() / 1000,
                upstream_latency_ms: r.upstream_latency_us() / 1000,
                target: state.target_name(r.target_idx()).to_string(),
                reason,
            }
        })
        .collect();

    // Target health
    let mut target_health: HashMap<String, TargetHealth> = HashMap::new();
    for target in &state.targets {
        target_health.insert(
            target.clone(),
            TargetHealth {
                healthy: true,
                total_requests: 0,
                error_count: 0,
                error_rate: "0%".to_string(),
                avg_latency_ms: 0,
                last_error: None,
                last_error_time: None,
            },
        );
    }

    // Compute per-target stats
    let mut target_latencies: HashMap<u8, Vec<u32>> = HashMap::new();
    let mut target_errors: HashMap<u8, (u64, Option<(String, u64)>)> = HashMap::new();
    let mut target_counts: HashMap<u8, u64> = HashMap::new();

    for r in &records {
        let idx = r.target_idx();
        *target_counts.entry(idx).or_insert(0) += 1;
        target_latencies
            .entry(idx)
            .or_insert_with(Vec::new)
            .push(r.latency_us() / 1000);

        if r.is_error() {
            let entry = target_errors.entry(idx).or_insert((0, None));
            entry.0 += 1;
            if entry.1.is_none() {
                entry.1 = Some((format!("{} {}", r.status(), r.path()), r.timestamp()));
            }
        }
    }

    for (idx, count) in target_counts {
        let target_name = state.target_name(idx);
        if let Some(health) = target_health.get_mut(target_name) {
            health.total_requests = count;

            if let Some(latencies) = target_latencies.get(&idx) {
                if !latencies.is_empty() {
                    health.avg_latency_ms = (latencies.iter().map(|&l| l as u64).sum::<u64>()
                        / latencies.len() as u64)
                        as u32;
                }
            }

            if let Some((error_count, last_error)) = target_errors.get(&idx) {
                health.error_count = *error_count;
                health.error_rate = format!("{:.1}%", (*error_count as f64 / count as f64) * 100.0);
                health.healthy = (*error_count as f64 / count as f64) < 0.1; // < 10% errors

                if let Some((err, ts)) = last_error {
                    health.last_error = Some(err.clone());
                    health.last_error_time = Some(format_relative_time(*ts, buffer.start_time()));
                }
            }
        }
    }

    // Request patterns (path -> count)
    let mut request_patterns: HashMap<String, u64> = HashMap::new();
    for r in &records {
        // Normalize path (remove query params, truncate)
        let path = r.path().split('?').next().unwrap_or("").to_string();
        *request_patterns.entry(path).or_insert(0) += 1;
    }

    // Keep only top 20 patterns
    let mut patterns: Vec<_> = request_patterns.into_iter().collect();
    patterns.sort_by(|a, b| b.1.cmp(&a.1));
    let request_patterns: HashMap<String, u64> = patterns.into_iter().take(20).collect();

    TraceSummary {
        last_updated: now,
        last_updated_human: format_timestamp(now),
        session,
        recent_errors,
        slow_requests,
        target_health,
        request_patterns,
    }
}

/// Write summary to a JSON file
pub fn write_summary(summary: &TraceSummary, path: &PathBuf) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(summary)?;
    fs::write(path, json)
}

/// Get the default summary file path
pub fn default_summary_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flow")
        .join("proxy")
        .join("trace-summary.json")
}

// Helper: format Unix timestamp as human-readable
fn format_timestamp(ts: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(ts);
    // Simple format without chrono dependency
    format!("{:?}", dt)
}

// Helper: format nanosecond timestamp relative to start
fn format_relative_time(ts_ns: u64, start: Instant) -> String {
    // Convert to wall clock time (approximate)
    let elapsed = start.elapsed();
    let now_ns = elapsed.as_nanos() as u64;

    if ts_ns > now_ns {
        return "now".to_string();
    }

    let diff_ns = now_ns - ts_ns;
    let diff_secs = diff_ns / 1_000_000_000;

    if diff_secs < 60 {
        format!("{}s ago", diff_secs)
    } else if diff_secs < 3600 {
        format!("{}m ago", diff_secs / 60)
    } else {
        format!("{}h ago", diff_secs / 3600)
    }
}

// Helper: suggest a fix based on error
fn suggest_fix(record: &TraceRecord, error_body: Option<&str>) -> Option<String> {
    let status = record.status();
    let path = record.path();

    // Parse error body for common patterns
    if let Some(body) = error_body {
        if body.contains("ParseError") || body.contains("validation") {
            return Some("Check request body schema matches expected format".to_string());
        }
        if body.contains("timeout") {
            return Some("Upstream service timed out - check service health".to_string());
        }
        if body.contains("ECONNREFUSED") {
            return Some("Upstream service not running - start the service".to_string());
        }
        if body.contains("token") || body.contains("auth") || body.contains("unauthorized") {
            return Some("Authentication failed - check credentials/token".to_string());
        }
    }

    // Fallback suggestions based on status code
    match status {
        400 => Some("Bad request - check request parameters".to_string()),
        401 => Some("Unauthorized - check authentication".to_string()),
        403 => Some("Forbidden - check permissions".to_string()),
        404 => Some(format!("Not found - verify endpoint '{}' exists", path)),
        500 => Some("Internal server error - check server logs".to_string()),
        502 => Some("Bad gateway - upstream service may be down".to_string()),
        503 => Some("Service unavailable - service may be overloaded".to_string()),
        504 => Some("Gateway timeout - upstream service too slow".to_string()),
        _ => None,
    }
}

/// Background task that periodically updates the summary file
pub struct SummaryWriter {
    buffer: Arc<TraceBuffer>,
    state: Arc<SummaryState>,
    path: PathBuf,
    interval: Duration,
}

impl SummaryWriter {
    pub fn new(
        buffer: Arc<TraceBuffer>,
        state: Arc<SummaryState>,
        path: PathBuf,
        interval: Duration,
    ) -> Self {
        Self {
            buffer,
            state,
            path,
            interval,
        }
    }

    /// Run the summary writer (blocking)
    pub fn run(&self) {
        loop {
            let summary = compute_summary(&self.buffer, &self.state);
            if let Err(e) = write_summary(&summary, &self.path) {
                eprintln!("Failed to write trace summary: {}", e);
            }
            std::thread::sleep(self.interval);
        }
    }

    /// Spawn as a background thread
    pub fn spawn(self) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || self.run())
    }
}
