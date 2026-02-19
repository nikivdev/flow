# f down

Bring a project down using lifecycle conventions.

## Quick Start

```bash
# Run lifecycle down task and optional domains teardown
f down
```

## Behavior

- Loads nearest `flow.toml` (or `--config` path).
- Runs lifecycle task:
  - `[lifecycle].down_task` when configured
  - otherwise fallback: `down`
- If no down task is found and `down_task` is not explicitly set, Flow falls back to killing all running Flow-managed processes for the current project (`f kill --all` behavior).
- If `[lifecycle.domains]` is configured, optional teardown is applied:
  - `remove_on_down = true` -> removes configured host route
  - `stop_proxy_on_down = true` -> stops shared local domains proxy
- On macOS launchd-managed native domains, stopping native proxy is handled by:
  - `sudo ./tools/domainsd-cpp/uninstall-macos-launchd.sh`

If neither a down task nor lifecycle domain teardown is configured, command fails with guidance.

## Options

| Option | Description |
|--------|-------------|
| `--config <PATH>` | Path to `flow.toml` (default: `./flow.toml`, searches upward when default is missing) |
| `ARGS...` | Extra args passed to the selected lifecycle task |
