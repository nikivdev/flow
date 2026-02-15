# Use Flow To Write Software Better

This is a practical, opinionated guide for using Flow as the control plane for software delivery, optimized for Claude Code and Codex.

The goal is simple: tighter feedback loops, fewer regressions, less context loss, and consistent quality gates.

---

## 1. Core idea: one operating loop

Do not treat Flow as just a task runner. Use it as the enforced loop:

1. Start with project context and reusable skills.
2. Implement in Claude/Codex with task-native commands.
3. Run the smallest meaningful tests first.
4. Capture traces/logs when behavior is unclear.
5. Commit through `f commit` with quality/testing/skill gates.
6. Ship through Flow tasks, not ad hoc commands.

If you do this consistently, team behavior becomes predictable and AI sessions become reliable.

---

## 2. Machine baseline (once per machine)

Run these first:

```bash
f doctor
f auth login
f latest
```

What this gives you:

- verified shell and toolchain integration
- authenticated Flow AI and storage access
- latest Flow binary with current command behavior

If you use fish integration heavily:

```bash
f shell-init
```

---

## 3. Project baseline (once per repo)

From the repository root:

```bash
f info
f tasks list
f setup
```

If project is not Flow-managed yet:

```bash
f init
```

Then immediately add these foundations to `flow.toml`:

- `[skills]` and `[skills.codex]`
- `[commit.testing]`
- `[commit.quality]`
- `[commit.skill_gate]`
- core tasks (`test`, `test-related`, build, dev, deploy/ship)

---

## 4. Reference `flow.toml` pattern (AI-first, quality-enforced)

Use this as a starting profile and adjust per repo:

```toml
version = 1

[project]
name = "your-project"

[skills]
sync_tasks = true
install = ["quality-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false

[[tasks]]
name = "test"
command = "<your test command>"
description = "Run project tests"

[[tasks]]
name = "test-related"
command = "<script that runs likely related tests>"
description = "Run smallest related tests for changed files"

[commit]
review_instructions_file = ".ai/commit-review-instructions.md"

[commit.testing]
mode = "block"                    # off | warn | block
runner = "bun"                    # Bun-first local gate
require_related_tests = true
ai_scratch_test_dir = ".ai/test"  # optional gitignored AI scratch tests
run_ai_scratch_tests = true       # run scratch tests when no related tracked tests
allow_ai_scratch_to_satisfy_gate = false
max_local_gate_seconds = 30

[commit.quality]
mode = "block"
require_docs = true
require_tests = true
auto_generate_docs = true
doc_level = "basic"

[commit.skill_gate]
mode = "block"
required = ["quality-feature-delivery"]
```

Why this matters:

- `sync_tasks` + Codex skill generation makes tasks visible as skills.
- blocked commit gates make quality non-optional.
- related-test enforcement keeps the loop fast and relevant.

---

## 5. Daily development loop (the part that compounds)

### 5.1 Start every session from repo root

```bash
cd <repo>
f tasks list
```

Then choose one clear objective and one validation command before coding.

### 5.2 Drive execution through tasks

Prefer:

```bash
f dev
f test-related
f logs <task>
```

Avoid direct, inconsistent commands when equivalent Flow tasks exist.

### 5.3 Use Claude/Codex with explicit constraints

Your prompt should include:

- objective
- files or subsystem boundaries
- required tests
- expected output shape
- “commit through `f commit` without skip flags”

Example prompt frame:

```text
Implement X in Y files.
Run f test-related-main first, then broader tests if needed.
Update .ai/features for user-visible changes.
Commit using f commit with no skip flags.
```

### 5.4 Keep loop tight before broad

Order of validation:

1. related tests (`f test-related` / branch-based variant)
2. subsystem suite
3. full suite only if risk justifies

### 5.5 Commit only through `f commit`

```bash
f commit
```

This centralizes:

- AI review
- test/doc quality checks
- feature documentation updates
- sync/audit metadata

Do not bypass with `--skip-quality` or `--skip-tests` unless explicitly intentional.

---

## 6. Features-as-knowledge (`.ai/features`)

Treat `.ai/features/*.md` as the source of truth for what exists.

Each user-visible feature should map to:

- purpose/description
- source files
- test files
- coverage status
- last verified commit

Why this is high leverage:

- new AI sessions start with real project capabilities
- stale feature docs are detectable at commit time
- dashboard/reporting can track drift and coverage

---

## 7. Skills as enforced behavior (not optional tips)

Use local skills for repo-specific “how we build here”.

Recommended minimum skill set:

1. quality feature delivery (tests + docs + commit gates)
2. environment/secret usage (`f env` only)
3. release/ship protocol
4. tracing/diagnostics protocol

Then enforce with:

```toml
[commit.skill_gate]
mode = "block"
required = ["quality-feature-delivery"]
```

This is how you convert good intentions into default behavior.

---

## 8. Testing strategy for speed and confidence

### 8.1 Two test lanes

- lane A: very fast related tests for development iterations
- lane B: broader suite for pre-ship confidence

### 8.2 Make lane A deterministic

Use a script (like `.ai/scripts/test-related.ts`) that:

- maps changed source files to candidate tests
- supports `--base origin/main --head HEAD`
- can list commands without running (`--list`)
- runs the smallest useful subset first

### 8.3 Add preflight guards for expensive runners

If your runner fails due environment prerequisites (toolchain/vendor issues), add a preflight task:

- `f <runner>-ready`
- optional auto-repair task `f <runner>-fix`

This avoids burning minutes before obvious infra failures.

---

## 9. Logging and tracing loop

When behavior is unclear, switch from “guess and patch” to “observe and patch”:

1. run the target task via Flow
2. inspect `f logs <task>`
3. collect traces (`f trace` / project-specific trace tasks)
4. summarize signal before changing code

The best pattern is “capture once, reason once, patch once.”

---

## 10. Environment and secrets discipline

Use `f env` as the single path for secrets and runtime env management:

```bash
f env setup
f env set KEY=value
f env pull
f env run <command>
```

Avoid ad hoc `.env` drift across machines.

---

## 11. Shipping loop (release confidence)

For deployment or mobile shipping flows, define one confidence task that runs before release:

- health checks
- trace ingestion checks
- critical smoke test
- related tests for release-impact files

Then make ship task depend on that confidence task.

Example:

```text
f mobile-confidence -> f mobile-ship
```

Result: broken pipelines fail before expensive release steps.

---

## 12. Existing project onboarding (high-value sequence)

When adding Flow to an existing repo, use this order:

1. add `flow.toml` with core tasks
2. add env management (`[storage]` + `f env` flow)
3. add related-test task/script
4. add commit testing + quality + skill gates in warn mode
5. validate for 2-3 days
6. flip to block mode
7. add `.ai/features` for top capabilities

This avoids destabilizing the team while still moving to enforcement.

---

## 13. Prompt templates that work better

### Implementation prompt

```text
Implement <feature> in <scope>.
Use Flow tasks only (no ad hoc commands when task exists).
Run related tests first, then broaden if risk warrants.
Update .ai/features for user-visible behavior changes.
Commit with f commit (no skip flags).
```

### Debugging prompt

```text
Do not patch yet.
Collect logs/traces via Flow tasks and summarize likely root causes.
Propose smallest validating experiment.
After confirmation, implement fix + related tests + feature doc update.
Commit via f commit.
```

### Refactor prompt

```text
Refactor <module> without behavior changes.
Keep public API stable.
Run focused tests proving no regression.
Document any non-obvious migration risks.
Commit via f commit.
```

---

## 14. Anti-patterns to avoid

1. Running direct commands repeatedly when Flow tasks exist.
2. Treating tests as optional before `f commit`.
3. Using skip flags routinely.
4. Writing prompts without required validation commands.
5. Keeping feature docs as manual, stale notes.
6. Debugging by repeated blind edits instead of trace/log loop.

---

## 15. Operational checklists

### Start-of-day checklist

1. `f latest` (if Flow changed frequently)
2. `f tasks list`
3. `f ai` / `f codex` / `f claude` resume context
4. confirm one objective + one validation command

### Pre-commit checklist

1. related tests pass
2. feature docs updated (`.ai/features`)
3. no quality gate bypass intended
4. `f commit`

### Pre-ship checklist

1. confidence task passes
2. relevant traces/logs clean
3. release task run through Flow

---

## 16. Maturity model (how teams level up)

### Level 1: Convenience

- tasks run through `f`
- basic env usage

### Level 2: Consistency

- related test task
- shared review instructions
- reusable skills

### Level 3: Enforcement

- blocked testing/quality gates
- blocked skill gate
- `.ai/features` as living capability map

### Level 4: Observability-driven

- preflight + confidence tasks
- trace-first debugging
- structured release checks

Aim to reach Level 3 quickly, then Level 4 where release speed and reliability both improve.

---

## 17. Practical defaults for Codex/Claude-heavy teams

Use these defaults unless you have a reason not to:

- `commit.testing.mode = "block"`
- `commit.quality.mode = "block"`
- `commit.skill_gate.mode = "block"`
- `skills.sync_tasks = true`
- `skills.codex.generate_openai_yaml = true`
- `skills.codex.force_reload_after_sync = true`
- branch-diff related tests (`--base origin/main --head HEAD`)

This gives the highest consistency with the least manual memory burden.

---

## 18. Bottom line

Flow works best when it is the enforced operating system for development, not an optional helper.

If you route implementation, testing, docs, commit review, and shipping through Flow, you get:

- faster iteration
- lower regression rates
- shared project memory for humans and AI
- auditable delivery quality

That is the path to writing software better, repeatedly.
