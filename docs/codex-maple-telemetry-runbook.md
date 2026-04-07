# Codex Maple Telemetry Runbook

Use this when you want shared analytics for Flow-guided Codex usage without
changing the local source of truth.

## What This Does

Flow keeps Codex telemetry local first:

- `codex/skill-eval/events.jsonl`
- `codex/skill-eval/outcomes.jsonl`
- Jazz2-backed Codex memory mirror

Optional Maple export reads those local logs and emits a derived, redacted OTLP
stream.

What gets exported:

- route / mode / action
- runtime skill names and counts
- prompt/context size metrics
- reference counts
- outcome kind / success
- repo leaf name plus hashed path identifiers

What does not get exported:

- raw prompt text
- full filesystem paths
- raw session ids

## Configure

Use Flow env store so the daemon and Flow-launched Codex sessions see the same
values. For local-only secrets, prefer the personal store:

```bash
cd ~/code/flow
f env set --personal FLOW_CODEX_MAPLE_LOCAL_ENDPOINT=http://ingest.maple.localhost/v1/traces
f env set --personal FLOW_CODEX_MAPLE_LOCAL_INGEST_KEY=maple_pk_local_xxx
f env set --personal FLOW_CODEX_MAPLE_HOSTED_ENDPOINT=https://ingest.maple.dev/v1/traces
f env set --personal FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY=maple_sk_hosted_xxx
f env set --personal FLOW_CODEX_MAPLE_HOSTED_PUBLIC_INGEST_KEY=maple_pk_hosted_xxx
```

Optional tuning:

```bash
f env set --personal FLOW_CODEX_MAPLE_SERVICE_NAME=flow-codex
f env set --personal FLOW_CODEX_MAPLE_SCOPE_NAME=flow.codex
f env set --personal FLOW_CODEX_MAPLE_ENV=local
f env set --personal FLOW_CODEX_MAPLE_QUEUE_CAPACITY=1024
f env set --personal FLOW_CODEX_MAPLE_MAX_BATCH_SIZE=64
f env set --personal FLOW_CODEX_MAPLE_FLUSH_INTERVAL_MS=100
f env set --personal FLOW_CODEX_MAPLE_CONNECT_TIMEOUT_MS=400
f env set --personal FLOW_CODEX_MAPLE_REQUEST_TIMEOUT_MS=800
```

On macOS, personal env reads may require a daily unlock:

```bash
f env unlock
```

If you launch Codex through Flow (`j ...`, `f codex ...`, `k ...`), Flow will
hydrate these `FLOW_CODEX_MAPLE_*` vars into the child Codex process. Explicit
shell env vars override the personal store for that launch, which makes one-off
telemetry tests quiet and deterministic.

From inside a Codex session, the write path stays the same:

```bash
f env set --personal FLOW_CODEX_MAPLE_HOSTED_ENDPOINT=...
f env set --personal FLOW_CODEX_MAPLE_HOSTED_INGEST_KEY=...
f env get --personal FLOW_CODEX_MAPLE_HOSTED_ENDPOINT
```

## Run

Inspect current state:

```bash
f codex telemetry status
```

Flush unseen local telemetry once:

```bash
f codex telemetry flush --limit 200
```

Equivalent task shortcuts:

```bash
f codex-telemetry-status
f codex-telemetry-flush
```

## Background Export

If `jd` is running, it also performs a bounded background flush pass during
its normal maintenance loop. This keeps export cheap and avoids a separate
always-on process.

## Notes

- Export is derived and best-effort.
- Local logs and memory stay canonical even if Maple is unavailable.
- This is intended for route/context/outcome analytics, not transcript export.
