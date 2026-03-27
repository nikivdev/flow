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

# Full inline diff/review context is now the default
f pr feedback 8

# Use terse terminal output if needed
f pr feedback 8 --compact
```

## Feedback Workflow

`f pr feedback` fetches:

- PR reviews (`/pulls/<n>/reviews`)
- Inline review comments (`/pulls/<n>/comments`)
- Top-level PR comments (`/issues/<n>/comments`)

It then:

1. Prints an actionable list in terminal with inline review state and diff hunk context by default.
2. Writes a markdown snapshot to `.ai/reviews/pr-feedback-<pr>.md`.
3. Writes a machine-readable JSON snapshot to `.ai/reviews/pr-feedback-<pr>.json`.
4. Writes a human review plan to `~/docs/plan/review/<repo>-pr-<pr>-feedback.md`.
5. Writes a PR-local execution artifact to `~/docs/plan/review/<repo>-pr-<pr>-review-rules.md`.
6. Writes a Kit system prompt to `~/docs/plan/review/<repo>-pr-<pr>-kit-system.md`.
7. Optionally (`--todo`) records feedback into `.ai/todos/todos.json` with dedupe via external refs.

The review plan includes ready-to-run `kit` commands for:

- deterministic repo review via `kit review`
- preventative lint/review rule synthesis from the fetched GitHub feedback set

The generated `*-review-rules.md` artifact contains the per-item resolution loop,
prompt template, required response sections, and Kit-upgrade decision order so
the workflow can be reopened without extra instructions.

## Notes

- If no selector is passed, Flow resolves the PR from the current branch.
- `f pr feedback --todo` is safe to re-run; existing feedback refs are not duplicated.
- `f pr feedback` is full-context by default. Use `--compact` if you want the older terse terminal view.
- `f pr feedback` now emits a Kit-ready handoff, so the same feedback set can drive a future review bot instead of staying trapped in GitHub comments.
- `f pr open edit` remains the quickest path to tweak PR title/body from local editor.
