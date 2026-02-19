use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cli::{
    SeqRpcAction, SeqRpcCommand, SeqRpcIdOpts, SeqRpcOpenAppOpts, SeqRpcRawOpts,
    SeqRpcScreenshotOpts,
};
use crate::seq_client::{RpcRequest, SeqClient};

pub fn run(cmd: SeqRpcCommand) -> Result<()> {
    let socket = resolve_socket_path(cmd.socket);
    let timeout = Duration::from_millis(cmd.timeout_ms.max(1));
    let mut client = SeqClient::connect_with_timeout(&socket, timeout)
        .with_context(|| format!("failed to connect to seqd at {}", socket.display()))?;
    let req = build_request(cmd.action)?;
    let resp = client.call(&req)?;

    if cmd.pretty {
        println!("{}", serde_json::to_string_pretty(&resp)?);
    } else {
        println!("{}", serde_json::to_string(&resp)?);
    }

    if resp.ok {
        Ok(())
    } else {
        bail!(
            "seqd rpc op '{}' failed: {}",
            resp.op,
            resp.error.unwrap_or_else(|| "unknown error".to_string())
        )
    }
}

fn resolve_socket_path(cli_socket: Option<PathBuf>) -> PathBuf {
    if let Some(path) = cli_socket {
        return path;
    }
    if let Ok(value) = std::env::var("SEQ_SOCKET_PATH")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    if let Ok(value) = std::env::var("SEQD_SOCKET")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    PathBuf::from("/tmp/seqd.sock")
}

fn build_request(action: SeqRpcAction) -> Result<RpcRequest> {
    match action {
        SeqRpcAction::Ping(ids) => Ok(with_ids(RpcRequest::new("ping"), ids)),
        SeqRpcAction::AppState(ids) => Ok(with_ids(RpcRequest::new("app_state"), ids)),
        SeqRpcAction::Perf(ids) => Ok(with_ids(RpcRequest::new("perf"), ids)),
        SeqRpcAction::OpenApp(opts) => Ok(build_open_app("open_app", opts)),
        SeqRpcAction::OpenAppToggle(opts) => Ok(build_open_app("open_app_toggle", opts)),
        SeqRpcAction::Screenshot(opts) => Ok(build_screenshot(opts)),
        SeqRpcAction::Rpc(opts) => build_raw(opts),
    }
}

fn build_open_app(op: &str, opts: SeqRpcOpenAppOpts) -> RpcRequest {
    let mut req = with_ids(RpcRequest::new(op), opts.ids);
    req.args = Some(json!({ "name": opts.name }));
    req
}

fn build_screenshot(opts: SeqRpcScreenshotOpts) -> RpcRequest {
    let mut req = with_ids(RpcRequest::new("screenshot"), opts.ids);
    req.args = Some(json!({ "path": opts.path }));
    req
}

fn build_raw(opts: SeqRpcRawOpts) -> Result<RpcRequest> {
    let mut req = with_ids(RpcRequest::new(opts.op), opts.ids);
    if let Some(args_json) = opts.args_json {
        let parsed: Value = serde_json::from_str(&args_json)
            .with_context(|| format!("failed to parse --args-json as JSON: {}", args_json))?;
        req.args = Some(parsed);
    }
    Ok(req)
}

fn with_ids(mut req: RpcRequest, ids: SeqRpcIdOpts) -> RpcRequest {
    req.request_id = ids.request_id;
    req.run_id = ids.run_id;
    req.tool_call_id = ids.tool_call_id;
    req
}
