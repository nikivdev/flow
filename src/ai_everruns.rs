use std::collections::HashSet;
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, RequestBuilder};
use seq_everruns_bridge::{
    ToolCall as BridgeToolCall, build_request as bridge_build_request,
    client_side_tool_definitions as bridge_tool_definitions, parse_tool_call_requested,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::cli::AiEverrunsOpts;
use crate::config::{self, EverrunsConfig};
use crate::seq_client::{RpcRequest, SeqClient};

const DEFAULT_EVERRUNS_BASE_URL: &str = "http://127.0.0.1:9300/api";
const DEFAULT_EVERRUNS_API_KEY_ENV: &str = "EVERRUNS_API_KEY";
const DEFAULT_SEQ_SOCKET: &str = "/tmp/seqd.sock";

pub fn run(opts: AiEverrunsOpts) -> Result<()> {
    let prompt = resolve_prompt(&opts)?;
    let resolved = ResolvedSettings::from_opts(&opts)?;
    let api = EverrunsApi::new(resolved.base_url.clone(), resolved.api_key.clone())?;

    let session_id = resolve_session_id(&api, &resolved)?;
    let seq_bridge = SeqBridge::connect(&resolved)?;

    eprintln!("everruns session: {}", session_id);

    let message_id = api.post_message(&session_id, &prompt)?;
    eprintln!("message_id: {}", message_id);

    wait_for_completion(
        &api,
        &seq_bridge,
        &session_id,
        &message_id,
        resolved.poll_ms,
        resolved.wait_timeout_secs,
    )
}

fn resolve_prompt(opts: &AiEverrunsOpts) -> Result<String> {
    if !opts.prompt.is_empty() {
        let joined = opts.prompt.join(" ").trim().to_string();
        if joined.is_empty() {
            bail!("prompt is empty");
        };
        return Ok(joined);
    }

    if io::stdin().is_terminal() {
        bail!("missing prompt. Usage: f ai everruns \"your prompt\"");
    }

    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read prompt from stdin")?;
    let prompt = buf.trim().to_string();
    if prompt.is_empty() {
        bail!("prompt from stdin is empty");
    }
    Ok(prompt)
}

fn wait_for_completion(
    api: &EverrunsApi,
    seq_bridge: &SeqBridge,
    session_id: &str,
    input_message_id: &str,
    poll_ms: u64,
    wait_timeout_secs: u64,
) -> Result<()> {
    let started = Instant::now();
    let mut since_id: Option<String> = None;
    let mut handled_tool_calls = HashSet::new();

    loop {
        if started.elapsed() > Duration::from_secs(wait_timeout_secs.max(1)) {
            bail!(
                "timed out waiting for Everruns output after {}s",
                wait_timeout_secs
            );
        }

        let events = api.list_events(session_id, since_id.as_deref())?;
        if let Some(last) = events.last() {
            since_id = Some(last.id.clone());
        }

        let mut did_work = false;
        for event in events {
            if let Some(ref event_input_id) = event.context.input_message_id
                && event_input_id != input_message_id
            {
                continue;
            }

            match event.event_type.as_str() {
                "tool.call_requested" => {
                    let requested_calls =
                        parse_tool_call_requested(&event.data).with_context(|| {
                            format!(
                                "failed to parse tool.call_requested payload for event {}",
                                event.id
                            )
                        })?;

                    let mut tool_results = Vec::new();
                    for call in requested_calls {
                        if !handled_tool_calls.insert(call.id.clone()) {
                            continue;
                        }
                        tool_results
                            .push(seq_bridge.execute_tool_call(session_id, &event.id, call));
                    }

                    if !tool_results.is_empty() {
                        api.submit_tool_results(session_id, tool_results)?;
                        did_work = true;
                    }
                }
                "output.message.completed" => {
                    if let Some(text) = extract_output_text(&event.data) {
                        println!("{}", text);
                    } else {
                        println!("{}", serde_json::to_string_pretty(&event.data)?);
                    }
                    return Ok(());
                }
                "turn.failed" => {
                    let error = event
                        .data
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown turn failure");
                    bail!("everruns turn failed: {}", error);
                }
                _ => {}
            }
        }

        if !did_work {
            thread::sleep(Duration::from_millis(poll_ms.max(25)));
        }
    }
}

fn extract_output_text(data: &Value) -> Option<String> {
    let content = data
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)?;
    let mut out = Vec::new();
    for part in content {
        if let Some(text) = part.get("text").and_then(Value::as_str)
            && !text.trim().is_empty()
        {
            out.push(text.to_string());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out.join("\n"))
    }
}

fn resolve_session_id(api: &EverrunsApi, resolved: &ResolvedSettings) -> Result<String> {
    if let Some(session_id) = resolved.session_id.as_ref() {
        if !resolved.no_seq_tools {
            eprintln!(
                "note: reusing session {} (seq tools are not injected for existing sessions)",
                session_id
            );
        }
        return Ok(session_id.clone());
    }

    let harness_id = if let Some(id) = resolved.harness_id.clone() {
        id
    } else {
        api.pick_first_harness_id()?
    };

    let agent_id = resolved
        .agent_id
        .clone()
        .or_else(|| api.pick_first_agent_id().ok());

    let mut body = Map::new();
    body.insert("harness_id".to_string(), Value::String(harness_id.clone()));
    if let Some(agent_id) = agent_id {
        body.insert("agent_id".to_string(), Value::String(agent_id));
    }
    if let Some(model_id) = resolved.model_id.clone() {
        body.insert("model_id".to_string(), Value::String(model_id));
    }
    if !resolved.no_seq_tools {
        body.insert("tools".to_string(), Value::Array(bridge_tool_definitions()));
    }

    let session_id = api.create_session(Value::Object(body))?;
    eprintln!("created session {} (harness_id={})", session_id, harness_id);
    Ok(session_id)
}

#[derive(Debug, Clone)]
struct ResolvedSettings {
    base_url: String,
    api_key: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    harness_id: Option<String>,
    model_id: Option<String>,
    poll_ms: u64,
    wait_timeout_secs: u64,
    seq_socket: PathBuf,
    seq_timeout_ms: u64,
    no_seq_tools: bool,
}

impl ResolvedSettings {
    fn from_opts(opts: &AiEverrunsOpts) -> Result<Self> {
        let cfg = load_project_everruns_config();

        let api_key_env = env_non_empty("FLOW_EVERRUNS_API_KEY_ENV")
            .or_else(|| cfg.as_ref().and_then(|c| c.api_key_env.clone()))
            .unwrap_or_else(|| DEFAULT_EVERRUNS_API_KEY_ENV.to_string());

        let base_url = first_non_empty(
            opts.base_url.clone(),
            env_non_empty("FLOW_EVERRUNS_BASE_URL")
                .or_else(|| env_non_empty("EVERRUNS_BASE_URL"))
                .or_else(|| cfg.as_ref().and_then(|c| c.base_url.clone()))
                .or_else(|| Some(DEFAULT_EVERRUNS_BASE_URL.to_string())),
        )
        .unwrap_or_else(|| DEFAULT_EVERRUNS_BASE_URL.to_string());
        let base_url = normalize_base_url(&base_url)?;

        let api_key = first_non_empty(
            opts.api_key.clone(),
            env_non_empty("FLOW_EVERRUNS_API_KEY")
                .or_else(|| env_non_empty(&api_key_env))
                .or_else(|| env_non_empty("EVERRUNS_API_KEY")),
        );

        let session_id = first_non_empty(
            opts.session_id.clone(),
            env_non_empty("FLOW_EVERRUNS_SESSION_ID")
                .or_else(|| env_non_empty("EVERRUNS_SESSION_ID"))
                .or_else(|| cfg.as_ref().and_then(|c| c.session_id.clone())),
        );
        let agent_id = first_non_empty(
            opts.agent_id.clone(),
            env_non_empty("FLOW_EVERRUNS_AGENT_ID")
                .or_else(|| env_non_empty("EVERRUNS_AGENT_ID"))
                .or_else(|| cfg.as_ref().and_then(|c| c.agent_id.clone())),
        );
        let harness_id = first_non_empty(
            opts.harness_id.clone(),
            env_non_empty("FLOW_EVERRUNS_HARNESS_ID")
                .or_else(|| env_non_empty("EVERRUNS_HARNESS_ID"))
                .or_else(|| cfg.as_ref().and_then(|c| c.harness_id.clone())),
        );
        let model_id = first_non_empty(
            opts.model_id.clone(),
            env_non_empty("FLOW_EVERRUNS_MODEL_ID")
                .or_else(|| env_non_empty("EVERRUNS_MODEL_ID"))
                .or_else(|| cfg.as_ref().and_then(|c| c.model_id.clone())),
        );

        let seq_socket = resolve_seq_socket_path(opts.seq_socket.clone());

        Ok(Self {
            base_url,
            api_key,
            session_id,
            agent_id,
            harness_id,
            model_id,
            poll_ms: opts.poll_ms.max(25),
            wait_timeout_secs: opts.wait_timeout_secs.max(1),
            seq_socket,
            seq_timeout_ms: opts.seq_timeout_ms.max(1),
            no_seq_tools: opts.no_seq_tools,
        })
    }
}

fn normalize_base_url(raw: &str) -> Result<String> {
    let mut url = raw.trim().to_string();
    if url.is_empty() {
        bail!("Everruns base URL is empty");
    }
    while url.ends_with('/') {
        url.pop();
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!(
            "invalid Everruns base URL '{}': must start with http:// or https://",
            raw
        );
    }
    Ok(url)
}

fn resolve_seq_socket_path(cli_socket: Option<PathBuf>) -> PathBuf {
    if let Some(path) = cli_socket {
        return path;
    }
    if let Some(path) = env_non_empty("SEQ_SOCKET_PATH") {
        return PathBuf::from(path);
    }
    if let Some(path) = env_non_empty("SEQD_SOCKET") {
        return PathBuf::from(path);
    }
    PathBuf::from(DEFAULT_SEQ_SOCKET)
}

fn load_project_everruns_config() -> Option<EverrunsConfig> {
    let cwd = std::env::current_dir().ok()?;
    let flow_toml = find_flow_toml_upwards(&cwd)?;
    let cfg = config::load(flow_toml).ok()?;
    cfg.everruns
}

fn find_flow_toml_upwards(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start.to_path_buf());
    while let Some(dir) = current {
        let candidate = dir.join("flow.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        current = dir.parent().map(Path::to_path_buf);
    }
    None
}

fn first_non_empty(a: Option<String>, b: Option<String>) -> Option<String> {
    for candidate in [a, b].into_iter().flatten() {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn env_non_empty(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Clone)]
struct EverrunsApi {
    client: Client,
    base_url: String,
    api_key: Option<String>,
}

impl EverrunsApi {
    fn new(base_url: String, api_key: Option<String>) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build Everruns HTTP client")?;
        Ok(Self {
            client,
            base_url,
            api_key,
        })
    }

    fn pick_first_harness_id(&self) -> Result<String> {
        let value = self.get_json("/v1/harnesses", &[])?;
        let resp: ListResponse<ResourceStub> =
            serde_json::from_value(value).context("failed to decode harness list response")?;
        resp.data
            .into_iter()
            .find(|h| h.status.as_deref() != Some("disabled"))
            .map(|h| h.id)
            .ok_or_else(|| anyhow::anyhow!("no harnesses found in Everruns"))
    }

    fn pick_first_agent_id(&self) -> Result<String> {
        let value = self.get_json("/v1/agents", &[])?;
        let resp: ListResponse<ResourceStub> =
            serde_json::from_value(value).context("failed to decode agent list response")?;
        resp.data
            .into_iter()
            .find(|a| a.status.as_deref() != Some("archived"))
            .map(|a| a.id)
            .ok_or_else(|| anyhow::anyhow!("no agents found in Everruns"))
    }

    fn create_session(&self, body: Value) -> Result<String> {
        let value = self.post_json("/v1/sessions", body)?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Everruns create session response missing id"))
    }

    fn post_message(&self, session_id: &str, prompt: &str) -> Result<String> {
        let path = format!("/v1/sessions/{}/messages", session_id);
        let payload = json!({
            "message": {
                "content": [
                    { "type": "text", "text": prompt }
                ]
            }
        });
        let value = self.post_json(&path, payload)?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Everruns create message response missing id"))
    }

    fn list_events(&self, session_id: &str, since_id: Option<&str>) -> Result<Vec<EverrunsEvent>> {
        let path = format!("/v1/sessions/{}/events", session_id);
        let mut query: Vec<(&str, String)> = vec![
            ("exclude", "output.message.delta".to_string()),
            ("exclude", "reason.thinking.delta".to_string()),
        ];
        if let Some(since_id) = since_id {
            query.push(("since_id", since_id.to_string()));
        }

        let value = self.get_json(&path, &query)?;
        let resp: ListResponse<EverrunsEvent> =
            serde_json::from_value(value).context("failed to decode events response")?;
        Ok(resp.data)
    }

    fn submit_tool_results(
        &self,
        session_id: &str,
        tool_results: Vec<SubmitToolResult>,
    ) -> Result<()> {
        let path = format!("/v1/sessions/{}/tool-results", session_id);
        let payload = SubmitToolResultsRequest { tool_results };
        let _ = self.post_json(&path, serde_json::to_value(payload)?)?;
        Ok(())
    }

    fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.with_auth(self.client.get(url)).query(query);
        self.send_json(request, "GET", path)
    }

    fn post_json(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.with_auth(self.client.post(url)).json(&body);
        self.send_json(request, "POST", path)
    }

    fn with_auth(&self, request: RequestBuilder) -> RequestBuilder {
        if let Some(api_key) = self.api_key.as_deref() {
            request.bearer_auth(api_key)
        } else {
            request
        }
    }

    fn send_json(&self, request: RequestBuilder, method: &str, path: &str) -> Result<Value> {
        let response = request
            .send()
            .with_context(|| format!("Everruns API {} {} request failed", method, path))?;
        let status = response.status();
        let body = response.text().with_context(|| {
            format!(
                "Everruns API {} {} failed to read response body",
                method, path
            )
        })?;
        if !status.is_success() {
            bail!(
                "Everruns API {} {} returned {}: {}",
                method,
                path,
                status,
                body
            );
        }
        serde_json::from_str(&body).with_context(|| {
            format!(
                "Everruns API {} {} returned invalid JSON: {}",
                method, path, body
            )
        })
    }
}

struct SeqBridge {
    client: std::sync::Mutex<SeqClient>,
}

impl SeqBridge {
    fn connect(settings: &ResolvedSettings) -> Result<Self> {
        let timeout = Duration::from_millis(settings.seq_timeout_ms);
        let client =
            SeqClient::connect_with_timeout(&settings.seq_socket, timeout).with_context(|| {
                format!(
                    "failed to connect to seqd at {}",
                    settings.seq_socket.display()
                )
            })?;
        Ok(Self {
            client: std::sync::Mutex::new(client),
        })
    }

    fn execute_tool_call(
        &self,
        session_id: &str,
        event_id: &str,
        call: BridgeToolCall,
    ) -> SubmitToolResult {
        let call_id = call.id.clone();
        let ext_req = match bridge_build_request(session_id, event_id, &call) {
            Ok(req) => req,
            Err(err) => {
                return SubmitToolResult {
                    tool_call_id: call_id,
                    result: None,
                    error: Some(err.to_string()),
                };
            }
        };
        let op_name = ext_req.op.clone();
        let req = RpcRequest {
            op: ext_req.op,
            args: ext_req.args,
            request_id: ext_req.request_id,
            run_id: ext_req.run_id,
            tool_call_id: ext_req.tool_call_id,
        };

        let result_call_id = req
            .tool_call_id
            .as_ref()
            .cloned()
            .unwrap_or_else(|| call.id.clone());

        let mut client = match self.client.lock() {
            Ok(client) => client,
            Err(_) => {
                return SubmitToolResult {
                    tool_call_id: result_call_id,
                    result: None,
                    error: Some("seq client mutex poisoned".to_string()),
                };
            }
        };

        match client.call(&req) {
            Ok(resp) => {
                if resp.ok {
                    SubmitToolResult {
                        tool_call_id: result_call_id,
                        result: Some(resp.result.unwrap_or_else(|| json!({}))),
                        error: None,
                    }
                } else {
                    SubmitToolResult {
                        tool_call_id: result_call_id,
                        result: None,
                        error: Some(resp.error.unwrap_or_else(|| {
                            format!("seq {} failed with unknown error", op_name)
                        })),
                    }
                }
            }
            Err(error) => SubmitToolResult {
                tool_call_id: result_call_id,
                result: None,
                error: Some(format!("seq {} call failed: {}", op_name, error)),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
struct ListResponse<T> {
    data: Vec<T>,
}

#[derive(Debug, Deserialize)]
struct ResourceStub {
    id: String,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EverrunsEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    context: EventContext,
    #[serde(default)]
    data: Value,
}

#[derive(Debug, Default, Deserialize)]
struct EventContext {
    #[serde(default)]
    input_message_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct SubmitToolResultsRequest {
    tool_results: Vec<SubmitToolResult>,
}

#[derive(Debug, Serialize)]
struct SubmitToolResult {
    tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
