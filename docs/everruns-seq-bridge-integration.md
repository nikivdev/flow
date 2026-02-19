# Everruns + Seq Bridge Integration

This document describes the Flow integration that runs Everruns sessions and executes
client-side `seq_*` tool calls via `seqd` without duplicating Seq mapping logic.

## Why This Was Added

`f ai everruns` already existed, but duplicated three things now maintained in `~/code/seq`:

- Everruns `seq_*` client-side tool catalog
- tool-name normalization rules
- request correlation ID shaping for seq RPC (`request_id`, `run_id`, `tool_call_id`)

Flow now imports the shared bridge crate instead of carrying its own copy.

## What Changed

Code path changed only in Everruns tool-bridge internals:

- `src/ai_everruns.rs`
- `Cargo.toml` dependency on `seq_everruns_bridge`

Flow still owns and keeps unchanged:

- Everruns prompt/session/message/event loop
- Flow config/env resolution for Everruns (`[everruns]`, `FLOW_EVERRUNS_*`)
- `f seq-rpc` command and other AI session commands

## No-Overlap Contract

This integration is intentionally scoped to avoid feature overlap:

- Flow does not reimplement `seq_*` tool schema/mapping anymore.
- Flow does not add a second Everruns runtime.
- Existing `f ai claude` / `f ai codex` / `f seq-rpc` behavior remains unchanged.

## Dependency Setup

Current local setup in `Cargo.toml`:

```toml
seq_everruns_bridge = { path = "../seq/api/rust/seq_everruns_bridge" }
```

This matches a sibling checkout layout:

- `~/code/flow`
- `~/code/seq`

### Submodule Option (recommended for portability)

If you want reproducible CI/clone behavior, replace the sibling path with a submodule path:

1. add seq as submodule (example): `third_party/seq`
2. update dep path to:

```toml
seq_everruns_bridge = { path = "third_party/seq/api/rust/seq_everruns_bridge" }
```

## Validation (Real Results)

Run from `~/code/flow`:

```bash
cargo check
cargo run --release --bin f -- ai everruns --help
rg "bridge_tool_definitions|parse_tool_call_requested|bridge_build_request" src/ai_everruns.rs
rg "map_seq_operation|seq_client_tool_definitions" src/ai_everruns.rs
```

End-to-end smoke (requires local Everruns + seqd):

```bash
f ai everruns "ping"
```

Expected evidence of integration:

- successful compile with shared bridge dependency
- bridge call-sites present in Flow Everruns runtime
- no duplicated tool catalog/mapping in `src/ai_everruns.rs`

## When To Keep It / When To Revert

Keep this integration if:

- you want one source of truth for Everruns `seq_*` tool behavior
- Flow and Seq should stay protocol-aligned with less maintenance drift

Revert if:

- Flow must build in environments where seq bridge path is unavailable
- you intentionally want Flow and Seq to diverge in tool mapping behavior
