# f setup

Bootstrap the project if needed, generate a `flow.toml` if missing, then run the `setup` task or print shell aliases.

## Quick Start

```bash
# Bootstrap if missing, generate flow.toml if missing, then run setup task or print aliases
f setup

# Configure host deployment (Linux)
f setup deploy

# Configure release hosting for server projects
f setup release

# Use a specific config file
f setup --config ./flow.toml
```

## Behavior

- If the project is not bootstrapped, it runs the bootstrap flow (`.ai/`, `.gitignore`).
- If `flow.toml` is missing, it prompts to generate `setup` + `dev` tasks (AI via `gen` if available, otherwise manual prompts).
- If `flow.toml` already exists, Flow non-destructively appends missing Codex baseline sections (`[skills]`, `[skills.codex]`, commit skill gate, and Bun testing gate in Bun contexts).
- After baseline upgrades, Flow triggers a Codex skills reload (respecting `[skills.codex].force_reload_after_sync`) so open sessions pick up changes immediately.
- If `flow.toml` defines a `setup` task, `f setup` runs that task.
- After the `setup` task exits, Flow re-reads `flow.toml`, re-syncs task skills to `.ai/skills`, and reloads Codex skills (when configured). This makes setup-generated task changes visible to Claude/Codex immediately.
- Otherwise, it prints shell aliases from `[alias]` in `flow.toml`.
- After successful completion, Flow writes a setup checkpoint to `.rise/setup.json` in the repo root.
- `f setup deploy` adds a `[host]` section, creates a remote setup script, copies env templates, and optionally stores the deploy host.
- `f setup release` detects server projects and offers Linux host deployment defaults.

## Options

| Option | Description |
|--------|-------------|
| `--config <PATH>` | Path to `flow.toml` (default: `./flow.toml`) |
| `TARGET` | Optional setup target (e.g., `deploy`, `release`) |

### Global Server Setup Defaults

You can provide a server template in your global config at `~/.config/flow/flow.toml`:

```toml
[setup.server]
template = "/Users/nikiv/infra/flow.toml"
```

Or inline host defaults:

```toml
[setup.server.host]
setup = """#!/usr/bin/env bash
set -euo pipefail
..."""
env_file = ".env.host"
port = 3000
```
