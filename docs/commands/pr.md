# f pr

Create or open GitHub pull requests, edit PR text locally, and pull review feedback.

## Quick Start

```bash
# Create/update PR from current changes (default base: main)
f pr "assistant improvements"

# Open current branch PR in browser
f pr open

# Edit PR title/body in local markdown and sync on save
f pr open edit

# Pull actionable review feedback for current PR
f pr feedback

# Pull feedback for specific PR and store as local todos
f pr feedback 8 --todo
f pr feedback https://github.com/owner/repo/pull/8 --todo
```

## Feedback Workflow

`f pr feedback` fetches:

- PR reviews (`/pulls/<n>/reviews`)
- Inline review comments (`/pulls/<n>/comments`)
- Top-level PR comments (`/issues/<n>/comments`)

It then:

1. Prints a concise actionable list in terminal.
2. Writes a snapshot to `.ai/reviews/pr-feedback-<pr>.md`.
3. Optionally (`--todo`) records feedback into `.ai/todos/todos.json` with dedupe via external refs.

## Notes

- If no selector is passed, Flow resolves the PR from the current branch.
- `f pr feedback --todo` is safe to re-run; existing feedback refs are not duplicated.
- `f pr open edit` remains the quickest path to tweak PR title/body from local editor.
