# Everruns + Maple Runbook

This is the fastest path to use the new Everruns telemetry export now.

It sends `f ai everruns` traces to:

- local Maple (dev visualization)
- hosted Maple (shared/history visualization)

## What gets exported

When enabled, Flow exports:

- `everruns.tool_call` spans for each `seq_*` tool execution
- runtime spans such as:
  - `everruns.tool_call_requested`
  - `everruns.output_message_completed`
  - `everruns.turn_failed`

## Prerequisites

1. `seqd` is running and reachable at your socket (`/tmp/seqd.sock` by default).
2. Everruns API is reachable (`http://127.0.0.1:9300/api` by default).
3. You have Maple ingest keys for local and/or hosted endpoint.

## 1) Configure env (now)

From `~/code/flow`, set the endpoints + keys:

```bash
f env set SEQ_EVERRUNS_MAPLE_LOCAL_ENDPOINT=http://ingest.maple.localhost/v1/traces
f env set SEQ_EVERRUNS_MAPLE_LOCAL_INGEST_KEY=maple_pk_local_xxx
f env set SEQ_EVERRUNS_MAPLE_HOSTED_ENDPOINT=https://ingest.1focus.ai/v1/traces
f env set SEQ_EVERRUNS_MAPLE_HOSTED_INGEST_KEY=maple_pk_hosted_xxx
```

Optional tuning:

```bash
f env set SEQ_EVERRUNS_MAPLE_QUEUE_CAPACITY=4096
f env set SEQ_EVERRUNS_MAPLE_MAX_BATCH_SIZE=128
f env set SEQ_EVERRUNS_MAPLE_FLUSH_INTERVAL_MS=50
f env set SEQ_EVERRUNS_MAPLE_CONNECT_TIMEOUT_MS=400
f env set SEQ_EVERRUNS_MAPLE_REQUEST_TIMEOUT_MS=800
```

For optimized mirror (remote ClickHouse + durable local spool), also set:

```bash
f env set SEQ_CH_MODE=mirror
f env set SEQ_CH_MEM_PATH=~/repos/ClickHouse/ClickHouse/user_files/seq_mem.jsonl
f env set SEQ_CH_LOG_PATH=~/repos/ClickHouse/ClickHouse/user_files/seq_trace.jsonl
```

## 2) Run with env injected

Use `f env run` so runtime sees configured values:

```bash
f env run -- f ai everruns "open Safari and take a screenshot"
```

If you already export envs another way, this also works:

```bash
f ai everruns "open Safari and take a screenshot"
```

On startup, if telemetry is enabled, Flow prints:

`maple dual-ingest telemetry enabled`

## 3) Verify in Maple

In Maple (local and hosted), filter by:

- `service.name = seq-everruns-bridge`

Look for span names:

- `everruns.tool_call`
- `everruns.tool_call_requested`
- `everruns.output_message_completed`

## Troubleshooting

1. Error: `invalid SEQ_EVERRUNS_MAPLE_* configuration`
   - You set only endpoint or only key for local/hosted pair.
   - Fix by setting both or removing both for that pair.
2. Everruns command works but no spans in Maple
   - Confirm ingest endpoint includes `/v1/traces`.
   - Confirm ingest key is valid for that endpoint.
   - Confirm you ran through `f env run -- ...` (or equivalent env injection).
3. Temporary Maple outage
   - Tool execution continues.
   - Export is best-effort and non-blocking.
