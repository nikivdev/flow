---
name: quality-bun-feature-delivery
description: Enforce tight Codex/Claude delivery loop with Bun-first testing, skill sync/reload, and commit quality gates.
version: 2
source: flow-default
---

# Quality Bun Feature Delivery

Use this workflow for every feature/change.

## Rules (Must)

- In Bun repos, run tests with `bun bd test ...` (never `bun test` for final validation).
- Run only relevant tests first (single file/filter) for fast feedback.
- Keep `.ai/features/*.md` and tests current for touched features.
- Do not finish work without a passing local verification command.

## Tight Loop

1. Update code + tests.
2. Run focused tests with Bun debug build:

```bash
bun bd test <target-test-file> -t "<optional filter>"
```

3. Sync task skills to keep agent context fresh:

```bash
f skills sync
```

4. Force Codex to reload skills for this cwd:

```bash
f skills reload
```

5. Commit through Flow quality gates:

```bash
f commit
```

## Bun Regression Check (for new tests)

When adding/changing tests in Bun itself, ensure the test fails on system Bun and passes on debug Bun:

```bash
USE_SYSTEM_BUN=1 bun test <target-test-file>
bun bd test <target-test-file>
```

## Recommended flow.toml

```toml
[skills]
sync_tasks = true
install = ["quality-bun-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false

[commit.testing]
mode = "block"
runner = "bun"
bun_repo_strict = true
require_related_tests = true
max_local_gate_seconds = 20

[commit.skill_gate]
mode = "block"
required = ["quality-bun-feature-delivery"]

[commit.skill_gate.min_version]
quality-bun-feature-delivery = 2
```
