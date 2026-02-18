# f fast

Run AI tasks through the low-latency fast client path.

This command is optimized for hot-loop invocation and prefers `fai`/`ai-taskd-client` over full `f` task startup.

## Usage

```bash
f fast ai:flow/noop
f fast ai:flow/bench-cli -- --iterations 30
f fast --root ~/code/flow ai:flow/dev-check
f fast --no-cache ai:flow/dev-check
```

## Behavior

1. Tries fast client dispatch (`fai`, local `target/.../ai-taskd-client`, or `ai-taskd-client` on PATH).
2. If daemon is not running, starts `ai-taskd` and retries.
3. Falls back to direct daemon dispatch if no fast client binary is found.

## Options

- `--root <PATH>`: root directory for `.ai/tasks` discovery (default `.`)
- `--no-cache`: disable cached binary execution and use direct Moon run
- `TASK`: required AI selector like `ai:flow/dev-check`
- trailing args after `--` are passed to the task

## Notes

- `f fast` is intentionally for `ai:*` selectors only.
- Install `fai` for best latency:

```bash
f install-ai-fast-client
f tasks daemon start
f fast ai:flow/noop
```

For pooled burst execution and timings, use `fai` directly:

```bash
fai --timings ai:flow/noop
printf 'ai:flow/noop\nai:flow/noop\n' | fai --batch-stdin --timings
```
