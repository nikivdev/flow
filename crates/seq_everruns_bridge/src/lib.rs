use seq_client::{RpcRequest, SeqClient};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub mod maple;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallRequestedData {
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("unsupported seq tool name: {0}")]
    UnsupportedTool(String),
}

pub fn parse_tool_call_requested(data: &Value) -> Result<Vec<ToolCall>, serde_json::Error> {
    let parsed: ToolCallRequestedData = serde_json::from_value(data.clone())?;
    Ok(parsed.tool_calls)
}

pub fn execute_tool_call(
    client: &SeqClient,
    session_id: &str,
    event_id: &str,
    call: &ToolCall,
) -> ToolResult {
    execute_tool_call_with_maple(client, session_id, event_id, call, None)
}

pub fn execute_tool_call_with_maple(
    client: &SeqClient,
    session_id: &str,
    event_id: &str,
    call: &ToolCall,
    maple_exporter: Option<&maple::MapleTraceExporter>,
) -> ToolResult {
    let started = Instant::now();
    let start_unix_nano = unix_time_nanos_now();
    let seq_op = map_tool_name_to_seq_op(&call.name).unwrap_or("unknown");

    let result = match build_request(session_id, event_id, call) {
        Ok(req) => match client.call(req) {
            Ok(resp) => {
                if resp.ok {
                    ToolResult {
                        tool_call_id: call.id.clone(),
                        result: Some(resp.result.unwrap_or_else(|| json!({}))),
                        error: None,
                    }
                } else {
                    let op = map_tool_name_to_seq_op(&call.name).unwrap_or("unknown");
                    ToolResult {
                        tool_call_id: call.id.clone(),
                        result: None,
                        error: Some(
                            resp.error
                                .unwrap_or_else(|| format!("seq {op} failed with unknown error")),
                        ),
                    }
                }
            }
            Err(err) => {
                let op = map_tool_name_to_seq_op(&call.name).unwrap_or("unknown");
                ToolResult {
                    tool_call_id: call.id.clone(),
                    result: None,
                    error: Some(format!("seq {op} call failed: {err}")),
                }
            }
        },
        Err(err) => ToolResult {
            tool_call_id: call.id.clone(),
            result: None,
            error: Some(err.to_string()),
        },
    };

    if let Some(exporter) = maple_exporter {
        let elapsed = started.elapsed();
        let duration_ms = elapsed.as_millis() as u64;
        let end_unix_nano = start_unix_nano.saturating_add(elapsed.as_nanos() as u64);
        let ok = result.error.is_none();
        let span = maple::MapleSpan::for_tool_call(
            session_id,
            event_id,
            &call.id,
            &call.name,
            seq_op,
            ok,
            result.error.as_deref(),
            start_unix_nano,
            end_unix_nano,
            duration_ms,
        );
        exporter.emit_span(span);
    }

    result
}

pub fn build_request(
    session_id: &str,
    event_id: &str,
    call: &ToolCall,
) -> Result<RpcRequest, BridgeError> {
    let op = map_tool_name_to_seq_op(&call.name)
        .ok_or_else(|| BridgeError::UnsupportedTool(call.name.clone()))?;

    let mut req = RpcRequest::new(op)
        .with_request_id(format!("everruns:{event_id}:{}", call.id))
        .with_run_id(session_id)
        .with_tool_call_id(&call.id);

    if !call.arguments.is_null() {
        req = req.with_args_json(call.arguments.clone());
    }

    Ok(req)
}

pub fn map_tool_name_to_seq_op(tool_name: &str) -> Option<&'static str> {
    let mut name = tool_name.trim().to_ascii_lowercase().replace('-', "_");

    for prefix in ["seq.", "seq:", "seq_"] {
        if let Some(rest) = name.strip_prefix(prefix) {
            name = rest.to_string();
            break;
        }
    }

    match name.as_str() {
        "ping" => Some("ping"),
        "app_state" => Some("app_state"),
        "perf" => Some("perf"),
        "open_app" => Some("open_app"),
        "open_app_toggle" => Some("open_app_toggle"),
        "run_macro" => Some("run_macro"),
        "click" => Some("click"),
        "right_click" => Some("right_click"),
        "double_click" => Some("double_click"),
        "move" => Some("move"),
        "scroll" => Some("scroll"),
        "drag" => Some("drag"),
        "screenshot" => Some("screenshot"),
        _ => None,
    }
}

pub fn client_side_tool_definitions() -> Vec<Value> {
    vec![
        client_tool(
            "seq_ping",
            "Health check seqd runtime",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        client_tool(
            "seq_app_state",
            "Get frontmost/previous app snapshot",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        client_tool(
            "seq_perf",
            "Get seqd performance snapshot",
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        client_tool(
            "seq_open_app",
            "Open application by name",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","description":"App name (e.g. Safari)"}},
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_open_app_toggle",
            "Toggle to app by name",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","description":"App name (e.g. Safari)"}},
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_run_macro",
            "Run seq macro by name",
            json!({
                "type":"object",
                "properties":{"name":{"type":"string","description":"Macro name"}},
                "required":["name"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_click",
            "Click at screen coordinates",
            json!({
                "type":"object",
                "properties":{"x":{"type":"number"},"y":{"type":"number"}},
                "required":["x","y"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_right_click",
            "Right click at screen coordinates",
            json!({
                "type":"object",
                "properties":{"x":{"type":"number"},"y":{"type":"number"}},
                "required":["x","y"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_double_click",
            "Double click at screen coordinates",
            json!({
                "type":"object",
                "properties":{"x":{"type":"number"},"y":{"type":"number"}},
                "required":["x","y"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_move",
            "Move pointer to coordinates",
            json!({
                "type":"object",
                "properties":{"x":{"type":"number"},"y":{"type":"number"}},
                "required":["x","y"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_scroll",
            "Scroll at coordinates by delta",
            json!({
                "type":"object",
                "properties":{"x":{"type":"number"},"y":{"type":"number"},"dy":{"type":"integer"}},
                "required":["x","y","dy"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_drag",
            "Drag from one coordinate to another",
            json!({
                "type":"object",
                "properties":{"x1":{"type":"number"},"y1":{"type":"number"},"x2":{"type":"number"},"y2":{"type":"number"}},
                "required":["x1","y1","x2","y2"],
                "additionalProperties":false
            }),
        ),
        client_tool(
            "seq_screenshot",
            "Capture screenshot to optional path",
            json!({
                "type":"object",
                "properties":{"path":{"type":"string","description":"Output path (optional)"}},
                "additionalProperties":false
            }),
        ),
    ]
}

fn client_tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "client_side",
        "name": name,
        "description": description,
        "parameters": parameters
    })
}

fn unix_time_nanos_now() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur.as_nanos() as u64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_tool_names() {
        assert_eq!(map_tool_name_to_seq_op("seq_open_app"), Some("open_app"));
        assert_eq!(map_tool_name_to_seq_op("seq.open_app"), Some("open_app"));
        assert_eq!(map_tool_name_to_seq_op("seq:open-app"), Some("open_app"));
        assert_eq!(map_tool_name_to_seq_op("PING"), Some("ping"));
        assert_eq!(map_tool_name_to_seq_op("unknown_tool"), None);
    }

    #[test]
    fn builds_request_with_correlation_ids() {
        let call = ToolCall {
            id: "tool-9".to_string(),
            name: "seq_click".to_string(),
            arguments: json!({"x": 1, "y": 2}),
        };

        let req = build_request("session-1", "event-7", &call).expect("request should build");
        assert_eq!(req.op, "click");
        assert_eq!(req.request_id.as_deref(), Some("everruns:event-7:tool-9"));
        assert_eq!(req.run_id.as_deref(), Some("session-1"));
        assert_eq!(req.tool_call_id.as_deref(), Some("tool-9"));
        assert_eq!(req.args, Some(json!({"x": 1, "y": 2})));
    }

    #[test]
    fn emits_expected_tool_catalog() {
        let defs = client_side_tool_definitions();
        assert_eq!(defs.len(), 13);
        let names: Vec<&str> = defs
            .iter()
            .filter_map(|v| v.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"seq_open_app"));
        assert!(names.contains(&"seq_screenshot"));
    }

    #[test]
    fn parse_tool_call_requested_payload() {
        let payload = json!({
            "tool_calls": [
                {"id":"tc1","name":"seq_ping","arguments":{}},
                {"id":"tc2","name":"seq_open_app","arguments":{"name":"Safari"}}
            ]
        });

        let calls = parse_tool_call_requested(&payload).expect("payload should parse");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "tc1");
        assert_eq!(calls[1].name, "seq_open_app");
    }

    #[test]
    fn unsupported_tool_returns_error_result() {
        let call = ToolCall {
            id: "tcX".to_string(),
            name: "seq_not_real".to_string(),
            arguments: json!({}),
        };

        let result = ToolResult {
            tool_call_id: call.id.clone(),
            result: None,
            error: Some(
                build_request("session", "event", &call)
                    .expect_err("should error")
                    .to_string(),
            ),
        };

        assert_eq!(result.tool_call_id, "tcX");
        assert!(result
            .error
            .unwrap_or_default()
            .contains("unsupported seq tool name"));
    }

    #[test]
    fn bridge_error_is_displayable() {
        let e = BridgeError::UnsupportedTool("foo".to_string());
        assert_eq!(e.to_string(), "unsupported seq tool name: foo");
    }
}
