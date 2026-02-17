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
[skills.codex]        # optional: Codex-specific skill metadata/reload behavior
# generate_openai_yaml = true
# force_reload_after_sync = true
# task_skill_allow_implicit_invocation = false
[skills.seq]          # optional: seq-backed dependency skill fetching defaults
# seq_repo = "~/code/seq"
# out_dir = ".ai/skills"
# scraper_base_url = "http://127.0.0.1:7444"
# allow_direct_fallback = true

[commit]             # optional: commit workflow defaults
# quick-default = true        # make `f commit` behave like `--quick` by default
# queue = false               # keep fast path pushing by default
# queue_on_issues = false     # do not force queue-only flow on review issues
# message-fallbacks = ["rise:zai:glm-5", "rise:cerebras:gpt-oss-120b", "remote", "openai"]
# review-fallbacks = ["glm5", "rise:cerebras:gpt-oss-120b", "codex-high"]

[options]            # optional: transport/runtime integrations
# myflow_mirror = true         # mirror commit + queue review events to myflow
# myflow_url = "https://myflow.sh"
# myflow_token = "..."
# codex_bin = "/path/to/codex-wrapper"  # must support `app-server` JSON-RPC
# codex_bin = "/Users/nikiv/code/flow/scripts/codex-jazz-wrapper"
# You can set this in repo flow.toml or global ~/.config/flow/flow.toml

[commit.testing]      # optional: local commit-time test gate
# mode = "warn"       # "warn" | "block" | "off"
# runner = "bun"      # currently optimized for bun in Bun repos
# bun_repo_strict = true
# require_related_tests = true
# ai_scratch_test_dir = ".ai/test"
# run_ai_scratch_tests = true
# allow_ai_scratch_to_satisfy_gate = false
# max_local_gate_seconds = 20

[commit.skill_gate]   # optional: require specific local skills before commit
# mode = "warn"       # "warn" | "block" | "off"
# required = ["quality-bun-feature-delivery"]
[commit.skill_gate.min_version]
# quality-bun-feature-delivery = 2

[git]                 # optional: git remote defaults for commit/sync
# remote = "origin"   # e.g. "myflow-i" for contributor mirror repos

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
- `[skills.codex]`: optional Codex tuning; task skill `agents/openai.yaml` generation, post-sync force reload, and implicit invocation policy defaults.
- `[skills.seq]`: optional defaults for `f skills fetch ...` (local seq scraper integration).
- `[commit]`: optional commit workflow defaults; `quick-default = true` makes plain `f commit` use fast commit + deferred Codex deep review.
- `[options]`: optional integration/runtime toggles; use `myflow_mirror` for mirror sync and `codex_bin` to route review calls through a wrapper transport.
- `[commit.testing]`: optional local testing gate evaluated during `f commit`; supports Bun-first strict mode plus optional AI scratch-test fallback (`.ai/test` by default).
- `[commit.skill_gate]`: optional required-skill policy for `f commit`; can enforce presence and minimum skill versions.
- `[git].remote`: preferred writable remote used by `f commit`/`f sync --push` (and jj remote defaults). Fallback order is `[git].remote`, then legacy `[jj].remote`, then `origin`.

## Notes

- Unsupported keys are ignored or will error; keep to this schema.
- Managed env tooling currently assumes `flox` is installed.
- Paths in commands are executed via `/bin/sh -c` in the config’s directory unless overridden.

## Codex-First Baseline

Use this baseline when optimizing for Codex/Claude sessions and tight feedback loops:

```toml
[skills]
sync_tasks = true
install = ["quality-bun-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false

[commit.testing]
mode = "block"
runner = "bun"
bun_repo_strict = true
require_related_tests = true
ai_scratch_test_dir = ".ai/test"
run_ai_scratch_tests = true
allow_ai_scratch_to_satisfy_gate = false
max_local_gate_seconds = 20

[commit.skill_gate]
mode = "block"
required = ["quality-bun-feature-delivery"]

[commit.skill_gate.min_version]
quality-bun-feature-delivery = 2
```
