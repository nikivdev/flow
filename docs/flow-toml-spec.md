# flow.toml Specification

Minimal schema for Flow CLI tasks and managed dependencies. Designed for easy refactors and LLM prompting.

## File Layout

```toml
version = 1
name = "my-project"      # optional human-friendly project name

[deps]                # optional: command deps or managed pkg specs
# key = "cmd"         # single command on PATH
# key = ["cmd1","cmd2"] # multiple commands
# key = { pkg-path = "ripgrep", version = "14" } # managed pkg descriptor

[flox]                # optional: install set for managed env (applies to all tasks)
[flox.install]
# name.pkg-path = "ripgrep"
# name.version = "14.1.1"
# name.pkg-group = "tools"        # optional grouping
# name.systems = ["x86_64-darwin"] # optional target systems
# name.priority = 10              # optional ordering hint

[[tasks]]             # project tasks
name = "setup"        # required, unique
command = "cargo check"
description = "Compile workspace" # optional
activate_on_cd_to_root = true     # optional, default false
dependencies = ["fast"]           # optional, names from [deps] or [flox.install]
shortcuts = ["s"]                 # optional aliases for task lookup

[skills]              # optional: skill enforcement (gitignored by default)
sync_tasks = true     # optional: generate skills for tasks
install = ["linear"]  # optional: ensure skills are installed (local ~/.codex/skills preferred, else registry)
[skills.seq]          # optional: seq-backed dependency skill fetching defaults
# seq_repo = "~/code/seq"
# out_dir = ".ai/skills"
# scraper_base_url = "http://127.0.0.1:7444"
# allow_direct_fallback = true

[[alias]]             # optional shell aliases (or use [aliases] table)
fr = "f run"          # key/value pairs of alias -> command

[aliases]             # optional table alternative to [[alias]]
fr = "f run"
```

## Semantics

- `version`: currently `1`.
- `name`: optional display name for the project (useful in history/metadata).
- `[deps]`: map of dependency names to either:
  - string (single command to check on PATH),
  - string array (multiple commands),
  - table with `pkg-path` (+ optional `version`, `pkg-group`, `systems`, `priority`) for managed pkg.
- `[flox.install]`: global managed packages always included when any task runs inside the managed env.
- `tasks.dependencies`: names resolved against `[deps]` first, then `[flox.install]`.
- Tasks run inside the managed env when any managed deps are present; otherwise they use host PATH.
- `activate_on_cd_to_root`: tasks flagged run automatically when Flow is invoked via `activate` hooks.
- `shortcuts`: case-insensitive aliases and abbreviations (auto-generated from task names) resolve tasks.
- `alias`/`aliases`: emitted by `f setup` as shell `alias` lines.
- `[skills]`: optional skill enforcement; `sync_tasks` generates `.ai/skills` from tasks and `install` ensures registry skills are present (skills are gitignored by default).
- `[skills.seq]`: optional defaults for `f skills fetch ...` (local seq scraper integration).

## Notes

- Unsupported keys are ignored or will error; keep to this schema.
- Managed env tooling currently assumes `flox` is installed.
- Paths in commands are executed via `/bin/sh -c` in the config’s directory unless overridden.
