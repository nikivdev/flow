# New PR (Flow + jj)

Use this when you want to create and iterate on a PR with Flow while keeping jj state clean.

## Goal

Create a PR from a queued commit/bookmark, avoid accidental extra commits, and keep PR metadata easy to edit.

## Recommended command flow

```bash
# 1) Start from repo root
cd ~/code/org/linsa/linsa

# 2) Sync trunk
f sync

# 3) Ensure jj is initialized (first time in repo)
f jj init

# 4) Create/track a feature bookmark (once per feature)
f jj bookmark create <bookmark-name> --track

# 5) Make changes, run checks, then queue commit (no push)
f commit --queue -m "<what changed>"

# 6) Create PR from queued commit (no new commit)
f pr --no-commit --base main

# 7) Edit PR title/body locally and auto-sync on save
f pr open edit
```

Important formatting rule:

- Do not pass multi-line PR body text as a quoted CLI string with escaped `\n`.
- Use file-based markdown editing (`f pr open edit`) or `gh pr edit --body-file <file>`.

## Update loop for follow-up commits

```bash
# After additional changes
f commit --queue -m "<follow-up>"
f pr --no-commit --base main
f pr open edit
```

## Reusable AI context template

Paste this in requests when you want PR creation handled consistently:

```md
Use Flow + jj PR workflow in this repo:
1. Run `f sync`.
2. Ensure jj is initialized (`f jj init`) and bookmark is tracked.
3. Commit with `f commit --queue` (no direct push).
4. Create/update PR with `f pr --no-commit --base main`.
5. Open PR editor with `f pr open edit` and sync title/body.
6. Report exact commands run and the final PR URL.

Constraints:
- Keep unrelated local changes untouched.
- Do not use destructive git commands.
- Do not create duplicate commits when creating PRs.
```

## Notes

- `f pr` without `--no-commit` will stage/commit before creating the PR.
- `f commit --queue` is the safest default for a review-first loop.
- If your base branch is not `main`, always pass `--base <branch>`.
- For bookmark-heavy workflows, run `f jj sync --bookmark <bookmark-name>` to fetch/rebase/push bookmark state.
