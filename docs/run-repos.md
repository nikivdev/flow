# Run Repos Shortcuts (`f r`, `f ri`, `f rp`, `f rip`)

This workflow lets you run Flow tasks in `~/run` and `~/run/i` from anywhere,
without manual `cd`.

## Standard Layout

```text
~/run/            # public run repo (has flow.toml)
~/run/i/          # internal run repo (has flow.toml)
~/run/i/linsa/    # nested internal project example
```

`f health` now ensures `~/run` and `~/run/i` directories exist.

Root behavior:
- This is a hard path: run repos live under `~/run`.
- `RUN_ROOT` can still override the root explicitly.

## Primary Commands

| Command | Meaning |
|---|---|
| `f r <task> [args...]` | Run task in `~/run` |
| `f ri <task> [args...]` | Run task in `~/run/i` |
| `f rp <project> <task> [args...]` | Run task in project under run tree |
| `f rip <project> <task> [args...]` | Run task in `~/run/i/<project>` |

## Resolution Rules

`f rp <project> ...` resolves in this order:

1. `~/run/<project>`
2. `~/run/i/<project>` (fallback)

If both exist, Flow fails with an ambiguity error and asks for explicit path:

- `f rp <project> ...` for public path
- `f rp i/<project> ...` or `f rip <project> ...` for internal path

## Nested Project Support

Nested `flow.toml` projects are supported. Example:

```bash
f rip linsa bootstrap
f rp linsa opencode-codex-login
```

Both target `~/run/i/linsa` (unless `~/run/linsa` also exists).

## Why This Is Robust

- Uses explicit `f run --config <dir>/flow.toml <task>` internally.
- Avoids task-lookup ambiguity when nested `flow.toml` files exist.
- Blocks unsafe paths (`/absolute`, `..` traversal) for run repo/project selectors.

## Discovery and Maintenance

```bash
f run-list           # list all flow.toml repos/projects under ~/run (recursive)
f run-sync           # sync all git repos under ~/run (recursive)
f run-sync i         # sync only ~/run/i
```

## Script Interface

Task shortcuts are powered by:

```bash
scripts/run-repos.sh
```

Direct script commands:

```bash
bash ./scripts/run-repos.sh r <task> [args...]
bash ./scripts/run-repos.sh ri <task> [args...]
bash ./scripts/run-repos.sh rp <project> <task> [args...]
bash ./scripts/run-repos.sh rip <project> <task> [args...]
```

`RUN_ROOT` can be overridden for testing:

```bash
RUN_ROOT=/tmp/my-run-layout f rp linsa whoami
```
