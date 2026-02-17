# New Branch (Flow + jj)

Use this when you want a clean feature branch quickly with Flow (which syncs with jj under the hood).

## Goal

Create a branch from latest `origin/main` (or preferred remote trunk), keep local state safe, and verify final state.

## Recommended command flow

```bash
# 1) Start from repo root
cd <repo>

# 2) Sync trunk
f sync

# 3) Try Flow-native switch first
f switch <branch-name> --remote origin

# 4) If branch does not exist locally/remotely yet, create from current HEAD
git switch -c <branch-name>

# 5) Verify
git rev-parse --abbrev-ref HEAD
git status --short
git log -1 --oneline
```

## Reusable AI context template

Paste this in requests when you want branch creation handled consistently:

```md
Use Flow-native branch creation in this repo:
1. Run `f sync`.
2. Try `f switch <branch-name> --remote origin`.
3. If Flow says branch is not found locally/remotely, run `git switch -c <branch-name>`.
4. Verify branch name, upstream/tracking, and clean working tree.
5. Report exact commands run and final branch/HEAD.

Constraints:
- Keep unrelated local changes untouched.
- Do not use destructive git commands.
- If `f sync` is blocked by commit queue, list queue and clear stale entries safely before retrying.
```

## Notes

- `f switch` preserves safety snapshots and stashes by default.
- Today, `f switch` may fail for a brand-new local-only branch name; use the documented fallback.
- If you intentionally need a different base, switch to that base first, then run `f switch <branch-name>`.
- If your default trunk is `upstream/main`, use `--remote upstream`.
