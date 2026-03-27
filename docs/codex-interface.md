# Flow Codex Interface

## Short Answer

Partly.

Flow does **not** use `codex app-server` for every Codex command.
There are multiple Codex lanes:

- the normal Flow session lane uses the regular Codex CLI through the Flow wrapper
- the run-owned agent lane uses `codex app-server`
- the native commit review lane uses `codex app-server`
- a few runtime-management helpers also use `codex app-server`

So the right model is:

- `f codex ...` is the user-facing interface
- underneath, Flow chooses either:
  - wrapped Codex CLI execution
  - or direct `codex app-server` JSON-RPC

## Main Pieces

### 1. Flow wrapper

File:

- `scripts/codex-flow-wrapper`

Role:

- launches the real `codex` binary
- materializes temporary runtime skills from Flow state
- cleans those temporary symlinks up after the Codex process exits

Important point:

- this is still the normal Codex CLI, not app-server

### 2. Flow Codex command layer

Primary file:

- `src/ai.rs`

Role:

- implements `f codex open`
- implements `f codex resolve`
- implements `f codex doctor`
- implements session recovery, reference expansion, and runtime-skill planning
- implements the Flow bridge into run-owned agents

Important point:

- most of this layer is orchestration and context shaping
- it does not imply app-server by itself

### 3. Flow codexd daemon

Primary file:

- `src/codexd.rs`

Role:

- local Flow daemon for fast repo/session intelligence
- caches recent queryable state
- serves project-ai and related lightweight repo intelligence

Important point:

- `codexd` is a Flow daemon
- it is **not** the Codex app-server

### 4. Run-owned agent runtime

Primary files:

- `~/run/scripts/agent-router.sh`
- `~/run/scripts/agent-codex-app-server.py`

Role:

- executes spec-backed agents such as `planner`, `commit`, `input`, and `migration-planner`
- opens or resumes threads
- sends turns through `codex app-server`
- persists artifacts, traces, and per-agent state

Important point:

- this lane is app-server-based by design

## Which Paths Use App-Server

### Uses the normal Codex CLI through the Flow wrapper

- `f codex open`
- `f codex resume`
- `f codex continue`
- `f codex connect`
- normal interactive Codex sessions launched by Flow

How it works:

1. Flow resolves the repo/path and any compact context to inject.
2. Flow may prepare runtime skill state.
3. Flow launches the configured Codex binary, usually `scripts/codex-flow-wrapper`.
4. The wrapper exposes runtime skills and then execs the real `codex`.

### Uses `codex app-server`

- `f codex agent run ...`
- Flow native commit review
- Codex skill reload / force-rescan helper paths
- run-owned spec agents in `~/run`

How it works:

1. Flow or `~/run` spawns `codex app-server`.
2. It performs the initialize handshake over stdio JSON-RPC.
3. It creates or resumes a thread.
4. It sends structured requests such as:
   - `thread/start`
   - `turn/start`
   - `review/start`
   - `skills/list`
5. It consumes structured events and results.

## Command Surface Breakdown

### `f codex open`

Primary behavior:

- session recovery
- compact reference expansion
- runtime-skill activation
- normal Codex CLI launch

Transport:

- wrapper + standard Codex CLI

Notable files:

- `src/ai.rs`
- `scripts/codex-flow-wrapper`

### `f codex doctor`

Primary behavior:

- inspect effective Codex config for a repo/path
- report wrapper/runtime readiness
- report run-agent bridge readiness

Transport:

- local Flow inspection only

Important point:

- doctor does not require app-server to explain the current configuration

### `f codex agent list` / `show`

Primary behavior:

- inspect the run-owned agent corpus from `~/run`

Transport:

- Flow shells into `~/run/scripts/agent-router.sh`
- no app-server turn is needed for `list` or `show`

### `f codex agent run <agent-id> ...`

Primary behavior:

- execute a run-owned agent from Flow

Transport:

- Flow -> `~/run/scripts/agent-router.sh run-json`
- `agent-router.sh` -> `agent-codex-app-server.py`
- Python runner -> `codex app-server`

Important point:

- this is the main place where the new Flow-to-run bridge is explicitly app-server-based

### `f commit` Codex review lane

Primary behavior:

- review staged or uncommitted changes using the Codex native review path

Transport:

- direct `codex app-server`
- uses built-in `review/start`

Important point:

- this is a stronger primitive than sending a freeform review prompt through a normal chat turn

## Why Flow Uses More Than One Lane

Because the best transport depends on the job:

- interactive coding sessions are best served by the normal Codex CLI with Flow-managed runtime context
- structured agent execution needs thread control, event streaming, and durable artifacts, so app-server is the better substrate
- native code review has a dedicated app-server method, so Flow should use that instead of imitating review in plain text

## Current Mental Model

Use this rule:

- if you are opening or resuming a normal Codex coding session, think "wrapped Codex CLI"
- if you are running a reusable Flow/run agent or a native review lane, think "`codex app-server`"

That is the current architecture.

It is intentionally hybrid:

- Flow keeps the common path lightweight
- app-server is reserved for the places where structured threads, structured review, or structured artifacts materially improve the result

## Concrete Examples

### Example 1: normal coding turn

```bash
f codex open --path ~/code/flow "continue the codex agent rollout"
```

Expected transport:

- Flow orchestration
- wrapper
- normal Codex CLI

### Example 2: run-owned planning agent

```bash
f codex agent run planner --path ~/code/flow "make a 3 phase rollout plan"
```

Expected transport:

- Flow bridge
- `~/run` agent router
- `codex app-server`

### Example 3: commit review

```bash
f commit --slow --context --codex
```

Expected transport:

- Flow review pipeline
- direct `codex app-server`
- native `review/start`

## Non-Goals

What this interface is not doing today:

- it is not making every Flow Codex command app-server-only
- it is not using `codexkit` as the main Flow executor
- it is not duplicating run-owned agent specs inside Flow

## Summary

Flow's Codex interface is a control-plane interface, not a single transport.

The stable split today is:

- default session UX: wrapped Codex CLI
- structured agent execution: `codex app-server`
- native review: `codex app-server`
- repo intelligence and readiness checks: Flow-local logic plus `codexd`
