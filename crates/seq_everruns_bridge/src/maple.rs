use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_SCOPE_NAME: &str = "seq_everruns_bridge";
const DEFAULT_SERVICE_NAME: &str = "seq-everruns-bridge";
const DEFAULT_ENV: &str = "local";
const DEFAULT_QUEUE_CAPACITY: usize = 4096;
const DEFAULT_MAX_BATCH_SIZE: usize = 128;
const DEFAULT_FLUSH_INTERVAL_MS: u64 = 50;
const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 400;
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 800;

#[derive(Debug, Clone)]
pub struct MapleIngestTarget {
    pub traces_endpoint: String,
    pub ingest_key: String,
}

#[derive(Debug, Clone)]
pub struct MapleExporterConfig {
    pub service_name: String,
    pub service_version: Option<String>,
    pub deployment_environment: String,
    pub scope_name: String,
    pub queue_capacity: usize,
    pub max_batch_size: usize,
    pub flush_interval: Duration,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub targets: Vec<MapleIngestTarget>,
}

impl MapleExporterConfig {
    pub fn from_env() -> Result<Option<Self>, MapleConfigError> {
        let targets = parse_targets_from_env()?;
        if targets.is_empty() {
            return Ok(None);
        }

        let service_name = std::env::var("SEQ_EVERRUNS_MAPLE_SERVICE_NAME")
            .ok()
            .and_then(non_empty)
            .unwrap_or_else(|| DEFAULT_SERVICE_NAME.to_string());

        let deployment_environment = std::env::var("SEQ_EVERRUNS_MAPLE_ENV")
            .ok()
            .and_then(non_empty)
            .unwrap_or_else(|| DEFAULT_ENV.to_string());

        let service_version = std::env::var("SEQ_EVERRUNS_MAPLE_SERVICE_VERSION")
            .ok()
            .and_then(non_empty);

        let scope_name = std::env::var("SEQ_EVERRUNS_MAPLE_SCOPE_NAME")
            .ok()
            .and_then(non_empty)
            .unwrap_or_else(|| DEFAULT_SCOPE_NAME.to_string());

        let queue_capacity = env_usize("SEQ_EVERRUNS_MAPLE_QUEUE_CAPACITY")
            .unwrap_or(DEFAULT_QUEUE_CAPACITY)
            .max(1);
        let max_batch_size = env_usize("SEQ_EVERRUNS_MAPLE_MAX_BATCH_SIZE")
            .unwrap_or(DEFAULT_MAX_BATCH_SIZE)
            .max(1);
        let flush_interval = Duration::from_millis(
            env_u64("SEQ_EVERRUNS_MAPLE_FLUSH_INTERVAL_MS").unwrap_or(DEFAULT_FLUSH_INTERVAL_MS),
        );
        let connect_timeout = Duration::from_millis(
            env_u64("SEQ_EVERRUNS_MAPLE_CONNECT_TIMEOUT_MS").unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS),
        );
        let request_timeout = Duration::from_millis(
            env_u64("SEQ_EVERRUNS_MAPLE_REQUEST_TIMEOUT_MS").unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        );

        Ok(Some(Self {
            service_name,
            service_version,
            deployment_environment,
            scope_name,
            queue_capacity,
            max_batch_size,
            flush_interval,
            connect_timeout,
            request_timeout,
            targets,
        }))
    }
}

impl Default for MapleExporterConfig {
    fn default() -> Self {
        Self {
            service_name: DEFAULT_SERVICE_NAME.to_string(),
            service_version: None,
            deployment_environment: DEFAULT_ENV.to_string(),
            scope_name: DEFAULT_SCOPE_NAME.to_string(),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            max_batch_size: DEFAULT_MAX_BATCH_SIZE,
            flush_interval: Duration::from_millis(DEFAULT_FLUSH_INTERVAL_MS),
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            request_timeout: Duration::from_millis(DEFAULT_REQUEST_TIMEOUT_MS),
            targets: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum MapleConfigError {
    #[error("SEQ_EVERRUNS_MAPLE_TRACES_ENDPOINTS count ({endpoints}) does not match SEQ_EVERRUNS_MAPLE_INGEST_KEYS count ({keys})")]
    EndpointKeyCountMismatch { endpoints: usize, keys: usize },
    #[error("{prefix} endpoint/key must both be set")]
    IncompletePair { prefix: &'static str },
}

#[derive(Debug, Clone)]
pub struct MapleSpan {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: String,
    pub name: String,
    pub kind: i32,
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub status_code: i32,
    pub status_message: Option<String>,
    pub attributes: Vec<(String, String)>,
}

impl MapleSpan {
    pub fn for_runtime_event(
        session_id: &str,
        event_id: &str,
        stage: &str,
        ok: bool,
        error: Option<&str>,
        start_time_unix_nano: u64,
        end_time_unix_nano: u64,
        mut extra_attributes: Vec<(String, String)>,
    ) -> Self {
        let trace_id = stable_trace_id(session_id, event_id);
        let span_id = stable_span_id(&format!(
            "{session_id}:{event_id}:{stage}:{start_time_unix_nano}"
        ));
        extra_attributes.push(("session_id".to_string(), session_id.to_string()));
        extra_attributes.push(("event_id".to_string(), event_id.to_string()));
        extra_attributes.push(("stage".to_string(), stage.to_string()));
        extra_attributes.push(("bridge.ok".to_string(), ok.to_string()));
        if let Some(msg) = error {
            extra_attributes.push(("error.message".to_string(), msg.to_string()));
        }

        Self {
            trace_id,
            span_id,
            parent_span_id: String::new(),
            name: format!("everruns.{stage}"),
            kind: 1,
            start_time_unix_nano,
            end_time_unix_nano,
            status_code: if ok { 1 } else { 2 },
            status_message: error.map(|s| s.to_string()),
            attributes: extra_attributes,
        }
    }

    pub fn for_tool_call(
        session_id: &str,
        event_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        seq_op: &str,
        ok: bool,
        error: Option<&str>,
        start_time_unix_nano: u64,
        end_time_unix_nano: u64,
        duration_ms: u64,
    ) -> Self {
        let trace_id = stable_trace_id(session_id, event_id);
        let span_id = stable_span_id(&format!(
            "{session_id}:{event_id}:{tool_call_id}:{start_time_unix_nano}"
        ));

        let mut attributes = vec![
            ("session_id".to_string(), session_id.to_string()),
            ("event_id".to_string(), event_id.to_string()),
            ("tool_call_id".to_string(), tool_call_id.to_string()),
            ("tool_name".to_string(), tool_name.to_string()),
            ("seq_op".to_string(), seq_op.to_string()),
            ("bridge.ok".to_string(), ok.to_string()),
            ("bridge.duration_ms".to_string(), duration_ms.to_string()),
        ];
        if let Some(msg) = error {
            attributes.push(("error.message".to_string(), msg.to_string()));
        }

        Self {
            trace_id,
            span_id,
            parent_span_id: String::new(),
            name: "everruns.tool_call".to_string(),
            kind: 3,
            start_time_unix_nano,
            end_time_unix_nano,
            status_code: if ok { 1 } else { 2 },
            status_message: error.map(|s| s.to_string()),
            attributes,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MapleExporterStats {
    pub enqueued: u64,
    pub sent: u64,
    pub failed: u64,
    pub dropped: u64,
}

#[derive(Default)]
struct MapleExporterStatsAtomic {
    enqueued: AtomicU64,
    sent: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
}

struct WorkerTarget {
    traces_endpoint: String,
    ingest_key: String,
    agent: ureq::Agent,
}

pub struct MapleTraceExporter {
    tx: SyncSender<MapleSpan>,
    stats: Arc<MapleExporterStatsAtomic>,
}

impl MapleTraceExporter {
    pub fn from_env() -> Result<Option<Self>, MapleConfigError> {
        let Some(config) = MapleExporterConfig::from_env()? else {
            return Ok(None);
        };
        Ok(Some(Self::new(config)))
    }

    pub fn new(config: MapleExporterConfig) -> Self {
        let (tx, rx) = sync_channel(config.queue_capacity.max(1));
        let stats = Arc::new(MapleExporterStatsAtomic::default());
        let worker_stats = Arc::clone(&stats);
        thread::spawn(move || worker_main(rx, config, worker_stats));
        Self { tx, stats }
    }

    pub fn emit_span(&self, span: MapleSpan) {
        if self.tx.try_send(span).is_ok() {
            self.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> MapleExporterStats {
        MapleExporterStats {
            enqueued: self.stats.enqueued.load(Ordering::Relaxed),
            sent: self.stats.sent.load(Ordering::Relaxed),
            failed: self.stats.failed.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
        }
    }
}

fn worker_main(
    rx: Receiver<MapleSpan>,
    config: MapleExporterConfig,
    stats: Arc<MapleExporterStatsAtomic>,
) {
    let worker_targets: Vec<WorkerTarget> = config
        .targets
        .iter()
        .map(|target| WorkerTarget {
            traces_endpoint: target.traces_endpoint.clone(),
            ingest_key: target.ingest_key.clone(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(config.connect_timeout)
                .timeout_read(config.request_timeout)
                .timeout_write(config.request_timeout)
                .build(),
        })
        .collect();

    let mut batch = Vec::with_capacity(config.max_batch_size.max(1));
    let mut disconnected = false;

    while !disconnected {
        match rx.recv_timeout(config.flush_interval) {
            Ok(span) => batch.push(span),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                disconnected = true;
            }
        }

        while batch.len() < config.max_batch_size {
            match rx.try_recv() {
                Ok(span) => batch.push(span),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if !batch.is_empty() {
            flush_batch(&config, &worker_targets, &batch, &stats);
            batch.clear();
        }
    }
}

fn flush_batch(
    config: &MapleExporterConfig,
    worker_targets: &[WorkerTarget],
    spans: &[MapleSpan],
    stats: &Arc<MapleExporterStatsAtomic>,
) {
    let spans_payload: Vec<Value> = spans.iter().map(encode_span).collect();
    let resource_attrs = build_resource_attrs(config);
    let payload = json!({
        "resourceSpans": [
            {
                "resource": {
                    "attributes": resource_attrs
                },
                "scopeSpans": [
                    {
                        "scope": { "name": config.scope_name },
                        "spans": spans_payload
                    }
                ]
            }
        ]
    });

    let body = payload.to_string();
    for target in worker_targets {
        let sent = target
            .agent
            .post(&target.traces_endpoint)
            .set("content-type", "application/json")
            .set("x-maple-ingest-key", &target.ingest_key)
            .send_string(&body);

        match sent {
            Ok(resp) if (200..300).contains(&resp.status()) => {
                stats.sent.fetch_add(spans.len() as u64, Ordering::Relaxed);
            }
            Ok(_) | Err(_) => {
                stats
                    .failed
                    .fetch_add(spans.len() as u64, Ordering::Relaxed);
            }
        }
    }
}

fn build_resource_attrs(config: &MapleExporterConfig) -> Vec<Value> {
    let mut attrs = vec![
        otlp_string_attr("service.name", &config.service_name),
        otlp_string_attr("deployment.environment", &config.deployment_environment),
    ];

    if let Some(version) = &config.service_version {
        attrs.push(otlp_string_attr("service.version", version));
    }

    attrs
}

fn encode_span(span: &MapleSpan) -> Value {
    json!({
        "traceId": span.trace_id,
        "spanId": span.span_id,
        "parentSpanId": span.parent_span_id,
        "name": span.name,
        "kind": span.kind,
        "startTimeUnixNano": span.start_time_unix_nano.to_string(),
        "endTimeUnixNano": span.end_time_unix_nano.to_string(),
        "attributes": span
            .attributes
            .iter()
            .map(|(key, value)| otlp_string_attr(key, value))
            .collect::<Vec<Value>>(),
        "status": {
            "code": span.status_code,
            "message": span.status_message.clone().unwrap_or_default()
        }
    })
}

fn otlp_string_attr(key: &str, value: &str) -> Value {
    json!({
        "key": key,
        "value": { "stringValue": value }
    })
}

fn parse_targets_from_env() -> Result<Vec<MapleIngestTarget>, MapleConfigError> {
    let mut targets = Vec::new();

    let local_endpoint = std::env::var("SEQ_EVERRUNS_MAPLE_LOCAL_ENDPOINT")
        .ok()
        .and_then(non_empty);
    let local_key = std::env::var("SEQ_EVERRUNS_MAPLE_LOCAL_INGEST_KEY")
        .ok()
        .and_then(non_empty);
    match (local_endpoint, local_key) {
        (Some(endpoint), Some(key)) => targets.push(MapleIngestTarget {
            traces_endpoint: endpoint,
            ingest_key: key,
        }),
        (None, None) => {}
        _ => {
            return Err(MapleConfigError::IncompletePair {
                prefix: "SEQ_EVERRUNS_MAPLE_LOCAL",
            });
        }
    }

    let hosted_endpoint = std::env::var("SEQ_EVERRUNS_MAPLE_HOSTED_ENDPOINT")
        .ok()
        .and_then(non_empty);
    let hosted_key = std::env::var("SEQ_EVERRUNS_MAPLE_HOSTED_INGEST_KEY")
        .ok()
        .and_then(non_empty);
    match (hosted_endpoint, hosted_key) {
        (Some(endpoint), Some(key)) => targets.push(MapleIngestTarget {
            traces_endpoint: endpoint,
            ingest_key: key,
        }),
        (None, None) => {}
        _ => {
            return Err(MapleConfigError::IncompletePair {
                prefix: "SEQ_EVERRUNS_MAPLE_HOSTED",
            });
        }
    }

    let csv_endpoints = split_csv_env("SEQ_EVERRUNS_MAPLE_TRACES_ENDPOINTS");
    let csv_keys = split_csv_env("SEQ_EVERRUNS_MAPLE_INGEST_KEYS");
    if !csv_endpoints.is_empty() || !csv_keys.is_empty() {
        if csv_endpoints.len() != csv_keys.len() {
            return Err(MapleConfigError::EndpointKeyCountMismatch {
                endpoints: csv_endpoints.len(),
                keys: csv_keys.len(),
            });
        }
        for (endpoint, key) in csv_endpoints.into_iter().zip(csv_keys.into_iter()) {
            targets.push(MapleIngestTarget {
                traces_endpoint: endpoint,
                ingest_key: key,
            });
        }
    }

    Ok(dedup_targets(targets))
}

fn split_csv_env(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|raw| raw.split(',').filter_map(non_empty).collect())
        .unwrap_or_default()
}

fn dedup_targets(targets: Vec<MapleIngestTarget>) -> Vec<MapleIngestTarget> {
    let mut out: Vec<MapleIngestTarget> = Vec::new();
    for target in targets {
        let exists = out.iter().any(|existing| {
            existing.traces_endpoint == target.traces_endpoint
                && existing.ingest_key == target.ingest_key
        });
        if !exists {
            out.push(target);
        }
    }
    out
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn non_empty(s: impl AsRef<str>) -> Option<String> {
    let value = s.as_ref().trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

pub fn stable_trace_id(session_id: &str, event_id: &str) -> String {
    let a = fnv1a64(session_id.as_bytes());
    let b = fnv1a64(event_id.as_bytes());
    format!("{a:016x}{b:016x}")
}

pub fn stable_span_id(seed: &str) -> String {
    format!("{:016x}", fnv1a64(seed.as_bytes()))
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in data {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::time::Duration;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn unset_maple_envs() {
        let keys = [
            "SEQ_EVERRUNS_MAPLE_LOCAL_ENDPOINT",
            "SEQ_EVERRUNS_MAPLE_LOCAL_INGEST_KEY",
            "SEQ_EVERRUNS_MAPLE_HOSTED_ENDPOINT",
            "SEQ_EVERRUNS_MAPLE_HOSTED_INGEST_KEY",
            "SEQ_EVERRUNS_MAPLE_TRACES_ENDPOINTS",
            "SEQ_EVERRUNS_MAPLE_INGEST_KEYS",
            "SEQ_EVERRUNS_MAPLE_SERVICE_NAME",
            "SEQ_EVERRUNS_MAPLE_SERVICE_VERSION",
            "SEQ_EVERRUNS_MAPLE_ENV",
            "SEQ_EVERRUNS_MAPLE_SCOPE_NAME",
            "SEQ_EVERRUNS_MAPLE_QUEUE_CAPACITY",
            "SEQ_EVERRUNS_MAPLE_MAX_BATCH_SIZE",
            "SEQ_EVERRUNS_MAPLE_FLUSH_INTERVAL_MS",
            "SEQ_EVERRUNS_MAPLE_CONNECT_TIMEOUT_MS",
            "SEQ_EVERRUNS_MAPLE_REQUEST_TIMEOUT_MS",
        ];
        for key in keys {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn stable_ids_have_expected_length() {
        let trace_id = stable_trace_id("session-1", "event-1");
        let span_id = stable_span_id("session-1:event-1:tc1");
        assert_eq!(trace_id.len(), 32);
        assert_eq!(span_id.len(), 16);
    }

    #[test]
    fn reads_dual_target_env_config() {
        let _guard = ENV_LOCK.lock().expect("lock env");
        unset_maple_envs();
        std::env::set_var(
            "SEQ_EVERRUNS_MAPLE_LOCAL_ENDPOINT",
            "http://ingest.maple.localhost/v1/traces",
        );
        std::env::set_var("SEQ_EVERRUNS_MAPLE_LOCAL_INGEST_KEY", "maple_pk_local");
        std::env::set_var(
            "SEQ_EVERRUNS_MAPLE_HOSTED_ENDPOINT",
            "https://ingest.1focus.ai/v1/traces",
        );
        std::env::set_var("SEQ_EVERRUNS_MAPLE_HOSTED_INGEST_KEY", "maple_pk_hosted");

        let cfg = MapleExporterConfig::from_env()
            .expect("env parse")
            .expect("config should exist");
        assert_eq!(cfg.targets.len(), 2);
        assert!(cfg
            .targets
            .iter()
            .any(|t| t.traces_endpoint == "http://ingest.maple.localhost/v1/traces"));
        assert!(cfg
            .targets
            .iter()
            .any(|t| t.traces_endpoint == "https://ingest.1focus.ai/v1/traces"));
        unset_maple_envs();
    }

    #[test]
    fn csv_target_env_mismatch_returns_error() {
        let _guard = ENV_LOCK.lock().expect("lock env");
        unset_maple_envs();
        std::env::set_var(
            "SEQ_EVERRUNS_MAPLE_TRACES_ENDPOINTS",
            "http://ingest.maple.localhost/v1/traces,https://ingest.1focus.ai/v1/traces",
        );
        std::env::set_var("SEQ_EVERRUNS_MAPLE_INGEST_KEYS", "maple_pk_only_one");

        let err = MapleExporterConfig::from_env().expect_err("mismatch should error");
        assert!(matches!(
            err,
            MapleConfigError::EndpointKeyCountMismatch { .. }
        ));
        unset_maple_envs();
    }

    #[test]
    fn incomplete_local_pair_returns_error() {
        let _guard = ENV_LOCK.lock().expect("lock env");
        unset_maple_envs();
        std::env::set_var(
            "SEQ_EVERRUNS_MAPLE_LOCAL_ENDPOINT",
            "http://ingest.maple.localhost/v1/traces",
        );

        let err = MapleExporterConfig::from_env().expect_err("incomplete pair should error");
        assert!(matches!(
            err,
            MapleConfigError::IncompletePair {
                prefix: "SEQ_EVERRUNS_MAPLE_LOCAL"
            }
        ));
        unset_maple_envs();
    }

    #[test]
    fn exporter_sends_span_to_ingest_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");

        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req = [0_u8; 8192];
                let _ = stream.read(&mut req);
                let response =
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
                let _ = stream.write_all(response);
                let _ = stream.flush();
            }
        });

        let config = MapleExporterConfig {
            service_name: "seq-everruns-bridge-test".to_string(),
            service_version: None,
            deployment_environment: "test".to_string(),
            scope_name: "seq_everruns_bridge".to_string(),
            queue_capacity: 32,
            max_batch_size: 8,
            flush_interval: Duration::from_millis(10),
            connect_timeout: Duration::from_millis(200),
            request_timeout: Duration::from_millis(200),
            targets: vec![MapleIngestTarget {
                traces_endpoint: format!("http://{addr}/v1/traces"),
                ingest_key: "maple_pk_test".to_string(),
            }],
        };

        let exporter = MapleTraceExporter::new(config);
        let span = MapleSpan::for_tool_call(
            "session-1",
            "event-1",
            "tool-1",
            "seq_ping",
            "ping",
            true,
            None,
            1_739_890_000_000_000_000,
            1_739_890_000_100_000_000,
            100,
        );
        exporter.emit_span(span);

        std::thread::sleep(Duration::from_millis(80));
        let stats = exporter.stats();
        assert!(stats.sent >= 1, "expected at least one sent span");
        let _ = server.join();
    }
}
