# RL Plan For myflow -> Harbor System

Use this plan to turn the current export/prep automation into a measurable RL improvement loop for agent behavior.

## Scope

Current system already has:

- myflow export to Harbor snapshots (`assistant_sft.jsonl`, `train_events.jsonl`, `summary.json`)
- deterministic Harbor split prep (`train/val/test/canary` + `manifest.json`)
- infra timer wiring for recurring export/prepare jobs
- Maple telemetry hooks for export visibility

Goal: convert this into a closed loop where training updates are driven by observed failures/regressions and promoted only through hard gates.

## What RL Should Improve

Primary outcomes:

1. Better action selection in real workflows (fewer wrong tool/actions).
2. Lower production regressions (canary deltas trend positive).
3. Faster convergence per run (more useful data per training cycle).
4. Higher reliability under ambiguous/long-horizon tasks.

## Phase Plan

## Phase 0: Stabilize Data Reliability (Now)

1. Enforce snapshot integrity checks in Harbor ingest.
2. Fail job if `assistant_sft.jsonl` is empty or split counts are invalid.
3. Persist run metadata keyed by snapshot timestamp and git SHA of training config.

Done definition:

- every snapshot has a valid manifest + non-empty train split
- every training run can be traced back to one exact snapshot + config

## Phase 1: Reward Signal Contract (Next)

1. Define reward schema from `train_events.jsonl` (success, retries, rollback, human override, time-to-fix).
2. Map each signal to normalized reward components in Harbor.
3. Store per-sample reward breakdown for auditability.

Done definition:

- reward function is versioned (`reward_schema_version`)
- each trained sample has explainable reward components

## Phase 2: Offline RL + Canary Gate (Next)

1. Train candidate adapters on latest prepared snapshot.
2. Evaluate on fixed holdout + canary split from same manifest.
3. Add strict promotion gate: holdout pass + canary pass + no action-collapse.

Done definition:

- promotion is blocked automatically on gate failure
- gate outputs are attached to snapshot and run IDs

## Phase 3: Continuous Hard-Case Mining (Then)

1. Mine failed canary/production cases into a hardcase set.
2. Re-inject hardcases with higher sampling weight in next cycle.
3. Track “failure class recurrence” across runs.

Done definition:

- recurring failure classes trend downward across 3+ cycles

## Minimal Metrics To Track

- `canary_reward_delta_mean`
- `canary_reward_delta_ci95_low/high`
- `action_error_rate`
- `fallback_or_override_rate`
- `time_to_resolution_p50/p95`
- `hardcase_recurrence_rate`

## Runbook (Operator Loop)

```bash
# 1) Export latest data from myflow to Harbor
cd ~/code/myflow
f harbor-export-data-maple

# 2) Prepare deterministic splits
cd ~/repos/laude-institute/harbor
python3 scripts/prepare_myflow_dataset.py --snapshot latest

# 3) Train/eval candidate in Harbor (task names TBD in harbor)
# 4) Promote only if holdout + canary gates pass
```

## Immediate Next Steps

1. Add Harbor task: `myflow-validate-snapshot` (manifest + split sanity checks).
2. Add Harbor task: `myflow-eval-canary` (fixed JSON report schema for promotion gate).
3. Add Harbor task: `myflow-mine-hardcases` (from failed canary/prod traces).
4. Add one weekly dashboard cut from Maple + Harbor manifests for trend review.

