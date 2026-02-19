# Seq Agent RPC Contract (Hard Interface)

This document defines the required interface between agent runtimes (Flow/AI server) and OS-level automation.

Status: **mandatory for new integrations**.

If an agent needs macOS UI/app/input actions, it must call `seqd` via the Rust `seq_client` library.

## Why this is hard policy

- Lowest control-plane overhead (persistent local Unix socket, no shell spawn per tool call).
- Typed request/response contract with stable envelope fields.
- Better observability (`request_id`, `run_id`, `tool_call_id`) across planner + OS executor.
- Avoids drift from ad-hoc shell wrappers.

## Required architecture

1. Planner/agent loop runs in AI server.
2. AI server uses `seq_client` (`~/code/seq/api/rust/seq_client`) for OS actions.
3. `seq_client` sends JSON RPC v1 over Unix socket to `seqd`.
4. `seqd` executes OS ops and returns typed response envelope.

Do not insert shell wrappers in the hot path for OS actions.

## Allowed and forbidden paths

Allowed (required):
- Rust: `seq_client::SeqClient` + `RpcRequest`.
- Transport: Unix socket to `seqd` (`/tmp/seqd.sock` default).

Forbidden for production OS-tool execution:
- `bash -lc "seq ..."` inside tool loop.
- `curl`/`nc` direct JSON RPC from tool loop (okay for debugging only).
- Parsing human text responses as protocol.

## RPC envelope requirements

Every request should include:
- `op`
- `request_id`
- `run_id`
- `tool_call_id`

These IDs are required for trace joinability across:
- agent run logs
- tool-call logs
- `seqd` metrics/traces

## Operation mapping (agent tool -> seq op)

- Open app: `open_app`
- Toggle app: `open_app_toggle`
- Run macro: `run_macro`
- Click: `click`
- Right click: `right_click`
- Double click: `double_click`
- Move mouse: `move`
- Scroll: `scroll`
- Drag: `drag`
- Screenshot: `screenshot`
- Runtime status: `ping`, `app_state`, `perf`

See canonical protocol details: `~/code/seq/docs/agent-rpc-v1.md`.

## Reliability rules

- Create one `SeqClient` per worker and reuse it.
- Set explicit read/write timeout (`connect_with_timeout`).
- Treat `ok=false` as tool failure (surface `error` field).
- Retry policy:
  - Safe ops (`ping`, `app_state`, `perf`, maybe `screenshot`) may retry once.
  - Mutating UI ops (`click`, `drag`, `open_app`, `run_macro`) must not auto-retry blindly.
- Max response size guard should remain enabled.

## Latency policy

Hot path target is low latency at the control plane, not guaranteed zero end-to-end UI latency.

Expectations:
- RPC dispatch overhead should be microseconds to sub-millisecond locally.
- UI/app activation latency depends on macOS/window server and target app state.

Benchmark and regressions should measure:
- request send/receive time at client
- `dur_us` returned by `seqd`
- operation-level tail latency (p95/p99)

## Minimal integration example

```rust
use seq_client::{RpcRequest, SeqClient};
use serde_json::json;
use std::time::Duration;

fn call_open_app() -> Result<(), Box<dyn std::error::Error>> {
    let client = SeqClient::connect_with_timeout("/tmp/seqd.sock", Duration::from_secs(5))?;
    let resp = client.call(
        RpcRequest::new("open_app")
            .with_request_id("req-42")
            .with_run_id("run-abc")
            .with_tool_call_id("tool-7")
            .with_args_json(json!({ "name": "Safari" })),
    )?;
    if !resp.ok {
        return Err(format!("seq open_app failed: {:?}", resp.error).into());
    }
    Ok(())
}
```

## Migration checklist

1. Replace shell-based OS tools with `seq_client`.
2. Ensure all OS tool calls include `request_id`, `run_id`, `tool_call_id`.
3. Remove ad-hoc JSON parsing of CLI stdout.
4. Keep `seq rpc` / `nc` only for manual debugging and smoke tests.
5. Gate new OS tool additions on this contract.

