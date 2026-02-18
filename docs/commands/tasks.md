# f tasks

List and discover project tasks from:

- `flow.toml` (`[[tasks]]`)
- `.ai/tasks/*.mbt` (AI MoonBit tasks)

You can run tasks directly with `f <task>`.

## Usage

```bash
f tasks
f tasks list
f tasks dupes
f tasks init-ai
f tasks build-ai ai:flow/dev-check
f tasks run-ai ai:flow/dev-check
f tasks run-ai --daemon ai:flow/dev-check
f tasks daemon start
f tasks daemon status
f tasks daemon stop
cargo build --release --bin ai-taskd-client
./target/release/ai-taskd-client ai:flow/dev-check
f install-ai-fast-client
f fast ai:flow/dev-check
f bench-ai-runtime --iterations 80 --warmup 10
f bench-ffi-boundary --iters 10000000
f bench-ffi-boundary --iters 10000000 --native-opt
```

## AI Task Workflow

Initialize a starter MoonBit task:

```bash
f tasks init-ai
```

This creates:

```text
.ai/tasks/starter.mbt
```

Run it:

```bash
f starter
f ai:starter
```

Add more tasks as `.mbt` files under `.ai/tasks/` and run by name or selector:

```bash
f release-flow
f ai:project/release-flow
```

## Notes

- Default execution mode is cached native binary:
  1) `moon build --target native --release` once per content hash
  2) run cached artifact from `~/Library/Caches/flow/ai-tasks/...`
- Use `f tasks run-ai --no-cache ...` (or `FLOW_AI_TASK_RUNTIME=moon-run`) to force direct `moon run`.
- Set `FLOW_AI_TASK_MODE=release` for release builds (`--release`).
- Set `FLOW_AI_TASK_MODE=js` to run with JS target.
- `f tasks daemon` runs a lightweight local `ai-taskd` over Unix socket for warm repeated runs.
- For lowest invocation overhead, use the tiny client binary against the daemon:
  - `cargo build --release --bin ai-taskd-client`
  - `./target/release/ai-taskd-client ai:<selector>`
  - or `f install-ai-fast-client` then use `fai ai:<selector>`
  - This bypasses full `f` startup for hot-loop calls.
- Automatic preference for latency-critical AI selectors:
  - Opt in with `FLOW_AI_TASK_FAST_CLIENT=1` (typically together with `FLOW_AI_TASK_DAEMON=1`).
  - Then `f` will auto-prefer fast client dispatch for AI tasks tagged `fast`, `latency`, `hot`, or `hotkey`.
  - Override selector matching with `FLOW_AI_TASK_FAST_SELECTORS` (comma-separated patterns, supports `*` prefix/suffix).
  - Override client binary with `FLOW_AI_TASK_FAST_CLIENT_BIN=/path/to/fai`.
- `f recipe` still exists for legacy compatibility, but task-centric workflow is preferred.
