# f setup

Bootstrap the project if needed, then run the `setup` task or print shell aliases.

## Quick Start

```bash
# Bootstrap if missing, then run setup task or print aliases
f setup

# Use a specific config file
f setup --config ./flow.toml
```

## Behavior

- If the project is not bootstrapped (no `.ai/` or `flow.toml`), it runs the bootstrap flow.
- If `flow.toml` defines a `setup` task, `f setup` runs that task.
- Otherwise, it prints shell aliases from `[alias]` in `flow.toml`.

## Options

| Option | Description |
|--------|-------------|
| `--config <PATH>` | Path to `flow.toml` (default: `./flow.toml`) |
