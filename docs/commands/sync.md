# f sync

Sync git repo: pull from tracking remote, merge upstream changes, optionally push.

## Overview

Single command to bring a repository up to date. Pulls from the tracking branch, syncs upstream if configured (fork workflow), and optionally pushes. Works with both plain git and jj (jujutsu) colocated repos.

## Quick Start

```bash
# Pull latest from remote (no push by default)
f sync

# Pull and push
f sync --push

# Pull with rebase instead of merge
f sync -r

# Pull with rebase and push
f sync -r --push
```

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `--rebase` | `-r` | Use rebase instead of merge when pulling |
| `--push` | | Push to configured git remote after sync (default: off) |
| `--no-push` | | Skip pushing (legacy; already the default) |
| `--stash` | `-s` | Auto-stash uncommitted changes (default: true) |
| `--stash-commits` | | Stash local JJ commits to a bookmark before syncing (jj only) |
| `--allow-queue` | | Allow sync even when commit queue is non-empty |
| `--create-repo` | | Create origin repo on GitHub if it doesn't exist |
| `--fix` | `-f` | Auto-fix conflicts using Claude (default: true) |
| `--no-fix` | | Disable auto-fix |
| `--max-fix-attempts <N>` | | Maximum auto-fix attempts (default: 3) |
| `--allow-review-issues` | | Allow push even if P1/P2 review todos are open |
| `--compact` | | Reduce output noise |

## What Happens

### Step 1: Pre-flight checks

1. Detects if jj is available and healthy; falls back to git if not
2. Checks for unmerged files and resolves them (or prompts)
3. Handles in-progress rebase/merge
4. Stashes uncommitted changes if `--stash` is set

### Step 2: Pull from tracking branch

Pulls from the tracking remote/branch (e.g. `origin/main`). If the branch has no tracking info but the push remote has a matching branch, auto-configures tracking.

- With `--rebase`: runs `git pull --rebase`
- Without: runs `git pull --no-rebase --no-edit` (merge without opening an editor)
- Auto-resolves conflicts when `--fix` is enabled

### Step 3: Sync upstream

If an `upstream` remote exists (fork workflow), fetches and merges upstream changes into the current branch.

If no `upstream` remote but on a feature branch, syncs from `origin/<default-branch>` (e.g. `origin/main`) into the current branch.

### Step 4: Push (optional)

Only when `--push` is passed:

- Detects fork push targets (redirects to private fork remote if configured)
- Checks review-todo gate (blocks if P1/P2 issues are open unless `--allow-review-issues`)
- Skips push if the push remote equals upstream (read-only clone)
- Creates repo with `--create-repo` if origin doesn't exist

### Step 5: Restore stash

Restores auto-stashed changes if any were stashed in step 1.

## JJ (Jujutsu) Support

When a `.jj` directory is present and healthy, sync uses the jj flow instead of plain git. Falls back to git if:

- Configured `git.remote` is not `origin`/`upstream`
- Tracking remote is a custom remote
- jj workspace is unhealthy or corrupt

Use `--stash-commits` to bookmark local jj commits before syncing.

## Commit Queue Guard

If the commit queue has pending entries and sync would rebase (rewriting SHAs), sync refuses to proceed. Use `--allow-queue` to override, or process the queue first with `f commit-queue list`.

## Configuration

### Push remote

Set in `flow.toml` under `[git]`:

```toml
[git]
remote = "origin"  # default push remote
```

### Fork push

When a fork push target is configured, sync redirects push to the fork remote automatically.

## Examples

```bash
# Basic sync (pull only)
f sync

# Sync and push
f sync --push

# Rebase workflow
f sync -r --push

# Sync a fork (has upstream remote)
f sync --push

# Create missing origin and push
f sync --push --create-repo

# Skip auto-fix for conflicts
f sync --no-fix

# Allow sync with pending commit queue
f sync --allow-queue
```

## Troubleshooting

### "Unmerged files detected"

Sync found files with unresolved merge conflicts. By default (`--fix`), it tries to auto-resolve. If that fails, resolve manually:

```bash
git status                # see conflicted files
# edit and fix conflicts
git add <files>
f sync                    # retry
```

### "Commit queue is not empty"

Rebase-based sync can rewrite commit SHAs, breaking queued commits. Either:

```bash
f commit-queue list       # review the queue
f sync --allow-queue      # override the guard
```

### "Remote unreachable"

The push remote doesn't exist or auth/network failed. For missing origin:

```bash
f sync --push --create-repo
```

### jj corruption fallback

If jj sync fails due to workspace/store issues, sync automatically retries with plain git. Fix jj with:

```bash
jj git import
# or if still broken:
rm -rf .jj && jj git init --colocate
```

## See Also

- [upstream](upstream.md) - Manage upstream fork workflow
- [commit](commit.md) - Commit changes
- [jj](jj.md) - Jujutsu workflow helpers
