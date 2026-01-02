# f global

Run tasks from your global flow config (`~/.config/flow/flow.toml`).

## Quick Start

```bash
# List global tasks
f global --list
f global list

# Run a global task from anywhere
f global repos-clone-safari
f global run repos-clone-safari

# Match a query against global tasks
f global match "clone safari repo"
```

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `<TASK>` | | Global task name (omit to list) |
| `list` | | List global tasks |
| `run <TASK>` | | Run a global task |
| `match <QUERY>` | | Match a query against global tasks |
| `--list` | `-l` | List global tasks |
| `-- <ARGS...>` | | Pass extra args to the task |

## Examples

```bash
f global repos-clone-safari https://github.com/0xPlaygrounds/rig
```
