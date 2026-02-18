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
f bench-ai-runtime --iterations 80 --warmup 10
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
- `f recipe` still exists for legacy compatibility, but task-centric workflow is preferred.
