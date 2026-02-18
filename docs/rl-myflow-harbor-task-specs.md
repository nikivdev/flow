# RL Task Specs: myflow -> Harbor

Concrete task contracts for the Harbor RL loop. These map directly to scripts currently in `~/repos/laude-institute/harbor/scripts`.

## Task 1: Validate Snapshot

- Script: `scripts/myflow_validate_snapshot.py`
- Purpose: fail fast on broken snapshot/split artifacts before reward labeling or gating.
- Command:

```bash
python3 scripts/myflow_validate_snapshot.py \
  --snapshot <snapshot|latest> \
  --myflow-dir data/myflow \
  --prepared-dir data/myflow_prepared \
  --require-train-events \
  --report-out data/myflow_prepared/<snapshot>/validation_report.json
```

- Inputs:
  - `data/myflow/<snapshot>/assistant_sft.jsonl`
  - `data/myflow/<snapshot>/train_events.jsonl` (required when `--require-train-events`)
  - `data/myflow_prepared/<snapshot>/manifest.json`
- Outputs:
  - validation report JSON
- Exit contract:
  - `0` = pass
  - non-zero = integrity failure

## Task 2: Build Reward Labels

- Script: `scripts/myflow_build_reward_labels.py`
- Purpose: produce versioned per-event rewards for RL training and canary gating.
- Command:

```bash
python3 scripts/myflow_build_reward_labels.py \
  --snapshot <snapshot|latest> \
  --myflow-dir data/myflow \
  --out-dir data/myflow_rewards
```

- Inputs:
  - `data/myflow/<snapshot>/train_events.jsonl`
- Outputs:
  - `data/myflow_rewards/<snapshot>/train_event_rewards.jsonl`
  - `data/myflow_rewards/<snapshot>/reward_summary.json`
- Exit contract:
  - `0` = labels generated
  - non-zero = parse/IO/schema error

## Task 3: Canary Promotion Gate

- Script: `scripts/myflow_eval_canary.py`
- Purpose: gate promotion based on reward quality and optional baseline deltas.
- Command:

```bash
python3 scripts/myflow_eval_canary.py \
  --candidate data/myflow_rewards/<snapshot>/train_event_rewards.jsonl \
  --baseline data/myflow_rewards/<baseline_snapshot>/train_event_rewards.jsonl \
  --report-out data/myflow_reports/<snapshot>/canary_gate.json \
  --min-candidate-mean 0.55 \
  --min-delta-mean 0.00 \
  --min-delta-ci95-low -0.02
```

- Inputs:
  - candidate rewards JSONL
  - optional baseline rewards JSONL
  - optional `--rollouts` for action-dominance gate
- Outputs:
  - canary gate report JSON
- Exit contract:
  - `0` = promotion gate pass
  - `1` = gate fail (expected for regressions)
  - non-zero other = runtime error

## Task 4: Mine Hardcases

- Script: `scripts/myflow_mine_hardcases.py`
- Purpose: mine regressions/low-reward canary samples and produce next-cycle seed set.
- Command:

```bash
python3 scripts/myflow_mine_hardcases.py \
  --snapshot <snapshot|latest> \
  --prepared-dir data/myflow_prepared \
  --candidate-rewards data/myflow_rewards/<snapshot>/train_event_rewards.jsonl \
  --baseline-rewards data/myflow_rewards/<baseline_snapshot>/train_event_rewards.jsonl \
  --out-dir data/myflow_hardcases \
  --top-k 100
```

- Inputs:
  - prepared canary/train splits
  - candidate rewards
  - optional baseline rewards
- Outputs:
  - `hardcases.jsonl`
  - `next_train_seed.jsonl`
  - `hardcase_summary.json`
- Exit contract:
  - `0` = hardcases emitted
  - non-zero = missing inputs / parse errors

## Recommended Harbor Flow Task Names

1. `myflow-validate-snapshot`
2. `myflow-build-reward-labels`
3. `myflow-eval-canary`
4. `myflow-mine-hardcases`

Use these names in Harbor task orchestration so docs/runbooks stay stable.

## Executed Verification (2026-02-18)

Executed end-to-end on a deterministic fixture snapshot:

1. `prepare_myflow_dataset.py` (30 rows input)
2. `myflow_validate_snapshot.py` -> `PASS`
3. `myflow_build_reward_labels.py` -> labels generated
4. `myflow_eval_canary.py` -> `Promotion gate: PASS`
5. `myflow_mine_hardcases.py` -> hardcases + next seed generated

Artifact root used during verification:

- `/var/folders/.../tmp.5arfBojfhp/*` (temporary run directory)

