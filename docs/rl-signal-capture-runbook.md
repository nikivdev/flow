# RL Signal Capture Runbook (Flow + Seq)

This is the Phase 1 capture path for low-latency, high-signal RL data.

## 1) Enable low-latency local seq capture

From `~/code/seq`:

```bash
f rl-capture-on
f agent-qa-capture-on
```

This forces local spool mode (`SEQ_CH_MODE=file`) so user-path latency is not tied to remote network writes.

Or from `~/code/flow` (single command):

```bash
f rl-capture-on-all
```

## 2) Enable flow RL signal logging

From `~/code/flow`:

```bash
export FLOW_RL_SIGNALS=true
export FLOW_RL_SIGNALS_PATH=out/logs/flow_rl_signals.jsonl
export FLOW_RL_SIGNALS_SEQ_MIRROR=true
export FLOW_RL_SIGNALS_SEQ_PATH=~/repos/ClickHouse/ClickHouse/user_files/seq_mem.jsonl
export FLOW_RL_SIGNAL_TEXT=snippet
export FLOW_RL_SIGNAL_MAX_CHARS=4000
```

`f ai everruns ...` now emits structured runtime/tool events into the JSONL file.
`f ai:*` task execution via `ai-taskd` now also emits linked router events:

- `flow.router.decision.v1`
- `flow.router.override.v1` (when a suggested task differs from chosen task)
- `flow.router.outcome.v1`

These are mirrored directly into `seq_mem.jsonl` when `FLOW_RL_SIGNALS_SEQ_MIRROR=true`.

To capture override events, set suggestion context on the command that triggers `f ai:*`:

```bash
export FLOW_ROUTER_SUGGESTED_TASK=ai:flow/noop
export FLOW_ROUTER_OVERRIDE_REASON=manual_user_choice
f ai:flow/dev-check
```

## 3) Inspect quality in real time

From `~/code/flow`:

```bash
f rl-signals-tail
f rl-signals-summary --last 2000
```

From `~/code/seq`:

```bash
f rl-signal-tail
f rl-signal-summary
```

## 4) What should be present

- `everruns.run_started`
- `everruns.runtime_event` (includes stage + duration)
- `everruns.tool_call_result` (includes seq op, success/failure, error class)
- `everruns.qa_pair` (prompt/response supervision pair)
- `everruns.run_completed` or `everruns.run_failed`
- `agent.qa.pair` in `seq_mem.jsonl` (Claude/Codex Q/A pairs from background ingest)
- `flow.router.decision.v1` in `seq_mem.jsonl`
- `flow.router.override.v1` in `seq_mem.jsonl` (when suggestion context is provided)
- `flow.router.outcome.v1` in `seq_mem.jsonl`

## 5) Build Harbor snapshot from runtime traces

From `~/code/flow`:

```bash
f rl-dataset-build
f rl-dataset-validate
```

Outputs:

- `~/repos/laude-institute/harbor/data/flow_runtime/<timestamp>/events.jsonl`
- `~/repos/laude-institute/harbor/data/flow_runtime_prepared/<timestamp>/train.jsonl`
- `~/repos/laude-institute/harbor/data/flow_runtime_prepared/<timestamp>/val.jsonl`
- `~/repos/laude-institute/harbor/data/flow_runtime_prepared/<timestamp>/test.jsonl`
- `~/repos/laude-institute/harbor/data/flow_runtime_prepared/<timestamp>/validation_report.json`

Latest rolling copies are also written under `.../flow_runtime/latest` and `.../flow_runtime_prepared/latest`.

If capture is currently Q/A-only (`assistant_sft_example` rows), validation automatically relaxes event-diversity gates and still enforces row-count and basic quality checks.

## 6) Feed into Harbor training loop

Keep this file as raw trajectory telemetry; downstream pipelines should join with:

- myflow commit/session exports
- flow anon telemetry snapshots
- reward labels / canary outcomes

Do not train directly on raw logs without redaction + quality filtering.
