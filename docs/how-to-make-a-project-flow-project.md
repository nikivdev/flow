# How To Make A Project A Flow Project

This guide is for both:

- a brand-new repository
- an existing repository that already has scripts/tooling

The goal is to make `flow.toml` the project control plane so local development, AI workflows, quality gates, and deploy operations all run through `f`.

---

## What "Flow Project" Means

A project is Flow-managed when it has:

1. A `flow.toml` at repo root.
2. Core workflows exposed as `[[tasks]]` in `flow.toml`.
3. Secrets/environment managed by `f env` instead of committed `.env` files.
4. Commits made through `f commit` with explicit quality/testing policy.
5. Optional AI task and skills wiring (`.ai/tasks`, `.ai/skills`, `[skills]`).

If your team can run `f tasks`, `f <task>`, `f env`, and `f commit` end-to-end from repo root, the project is Flow-native.

---

## Step 0: Machine Prerequisites

On each developer machine:

```bash
f doctor
f auth login
f latest
```

Why:

- `f doctor` catches missing tooling and shell issues early.
- `f auth login` enables cloud-backed features (`f env`, remote flows).
- `f latest` avoids inconsistent command behavior across team members.

---

## Step 1: Bootstrap The Repo

From repository root:

```bash
cd /path/to/repo
f setup
```

`f setup` will:

- bootstrap project metadata (`.ai/`, `.gitignore` integration)
- generate `flow.toml` if missing
- append missing Codex baseline sections in existing `flow.toml` files
- run `setup` task if one exists

If `flow.toml` does not exist yet, `f setup` is the fastest way to initialize safely.

---

## Step 2: Define A Strong `flow.toml` Baseline

Use this as a practical starter and replace commands with your real project commands.

```toml
version = 1
name = "my-project"

[skills]
sync_tasks = true
install = ["quality-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false

[[tasks]]
name = "setup"
command = "echo 'project setup checks here'"
description = "Prepare local environment and verify prerequisites"

[[tasks]]
name = "dev"
command = "npm run dev"
description = "Start local development server"

[[tasks]]
name = "test"
command = "npm test"
description = "Run test suite"

[[tasks]]
name = "test-related"
command = "npm run test:related"
description = "Run smallest useful tests for current diff"

[[tasks]]
name = "lint"
command = "npm run lint"
description = "Lint source code"

[[tasks]]
name = "build"
command = "npm run build"
description = "Build production artifacts"

[commit]
review_instructions_file = ".ai/commit-review-instructions.md"

[commit.testing]
mode = "block"
require_related_tests = true
max_local_gate_seconds = 30

[commit.skill_gate]
mode = "block"
required = ["quality-feature-delivery"]

[invariants]
mode = "warn"
architecture_style = "layered architecture with task-based workflows"
non_negotiable = [
  "do not bypass Flow tasks for standard workflows",
  "keep user-visible changes documented"
]
forbidden = ["git reset --hard", "git add -A .env"]

[invariants.files]
max_lines = 600
```

Notes:

- Keep task names short and stable (`dev`, `test`, `build`, `deploy`) so AI sessions stay consistent.
- Prefer one canonical task per workflow. Avoid duplicate aliases with different behavior.

---

## Step 3: Migrate Existing Scripts Into Tasks

If you already have scripts in `package.json`, `Makefile`, shell scripts, or CI configs:

1. Map each workflow to one Flow task in `flow.toml`.
2. Keep script internals if needed, but make `f <task>` the public entrypoint.
3. Update docs/README to reference `f` commands first.

Example mapping:

- `npm run dev` -> `[[tasks]] name = "dev"`
- `make test` -> `[[tasks]] name = "test"`
- `./scripts/release.sh` -> `[[tasks]] name = "release"`

This prevents "works on my machine" command drift.

---

## Step 4: Move Secrets To `f env`

Do not commit secrets in repo files.

Use project-scoped env values:

```bash
f env project set -e dev API_KEY=sk-...
f env project set -e production API_KEY=sk-...
f env list -e dev
f env get -e production API_KEY -f value
```

Run commands with injected env:

```bash
f env run -e dev -- f dev
```

If your project deploys to Cloudflare, declare env policy in `flow.toml` and use `f env apply` / `f deploy cf`.

---

## Step 5: Make Commit Quality Non-Optional

Once baseline tasks and env are ready, enforce commit behavior:

```bash
f commit
```

Recommended:

- `f commit` for normal flow (fast commit + deferred deep review)
- `f commit --slow` when you want blocking review before commit

Policy lives in `flow.toml`:

- `[commit.testing]` for local test gate
- `[commit.skill_gate]` for required skills
- `[invariants]` for project-specific guardrails

Treat `--skip-tests` / `--skip-quality` as emergency-only.

---

## Step 6: Add AI Tasks (Optional, High Leverage)

Initialize AI task pack:

```bash
f tasks init-ai
f tasks list
f ai:starter
```

This creates `.ai/tasks/*.mbt` entries that can be run like normal tasks.

Use this for:

- structured automation
- repo-specific AI workflows
- low-latency repeatable helper routines

---

## Step 7: Wire Deploy Through Flow

Choose one deploy target in `flow.toml`:

- `[host]` for Linux SSH deploys
- `[cloudflare]` for Workers
- `[railway]` for Railway

Then run:

```bash
f deploy
```

Avoid ad hoc deploy commands in docs/CI when equivalent `f deploy` flow exists.

---

## Step 8: Update Team And CI Entry Points

Make team defaults explicit:

1. README Quick Start uses `f setup`, `f dev`, `f test`, `f commit`.
2. CI scripts call Flow tasks (`f test`, `f build`) instead of bespoke command chains.
3. Contributors run from repo root to avoid task resolution surprises.

Simple CI shape:

```bash
f setup
f test
f build
```

---

## Validation Checklist

Run this from repo root:

```bash
f setup
f tasks list
f test
f env list -e dev
f commit --dry
```

You are done when:

1. `flow.toml` defines all core workflows as tasks.
2. Secrets are in `f env`, not committed files.
3. `f commit` runs with your intended testing/quality policy.
4. New contributors can get productive with `f setup` + `f tasks list`.
5. CI and deploy flows run through Flow entrypoints.

---

## Common Migration Mistakes

1. Keeping parallel command paths (`npm run ...` in docs and `f ...` in flow): pick Flow as canonical.
2. Defining tasks without descriptions: hurts discoverability for humans and AI.
3. Leaving commit policy at defaults: set `[commit.testing]`, `[commit.skill_gate]`, and invariants intentionally.
4. Treating `flow.toml` as static: update it whenever workflow changes.

---

## Recommended Next Reads

- `docs/flow-toml-spec.md`
- `docs/commands/setup.md`
- `docs/commands/tasks.md`
- `docs/commands/env.md`
- `docs/commands/commit.md`
- `docs/how-to-use-flow-to-deploy.md`

