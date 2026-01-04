# f setup

Bootstrap the project if needed, generate a `flow.toml` if missing, then run the `setup` task or print shell aliases.

## Quick Start

```bash
# Bootstrap if missing, generate flow.toml if missing, then run setup task or print aliases
f setup

# Use a specific config file
f setup --config ./flow.toml
```

## Behavior

- If the project is not bootstrapped, it runs the bootstrap flow (`.ai/`, `.gitignore`).
- If `flow.toml` is missing, it prompts to generate `setup` + `dev` tasks (AI via `gen` if available, otherwise manual prompts).
- If `flow.toml` defines a `setup` task, `f setup` runs that task.
- Otherwise, it prints shell aliases from `[alias]` in `flow.toml`.

## Options

| Option | Description |
|--------|-------------|
| `--config <PATH>` | Path to `flow.toml` (default: `./flow.toml`) |
