# Managing Claude and Codex Sessions with Flow

Flow treats Claude and Codex as first-class coding runtimes in the same project loop: tasks, sessions, skills, and commit gates all live together.

## Core Workflow

```bash
# 1) Enter repo and inspect runnable workflows
cd <repo>
f tasks list

# 2) Resume exact prior AI context
f ai claude resume <session-id>
# or
f ai codex resume <session-id>

# 3) Keep skills synced to current tasks
f skills sync
f skills reload

# 4) Run the smallest meaningful validation
f test-related

# 5) Commit through flow's review/testing gates
f commit
```

This is the fastest way to keep context stable and avoid drift across sessions.

## Session Operations

```bash
# Fuzzy-select and resume (all providers)
f ai

# Provider-specific listing
f ai claude list
f ai codex list

# Resume by exact ID / prefix / alias
f ai claude resume a38cf8bf-f4e2-4308-8b27-0254f89c4385
f ai codex resume 019c61c5-0aef-71a1-b058-5c9ab43013d4
f ai resume my-feature

# Save alias
f ai save my-feature --id <session-id>

# Copy context/history for handoff
f ai context
f ai copy
```

## Resume Semantics You Should Rely On

### Exact Claude resumes are strict

When you pass an explicit session (`name` or `id`), Flow will not auto-open a different conversation if that ID fails.

- tries `claude --resume <id>`
- if failed, exits non-zero
- no automatic `--continue` fallback for explicit IDs

This prevents restoring into the wrong session.

### Claude no-arg resume can fallback

`f ai claude resume` (no argument) resumes the most recent Claude session for this repo. If that fails, Flow can fallback to `claude --continue` in the same cwd.

### Codex resume is direct

Flow resumes Codex by session ID and returns failure if resume fails. No fallback to another session is applied.

### TTY is required

Both Claude and Codex resume commands require an interactive terminal (TTY). In non-interactive shells, Flow fails fast with a clear error.

## Choosing Claude vs Codex in Flow

- Use Claude sessions when you want broader planning + deep repo narrative continuity.
- Use Codex sessions when you want tight code-edit and review loops with strong tool execution.
- Keep both available in the same repo; switch by resuming the exact session you need.

## Task-Driven AI Coding (Important)

AI sessions are most reliable when code execution goes through `flow.toml` tasks.

If a task prompts for input (like `Y/n/a/q` workflows), mark it:

```toml
[[tasks]]
name = "reclaim"
command = "./mole reclaim"
interactive = true
```

This keeps TTY passthrough correct for both humans and AI-assisted loops.

## Related Docs

- `commands/ai.md` for command-level semantics and examples
- `commands/skills.md` for skill sync/reload loop
- `commands/commit.md` for commit quality/testing gates
- `use-flow-to-write-software-better.md` for the full operating model
