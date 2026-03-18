use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use seq_everruns_bridge::maple::{
    MapleExporterConfig, MapleIngestTarget, MapleSpan, MapleTraceExporter,
};
use serde::{Deserialize, Serialize};

use crate::codex_skill_eval::{self, CodexSkillEvalEvent, CodexSkillOutcomeEvent};
use crate::config;
use crate::env as flow_env;

const CODEX_MAPLE_DEFAULT_SERVICE_NAME: &str = "flow-codex";
const CODEX_MAPLE_DEFAULT_SCOPE_NAME: &str = "flow.codex";
const CODEX_MAPLE_DEFAULT_ENV: &str = "local";
const CODEX_MAPLE_DEFAULT_QUEUE_CAPACITY: usize = 1024;
const CODEX_MAPLE_DEFAULT_MAX_BATCH_SIZE: usize = 64;
const CODEX_MAPLE_DEFAULT_FLUSH_INTERVAL_MS: u64 = 100;
const CODEX_MAPLE_DEFAULT_CONNECT_TIMEOUT_MS: u64 = 400;
const CODEX_MAPLE_DEFAULT_REQUEST_TIMEOUT_MS: u64 = 800;
const DEFAULT_MAPLE_MCP_ENDPOINT: &str = "https://api.maple.dev/mcp";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CodexTelemetryExportState {
    version: u32,
    events_offset: u64,
    outcomes_offset: u64,
    events_exported: u64,
    outcomes_exported: u64,
    last_exported_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTelemetryStatus {
    pub enabled: bool,
    pub configured_targets: usize,
    pub service_name: String,
    pub scope_name: String,
    pub state_path: String,
    pub events_path: String,
    pub outcomes_path: String,
    pub events_offset: u64,
    pub outcomes_offset: u64,
    pub events_exported: u64,
    pub outcomes_exported: u64,
    pub last_exported_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTelemetryFlushSummary {
    pub enabled: bool,
    pub configured_targets: usize,
    pub events_seen: usize,
    pub outcomes_seen: usize,
    pub events_exported: usize,
    pub outcomes_exported: usize,
    pub state_path: String,
    pub last_exported_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTraceStatus {
    pub enabled: bool,
    pub endpoint: String,
    pub token_source: String,
    pub tools_list_ok: bool,
    pub tools_count: usize,
    pub read_probe_ok: bool,
    pub read_probe_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexTraceInspectResult {
    pub trace_id: String,
    pub endpoint: String,
    pub token_source: String,
    pub flushed: bool,
    pub result: Option<serde_json::Value>,
    pub read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexCurrentSessionTrace {
    pub trace_id: String,
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub workflow_kind: Option<String>,
    pub service_name: Option<String>,
    pub flushed: bool,
    pub endpoint: String,
    pub token_source: String,
    pub result: Option<serde_json::Value>,
    pub read_error: Option<String>,
}

#[derive(Debug, Clone)]
struct MapleReadConfig {
    endpoint: String,
    token: String,
    token_source: String,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn telemetry_env_keys() -> Vec<String> {
    [
        "FLOW_CODEX_MAPLE_LOCAL_ENDPOINT",
        "FLOW_CODEX_MAPLE_LOCAL_INGEST_KEY",
        "FLOW_CODEX_MAPLE_HOSTED_ENDPOINT",
        "FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY",
        "FLOW_CODEX_MAPLE_TRACES_ENDPOINTS",
        "FLOW_CODEX_MAPLE_INGEST_KEYS",
        "FLOW_CODEX_MAPLE_SERVICE_NAME",
        "FLOW_CODEX_MAPLE_SERVICE_VERSION",
        "FLOW_CODEX_MAPLE_SCOPE_NAME",
        "FLOW_CODEX_MAPLE_ENV",
        "FLOW_CODEX_MAPLE_QUEUE_CAPACITY",
        "FLOW_CODEX_MAPLE_MAX_BATCH_SIZE",
        "FLOW_CODEX_MAPLE_FLUSH_INTERVAL_MS",
        "FLOW_CODEX_MAPLE_CONNECT_TIMEOUT_MS",
        "FLOW_CODEX_MAPLE_REQUEST_TIMEOUT_MS",
        "MAPLE_API_TOKEN",
        "MAPLE_MCP_URL",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn maple_target_env_keys() -> &'static [&'static str] {
    &[
        "FLOW_CODEX_MAPLE_LOCAL_ENDPOINT",
        "FLOW_CODEX_MAPLE_LOCAL_INGEST_KEY",
        "FLOW_CODEX_MAPLE_HOSTED_ENDPOINT",
        "FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY",
        "FLOW_CODEX_MAPLE_TRACES_ENDPOINTS",
        "FLOW_CODEX_MAPLE_INGEST_KEYS",
    ]
}

fn shell_has_explicit_maple_target_env() -> bool {
    maple_target_env_keys()
        .iter()
        .any(|key| env_non_empty(key).is_some())
}

fn env_non_empty_with_store(
    key: &str,
    personal_env: &mut Option<Option<std::collections::HashMap<String, String>>>,
) -> Option<String> {
    if let Some(value) = env_non_empty(key) {
        return Some(value);
    }
    if personal_env.is_none() {
        *personal_env = Some(flow_env::fetch_local_personal_env_vars(&telemetry_env_keys()).ok());
    }
    personal_env
        .as_ref()
        .and_then(|cached| cached.as_ref())
        .and_then(|values| values.get(key))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
}

fn telemetry_state_path() -> Result<PathBuf> {
    let root = config::ensure_global_state_dir()?.join("codex");
    fs::create_dir_all(&root)?;
    Ok(root.join("telemetry-export-state.json"))
}

fn load_state() -> Result<CodexTelemetryExportState> {
    let path = telemetry_state_path()?;
    if !path.exists() {
        return Ok(CodexTelemetryExportState {
            version: 1,
            ..Default::default()
        });
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut state: CodexTelemetryExportState =
        serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    if state.version == 0 {
        state.version = 1;
    }
    Ok(state)
}

fn save_state(state: &CodexTelemetryExportState) -> Result<()> {
    let path = telemetry_state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        &path,
        serde_json::to_vec_pretty(state).context("failed to encode telemetry state")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn parse_maple_exporter_config_from_env() -> Result<Option<MapleExporterConfig>> {
    let allow_store_fallback = !shell_has_explicit_maple_target_env();
    let mut personal_env = if allow_store_fallback {
        None
    } else {
        Some(None)
    };
    let mut targets = Vec::new();

    match (
        env_non_empty_with_store("FLOW_CODEX_MAPLE_LOCAL_ENDPOINT", &mut personal_env),
        env_non_empty_with_store("FLOW_CODEX_MAPLE_LOCAL_INGEST_KEY", &mut personal_env),
    ) {
        (Some(endpoint), Some(key)) => targets.push(MapleIngestTarget {
            traces_endpoint: endpoint,
            ingest_key: key,
        }),
        (None, None) => {}
        _ => anyhow::bail!("FLOW_CODEX_MAPLE_LOCAL_ENDPOINT and FLOW_CODEX_MAPLE_LOCAL_INGEST_KEY must both be set"),
    }

    match (
        env_non_empty_with_store("FLOW_CODEX_MAPLE_HOSTED_ENDPOINT", &mut personal_env),
        env_non_empty_with_store("FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY", &mut personal_env),
    ) {
        (Some(endpoint), Some(key)) => targets.push(MapleIngestTarget {
            traces_endpoint: endpoint,
            ingest_key: key,
        }),
        (None, None) => {}
        _ => anyhow::bail!("FLOW_CODEX_MAPLE_HOSTED_ENDPOINT and FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY must both be set"),
    }

    let csv_endpoints = env_non_empty_with_store("FLOW_CODEX_MAPLE_TRACES_ENDPOINTS", &mut personal_env)
        .map(|raw| {
            raw.split(',')
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let csv_keys = env_non_empty_with_store("FLOW_CODEX_MAPLE_INGEST_KEYS", &mut personal_env)
        .map(|raw| {
            raw.split(',')
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !csv_endpoints.is_empty() || !csv_keys.is_empty() {
        if csv_endpoints.len() != csv_keys.len() {
            anyhow::bail!(
                "FLOW_CODEX_MAPLE_TRACES_ENDPOINTS count ({}) does not match FLOW_CODEX_MAPLE_INGEST_KEYS count ({})",
                csv_endpoints.len(),
                csv_keys.len()
            );
        }
        for (endpoint, key) in csv_endpoints.into_iter().zip(csv_keys.into_iter()) {
            targets.push(MapleIngestTarget {
                traces_endpoint: endpoint,
                ingest_key: key,
            });
        }
    }

    if targets.is_empty() {
        return Ok(None);
    }

    targets.dedup_by(|a, b| {
        a.traces_endpoint == b.traces_endpoint && a.ingest_key == b.ingest_key
    });

    Ok(Some(MapleExporterConfig {
        service_name: env_non_empty("FLOW_CODEX_MAPLE_SERVICE_NAME")
            .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_SERVICE_NAME.to_string()),
        service_version: env_non_empty("FLOW_CODEX_MAPLE_SERVICE_VERSION"),
        deployment_environment: env_non_empty("FLOW_CODEX_MAPLE_ENV")
            .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_ENV.to_string()),
        scope_name: env_non_empty("FLOW_CODEX_MAPLE_SCOPE_NAME")
            .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_SCOPE_NAME.to_string()),
        queue_capacity: env_non_empty("FLOW_CODEX_MAPLE_QUEUE_CAPACITY")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(CODEX_MAPLE_DEFAULT_QUEUE_CAPACITY)
            .max(1),
        max_batch_size: env_non_empty("FLOW_CODEX_MAPLE_MAX_BATCH_SIZE")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(CODEX_MAPLE_DEFAULT_MAX_BATCH_SIZE)
            .max(1),
        flush_interval: std::time::Duration::from_millis(
            env_non_empty("FLOW_CODEX_MAPLE_FLUSH_INTERVAL_MS")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(CODEX_MAPLE_DEFAULT_FLUSH_INTERVAL_MS),
        ),
        connect_timeout: std::time::Duration::from_millis(
            env_non_empty("FLOW_CODEX_MAPLE_CONNECT_TIMEOUT_MS")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(CODEX_MAPLE_DEFAULT_CONNECT_TIMEOUT_MS),
        ),
        request_timeout: std::time::Duration::from_millis(
            env_non_empty("FLOW_CODEX_MAPLE_REQUEST_TIMEOUT_MS")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(CODEX_MAPLE_DEFAULT_REQUEST_TIMEOUT_MS),
        ),
        targets,
    }))
}

fn parse_maple_read_config_from_env() -> Result<Option<MapleReadConfig>> {
    let allow_store_fallback = env_non_empty("MAPLE_API_TOKEN").is_none()
        && env_non_empty("FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY").is_none();
    let mut personal_env = if allow_store_fallback {
        None
    } else {
        Some(None)
    };
    let shell_token = env_non_empty("MAPLE_API_TOKEN")
        .map(|value| (value, "shell".to_string()))
        .or_else(|| {
            env_non_empty("FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY")
                .map(|value| (value, "shell-ingest-key".to_string()))
        });
    let store_token = env_non_empty_with_store("MAPLE_API_TOKEN", &mut personal_env)
        .map(|value| (value, "flow-personal-env".to_string()))
        .or_else(|| {
            env_non_empty_with_store("FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY", &mut personal_env)
                .map(|value| (value, "flow-personal-ingest-key".to_string()))
        });
    let token = shell_token.or(store_token);
    let endpoint = env_non_empty_with_store("MAPLE_MCP_URL", &mut personal_env)
        .unwrap_or_else(|| DEFAULT_MAPLE_MCP_ENDPOINT.to_string());
    let Some((token, token_source)) = token else {
        return Ok(None);
    };
    Ok(Some(MapleReadConfig {
        endpoint,
        token,
        token_source,
        connect_timeout_ms: env_non_empty("FLOW_CODEX_MAPLE_CONNECT_TIMEOUT_MS")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(CODEX_MAPLE_DEFAULT_CONNECT_TIMEOUT_MS),
        request_timeout_ms: env_non_empty("FLOW_CODEX_MAPLE_REQUEST_TIMEOUT_MS")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(CODEX_MAPLE_DEFAULT_REQUEST_TIMEOUT_MS),
    }))
}

fn maple_json_rpc_request(
    config: &MapleReadConfig,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let client = Client::builder()
        .connect_timeout(std::time::Duration::from_millis(config.connect_timeout_ms))
        .timeout(std::time::Duration::from_millis(config.request_timeout_ms))
        .build()
        .context("failed to build Maple MCP client")?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let response = client
        .post(&config.endpoint)
        .bearer_auth(&config.token)
        .json(&request)
        .send()
        .with_context(|| format!("failed to reach Maple MCP at {}", config.endpoint))?;
    let status = response.status();
    let payload: serde_json::Value = response
        .json()
        .context("failed to parse Maple MCP response JSON")?;
    if !status.is_success() {
        anyhow::bail!(
            "Maple MCP request failed ({}): {}",
            status,
            serde_json::to_string(&payload).unwrap_or_else(|_| "unparseable error body".to_string())
        );
    }
    let envelope = if let Some(items) = payload.as_array() {
        items.first().cloned().unwrap_or(serde_json::Value::Null)
    } else {
        payload
    };
    if let Some(error) = envelope.get("error") {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown Maple MCP error");
        anyhow::bail!("Maple MCP error {code}: {message}");
    }
    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Maple MCP response did not include a result payload"))
}

fn maple_tool_result_error(result: &serde_json::Value) -> Option<String> {
    if result.get("isError").and_then(|value| value.as_bool()) != Some(true) {
        return None;
    }
    result
        .get("content")
        .and_then(|value| value.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| Some("Maple tool returned an unspecified error".to_string()))
}

fn maple_call_tool(
    config: &MapleReadConfig,
    name: &str,
    arguments: serde_json::Value,
) -> Result<serde_json::Value> {
    let result = maple_json_rpc_request(
        config,
        "tools/call",
        serde_json::json!({
            "name": name,
            "arguments": arguments,
        }),
    )?;
    if let Some(error) = maple_tool_result_error(&result) {
        anyhow::bail!("{error}");
    }
    Ok(result)
}

pub fn status() -> Result<CodexTelemetryStatus> {
    let config = parse_maple_exporter_config_from_env()?;
    let state = load_state()?;
    let state_path = telemetry_state_path()?;
    let events_path = codex_skill_eval::events_log_path()?;
    let outcomes_path = codex_skill_eval::outcomes_log_path()?;

    Ok(CodexTelemetryStatus {
        enabled: config.is_some(),
        configured_targets: config.as_ref().map(|value| value.targets.len()).unwrap_or(0),
        service_name: config
            .as_ref()
            .map(|value| value.service_name.clone())
            .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_SERVICE_NAME.to_string()),
        scope_name: config
            .as_ref()
            .map(|value| value.scope_name.clone())
            .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_SCOPE_NAME.to_string()),
        state_path: state_path.display().to_string(),
        events_path: events_path.display().to_string(),
        outcomes_path: outcomes_path.display().to_string(),
        events_offset: state.events_offset,
        outcomes_offset: state.outcomes_offset,
        events_exported: state.events_exported,
        outcomes_exported: state.outcomes_exported,
        last_exported_at_unix: state.last_exported_at_unix,
    })
}

pub fn trace_status() -> Result<CodexTraceStatus> {
    let Some(config) = parse_maple_read_config_from_env()? else {
        return Ok(CodexTraceStatus {
            enabled: false,
            endpoint: DEFAULT_MAPLE_MCP_ENDPOINT.to_string(),
            token_source: "missing".to_string(),
            tools_list_ok: false,
            tools_count: 0,
            read_probe_ok: false,
            read_probe_error: Some("MAPLE_API_TOKEN is not configured".to_string()),
        });
    };
    let list_result = maple_json_rpc_request(&config, "tools/list", serde_json::json!({}))?;
    let tools = list_result
        .get("tools")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let read_probe = maple_json_rpc_request(
        &config,
        "tools/call",
        serde_json::json!({
            "name": "system_health",
            "arguments": {},
        }),
    );
    Ok(CodexTraceStatus {
        enabled: true,
        endpoint: config.endpoint,
        token_source: config.token_source,
        tools_list_ok: true,
        tools_count: tools.len(),
        read_probe_ok: read_probe
            .as_ref()
            .ok()
            .and_then(maple_tool_result_error)
            .is_none(),
        read_probe_error: match read_probe {
            Ok(value) => maple_tool_result_error(&value),
            Err(error) => Some(error.to_string()),
        },
    })
}

pub fn inspect_trace(trace_id: &str, flush_first: bool) -> Result<CodexTraceInspectResult> {
    let Some(config) = parse_maple_read_config_from_env()? else {
        anyhow::bail!("MAPLE_API_TOKEN is not configured");
    };
    let flushed = if flush_first {
        let _ = flush(64);
        true
    } else {
        false
    };
    let result = maple_call_tool(
        &config,
        "inspect_trace",
        serde_json::json!({
            "trace_id": trace_id,
        }),
    );
    let (result, read_error) = match result {
        Ok(result) => (Some(result), None),
        Err(error) => (None, Some(error.to_string())),
    };
    Ok(CodexTraceInspectResult {
        trace_id: trace_id.to_string(),
        endpoint: config.endpoint,
        token_source: config.token_source,
        flushed,
        result,
        read_error,
    })
}

pub fn inspect_current_session_trace(flush_first: bool) -> Result<CodexCurrentSessionTrace> {
    let trace_id = env_non_empty("FLOW_TRACE_ID").ok_or_else(|| {
        anyhow::anyhow!(
            "FLOW_TRACE_ID is not set; start or resume the Codex session through Flow (`j`, `k`, or `f codex ...`)"
        )
    })?;
    let span_id = env_non_empty("FLOW_SPAN_ID");
    let parent_span_id = env_non_empty("FLOW_PARENT_SPAN_ID");
    let workflow_kind = env_non_empty("FLOW_WORKFLOW_KIND");
    let service_name = env_non_empty("FLOW_TRACE_SERVICE_NAME");
    let inspected = inspect_trace(&trace_id, flush_first)?;
    Ok(CodexCurrentSessionTrace {
        trace_id,
        span_id,
        parent_span_id,
        workflow_kind,
        service_name,
        flushed: inspected.flushed,
        endpoint: inspected.endpoint,
        token_source: inspected.token_source,
        result: inspected.result,
        read_error: inspected.read_error,
    })
}

fn stable_hex_id(parts: &[&str], width: usize) -> String {
    let mut out = String::new();
    let needed = width.div_ceil(16);
    for seed in 0..needed {
        let mut hasher = DefaultHasher::new();
        seed.hash(&mut hasher);
        for part in parts {
            part.hash(&mut hasher);
        }
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out.truncate(width);
    out
}

fn redact_id(value: Option<&str>) -> String {
    value
        .filter(|candidate| !candidate.trim().is_empty())
        .map(|candidate| stable_hex_id(&[candidate], 16))
        .unwrap_or_else(|| "none".to_string())
}

fn repo_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn path_hash(path: &str) -> String {
    stable_hex_id(&[path], 16)
}

fn artifact_name(path: Option<&str>) -> String {
    path.and_then(|value| Path::new(value).file_name())
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("none")
        .to_string()
}

fn event_span(event: &CodexSkillEvalEvent) -> MapleSpan {
    let session_seed = event
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(event.target_path.as_str());
    let event_seed = format!(
        "eval:{}:{}:{}:{}",
        event.recorded_at_unix, event.mode, event.route, event.action
    );
    let start_time_unix_nano = event.recorded_at_unix.saturating_mul(1_000_000_000);
    let end_time_unix_nano = start_time_unix_nano.saturating_add(1_000_000);
    MapleSpan {
        trace_id: event
            .trace_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string())
            .unwrap_or_else(|| stable_hex_id(&[session_seed], 32)),
        span_id: event
            .span_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string())
            .unwrap_or_else(|| stable_hex_id(&[session_seed, &event_seed], 16)),
        parent_span_id: event.parent_span_id.clone().unwrap_or_default(),
        name: event
            .workflow_kind
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("flow.codex.{value}"))
            .unwrap_or_else(|| "flow.codex.launch".to_string()),
        kind: 1,
        start_time_unix_nano,
        end_time_unix_nano,
        status_code: 1,
        status_message: None,
        attributes: vec![
            ("event.kind".to_string(), "codex_skill_eval".to_string()),
            ("mode".to_string(), event.mode.clone()),
            ("action".to_string(), event.action.clone()),
            ("route".to_string(), event.route.clone()),
            ("target.repo".to_string(), repo_name(&event.target_path)),
            ("target.path_hash".to_string(), path_hash(&event.target_path)),
            ("launch.path_hash".to_string(), path_hash(&event.launch_path)),
            ("session.hash".to_string(), redact_id(event.session_id.as_deref())),
            (
                "runtime.skill_count".to_string(),
                event.runtime_skills.len().to_string(),
            ),
            (
                "trace.workflow_kind".to_string(),
                event
                    .workflow_kind
                    .clone()
                    .unwrap_or_else(|| "launch".to_string()),
            ),
            (
                "trace.service_name".to_string(),
                event
                    .service_name
                    .clone()
                    .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_SERVICE_NAME.to_string()),
            ),
            (
                "runtime.skills".to_string(),
                if event.runtime_skills.is_empty() {
                    "none".to_string()
                } else {
                    event.runtime_skills.join(",")
                },
            ),
            (
                "prompt.context_budget_chars".to_string(),
                event.prompt_context_budget_chars.to_string(),
            ),
            ("prompt.chars".to_string(), event.prompt_chars.to_string()),
            (
                "prompt.injected_context_chars".to_string(),
                event.injected_context_chars.to_string(),
            ),
            (
                "prompt.reference_count".to_string(),
                event.reference_count.to_string(),
            ),
        ],
    }
}

fn outcome_span(outcome: &CodexSkillOutcomeEvent) -> MapleSpan {
    let target_seed = outcome
        .target_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("unknown-target");
    let outcome_seed = format!(
        "outcome:{}:{}:{:.3}",
        outcome.recorded_at_unix, outcome.kind, outcome.success
    );
    let start_time_unix_nano = outcome.recorded_at_unix.saturating_mul(1_000_000_000);
    let end_time_unix_nano = start_time_unix_nano.saturating_add(1_000_000);
    MapleSpan {
        trace_id: outcome
            .trace_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string())
            .unwrap_or_else(|| {
                stable_hex_id(
                    &[outcome.session_id.as_deref().unwrap_or(target_seed), "outcome"],
                    32,
                )
            }),
        span_id: outcome
            .span_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string())
            .unwrap_or_else(|| stable_hex_id(&[target_seed, &outcome_seed], 16)),
        parent_span_id: outcome.parent_span_id.clone().unwrap_or_default(),
        name: outcome
            .service_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|_| "flow.codex.outcome".to_string())
            .unwrap_or_else(|| "flow.codex.outcome".to_string()),
        kind: 1,
        start_time_unix_nano,
        end_time_unix_nano,
        status_code: if outcome.success >= 0.5 { 1 } else { 2 },
        status_message: None,
        attributes: vec![
            ("event.kind".to_string(), "codex_skill_outcome".to_string()),
            ("kind".to_string(), outcome.kind.clone()),
            ("success".to_string(), format!("{:.3}", outcome.success)),
            (
                "skill_names".to_string(),
                if outcome.skill_names.is_empty() {
                    "none".to_string()
                } else {
                    outcome.skill_names.join(",")
                },
            ),
            (
                "session.hash".to_string(),
                redact_id(outcome.session_id.as_deref()),
            ),
            (
                "target.repo".to_string(),
                outcome
                    .target_path
                    .as_deref()
                    .map(repo_name)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
            (
                "target.path_hash".to_string(),
                outcome
                    .target_path
                    .as_deref()
                    .map(path_hash)
                    .unwrap_or_else(|| "none".to_string()),
            ),
            (
                "artifact.name".to_string(),
                artifact_name(outcome.artifact_path.as_deref()),
            ),
            (
                "artifact.hash".to_string(),
                outcome
                    .artifact_path
                    .as_deref()
                    .map(path_hash)
                    .unwrap_or_else(|| "none".to_string()),
            ),
            (
                "trace.service_name".to_string(),
                outcome
                    .service_name
                    .clone()
                    .unwrap_or_else(|| CODEX_MAPLE_DEFAULT_SERVICE_NAME.to_string()),
            ),
        ],
    }
}

fn export_lines<T, F, E>(
    path: &Path,
    offset: &mut u64,
    remaining: &mut usize,
    mut parse_line: F,
    mut emit: E,
) -> Result<(usize, usize)>
where
    F: FnMut(&str) -> Option<T>,
    E: FnMut(T),
{
    if *remaining == 0 || !path.exists() {
        return Ok((0, 0));
    }
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if *offset > metadata.len() {
        *offset = 0;
    }

    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    file.seek(SeekFrom::Start(*offset))
        .with_context(|| format!("failed to seek {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut seen = 0usize;
    let mut exported = 0usize;

    while *remaining > 0 {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .with_context(|| format!("failed reading {}", path.display()))?;
        if bytes == 0 {
            break;
        }
        *offset = (*offset).saturating_add(bytes as u64);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        seen += 1;
        if let Some(item) = parse_line(trimmed) {
            emit(item);
            exported += 1;
        }
        *remaining = remaining.saturating_sub(1);
    }

    Ok((seen, exported))
}

pub fn flush(limit: usize) -> Result<CodexTelemetryFlushSummary> {
    let config = parse_maple_exporter_config_from_env()?;
    let state_path = telemetry_state_path()?;
    let Some(config) = config else {
        return Ok(CodexTelemetryFlushSummary {
            enabled: false,
            configured_targets: 0,
            events_seen: 0,
            outcomes_seen: 0,
            events_exported: 0,
            outcomes_exported: 0,
            state_path: state_path.display().to_string(),
            last_exported_at_unix: None,
        });
    };

    let exporter = MapleTraceExporter::new(config.clone());
    let mut state = load_state()?;
    let events_path = codex_skill_eval::events_log_path()?;
    let outcomes_path = codex_skill_eval::outcomes_log_path()?;
    let mut remaining = limit.max(1);

    let (events_seen, events_exported) = export_lines(
        &events_path,
        &mut state.events_offset,
        &mut remaining,
        |line| serde_json::from_str::<CodexSkillEvalEvent>(line).ok(),
        |event| exporter.emit_span(event_span(&event)),
    )?;
    let (outcomes_seen, outcomes_exported) = export_lines(
        &outcomes_path,
        &mut state.outcomes_offset,
        &mut remaining,
        |line| serde_json::from_str::<CodexSkillOutcomeEvent>(line).ok(),
        |outcome| exporter.emit_span(outcome_span(&outcome)),
    )?;

    if events_seen > 0 || outcomes_seen > 0 {
        state.version = 1;
        state.events_exported = state.events_exported.saturating_add(events_exported as u64);
        state.outcomes_exported = state
            .outcomes_exported
            .saturating_add(outcomes_exported as u64);
        if events_exported > 0 || outcomes_exported > 0 {
            state.last_exported_at_unix = Some(unix_now());
        }
        save_state(&state)?;
    }

    Ok(CodexTelemetryFlushSummary {
        enabled: true,
        configured_targets: config.targets.len(),
        events_seen,
        outcomes_seen,
        events_exported,
        outcomes_exported,
        state_path: state_path.display().to_string(),
        last_exported_at_unix: state.last_exported_at_unix,
    })
}

pub fn maybe_flush(limit: usize) -> Result<usize> {
    let summary = flush(limit)?;
    Ok(summary.events_exported + summary.outcomes_exported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_uses_leaf_directory() {
        assert_eq!(repo_name("/Users/test/code/flow"), "flow");
        assert_eq!(repo_name("flow"), "flow");
    }

    #[test]
    fn redact_id_is_stable_and_short() {
        let first = redact_id(Some("session-123"));
        let second = redact_id(Some("session-123"));
        assert_eq!(first, second);
        assert_eq!(first.len(), 16);
    }
}
