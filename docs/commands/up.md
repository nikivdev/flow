# f up

Bring a project up using lifecycle conventions.

## Quick Start

```bash
# Run lifecycle up (tries task "up", then "dev")
f up

# Pass args through to the selected task
f up -- --port 3001
```

## Behavior

- Loads nearest `flow.toml` (or `--config` path).
- If `[lifecycle.domains]` is configured:
  - runs `f domains add <host> <target> --replace`
  - runs `f domains up` (with configured engine when set)
- Runs lifecycle task:
  - `[lifecycle].up_task` when configured
  - otherwise fallback order: `up`, then `dev`

If no up task is found, command fails with guidance.

## Options

| Option | Description |
|--------|-------------|
| `--config <PATH>` | Path to `flow.toml` (default: `./flow.toml`, searches upward when default is missing) |
| `ARGS...` | Extra args passed to the selected lifecycle task |

## Recommended myflow config

```toml
[lifecycle]
up_task = "dev"

[lifecycle.domains]
host = "myflow.localhost"
target = "127.0.0.1:3000"
engine = "native"
remove_on_down = false
stop_proxy_on_down = false
```
