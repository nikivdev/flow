# f skills

Manage Codex/Claude skills for the current project.

Skills live in `.ai/skills/` and are symlinked to `.codex/skills` and `.claude/skills` so active agent sessions can discover them.

## Core Commands

```bash
# List local skills
f skills

# Create/edit/remove skills
f skills new <name> -d "description"
f skills edit <name>
f skills remove <name>

# Install curated skills
f skills install <name>

# Generate one skill per flow.toml task
f skills sync

# Force Codex app-server to rescan skills for this cwd
f skills reload
```

## Codex Tight Feedback Loop

Use this loop when coding with Codex/Claude:

1. Make code changes.
2. Run focused tests quickly (in Bun repos: `bun bd test ...`).
3. Refresh skill context:

```bash
f skills sync
f skills reload
```

4. Commit via quality gates:

```bash
f commit
```

`f skills reload` is useful for already-open Codex sessions; it refreshes the app-server skill cache without creating a new session.

## `flow.toml` Settings

```toml
[skills]
sync_tasks = true
install = ["quality-bun-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false
```

## Built-in Default Skills

Flow auto-materializes a small baseline set of project-local skills in `.ai/skills/`:

- `env`
- `quality-bun-feature-delivery`
- `pr-markdown-body-file`

These are symlinked into `.codex/skills` and `.claude/skills` and can be reloaded with:

```bash
f skills reload
```

### `skills.codex` fields

- `generate_openai_yaml`: writes `.ai/skills/<task>/agents/openai.yaml` for task-synced skills.
- `force_reload_after_sync`: after `f skills sync` or `f skills install`, force Codex app-server `skills/list` with `forceReload: true`.
- `task_skill_allow_implicit_invocation`: default `policy.allow_implicit_invocation` value in generated `agents/openai.yaml`.

## Recommended Enforcement

```toml
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

This blocks commits that skip required Bun-oriented testing/skill policy.
