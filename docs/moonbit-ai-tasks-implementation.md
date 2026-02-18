# MoonBit AI Tasks: Implementation Inventory

This document captures the task-centric MoonBit implementation added in Flow, where `.ai/tasks/*.mbt` is the primary extension mechanism.

## Scope

The implementation adds:

- discovery and execution of AI MoonBit tasks under `.ai/tasks/`
- CLI/docs updates to make `tasks` the primary interface
- legacy `recipe` command demoted to compatibility mode
- a concrete task pack for Flow self-development

## Code Paths Added/Changed

Core runtime and wiring:

- `src/ai_tasks.rs`
- `src/ai_taskd.rs`
- `src/tasks.rs`
- `src/palette.rs`
- `src/cli.rs`
- `src/lib.rs`

Legacy compatibility updates:

- `src/recipe.rs`

Docs updates:

- `docs/commands/tasks.md`
- `docs/commands/recipe.md`
- `docs/commands/readme.md`
- `docs/index.mdx`
- `docs/moonbit-ai-tasks-implementation.md`

Workspace hygiene:

- `.gitignore` (ignore Moon generated dirs under `.ai/tasks/**`)

## Runtime Behavior

### Discovery

Flow scans `.ai/tasks/` recursively for `.mbt` files and exposes selectors as `ai:<path>`.

Key behavior in `src/ai_tasks.rs`:

- ignores generated Moon artifacts during discovery (`.mooncakes`, `_build`)
- parses metadata from top comments:
  - `// title: ...`
  - `// description: ...`
  - `// tags: [a, b]`
- resolves stable task IDs and selectors from path layout

### Selection

Task resolution accepts:

- full selector: `ai:flow/dev-check`
- scoped selector forms
- name-based matching with ambiguity detection

### Execution

Flow executes AI tasks through a cache-first runtime.

Important execution details:

- auto-resolves nearest Moon workspace root (`moon.mod.json` / `moon.mod`)
- computes content hash (task source + Moon config + moon version)
- builds native artifact once (`moon build --target native --release`)
- reuses cached binary from `~/Library/Caches/flow/ai-tasks/<hash>/task-bin`
- falls back to `moon run` for tasks without Moon workspace metadata
- optional mode control via `FLOW_AI_TASK_MODE` (`dev`, `release`, `js`, etc.)
- default frozen dependency behavior unless `FLOW_AI_TASK_NO_FROZEN` is set
- runtime override via `FLOW_AI_TASK_RUNTIME=moon-run`

### Daemon

Flow now includes a lightweight Unix-socket daemon for repeated AI task runs:

- socket: `~/.flow/run/ai-taskd.sock`
- lifecycle: `f tasks daemon start|status|stop`
- daemon execution: `f tasks run-ai --daemon <selector>`

## Task Pack Added

Flow-local task pack under `.ai/tasks/flow/`:

- `.ai/tasks/flow/dev-check/main.mbt`
- `.ai/tasks/flow/pr-ready/main.mbt`
- `.ai/tasks/flow/regression-smoke/main.mbt`
- `.ai/tasks/flow/release-preflight/main.mbt`
- `.ai/tasks/flow/bench-cli/main.mbt`

Each task has its own Moon package/workspace files:

- `.ai/tasks/flow/<task>/moon.mod.json`
- `.ai/tasks/flow/<task>/moon.pkg.json`

### Task Intents

- `ai:flow/dev-check`: fast quality gate (`cargo check`, targeted tests, CLI help smoke)
- `ai:flow/pr-ready`: pre-PR gate (dev-check + docs parity + gitignore hygiene)
- `ai:flow/regression-smoke`: temporary project smoke for task discovery/execution
- `ai:flow/release-preflight`: build release binary and run release-path smoke checks
- `ai:flow/bench-cli`: quick latency benchmark for high-frequency Flow CLI entry points

## How To Run

From `~/code/flow`:

```bash
f tasks list
f tasks build-ai ai:flow/dev-check
f tasks run-ai ai:flow/dev-check
f tasks run-ai --daemon ai:flow/dev-check
f tasks daemon start
f tasks daemon status
f tasks daemon stop
f ai:flow/dev-check
f ai:flow/pr-ready
f ai:flow/regression-smoke
f ai:flow/release-preflight
f ai:flow/bench-cli
```

Optional benchmark controls:

```bash
FLOW_BENCH_ITERATIONS=30 FLOW_BENCH_WARMUP=5 f ai:flow/bench-cli
```

## Validation Commands

```bash
cargo check --all-targets
cargo build --release --bin f
f tasks list | rg '^ai:flow/'
f ai:flow/regression-smoke
```

## Generated Artifact Hygiene

To prevent accidental commit noise from Moon caches/build output, Flow ignores:

- `.ai/tasks/**/.mooncakes/`
- `.ai/tasks/**/_build/`

## Notes for Commits

When committing this work, scope to the relevant code + docs only:

```bash
f commit --path src/ai_tasks.rs \
  --path src/tasks.rs \
  --path src/palette.rs \
  --path src/cli.rs \
  --path src/lib.rs \
  --path src/recipe.rs \
  --path docs/commands/tasks.md \
  --path docs/commands/recipe.md \
  --path docs/commands/readme.md \
  --path docs/moonbit-ai-tasks-implementation.md \
  "add task-centric moonbit ai task runtime and flow task pack"
```
