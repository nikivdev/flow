# f reviews-todo

Manage deferred deep-review todos for queued commits.

This command is a workflow wrapper over `f commit-queue` for the fast-commit + deep-Codex-review loop.

## Quick Start

```bash
# List pending deep-review todos (queued commits)
f reviews-todo list

# Run Codex deep review for all queued commits
f reviews-todo codex --all

# Inspect one queued review todo
f reviews-todo show <commit-sha>

# Approve all queued commits after issues are addressed
f reviews-todo approve-all
```

## Why use this

- Keep `f commit` fast.
- Batch expensive Codex reviews later.
- Keep one place to track deep-review backlog.

## Notes

- `codex --all` maps to `f commit-queue review --all`.
- Queue entries live under `.ai/internal/commit-queue/`.
- Review findings are still recorded into `.ai/todos/todos.json` and commit review reports.
- If `[options].myflow_mirror = true` is enabled, queued Codex reviews from the quick path are mirrored to myflow as `commit_queue_review` events.
- For a full speed-first operating loop in `~/code/myflow`, see [`../fast-commit-deep-review-loop.md`](../fast-commit-deep-review-loop.md).
