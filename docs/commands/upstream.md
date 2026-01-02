# f upstream

Manage upstream fork workflow.

## Overview

Set up and sync forks with their upstream repositories. Creates a local `upstream` branch to cleanly track the original repo, making merges easier.

## Quick Start

```bash
# Set up upstream tracking
f upstream setup --upstream-url https://github.com/original/repo

# Pull latest from upstream
f upstream pull

# Full sync: pull, merge, push
f upstream sync
```

## Subcommands

| Command | Description |
|---------|-------------|
| `status` | Show current upstream configuration |
| `setup` | Set up upstream remote and local tracking branch |
| `pull` | Pull changes from upstream into local 'upstream' branch |
| `sync` | Full sync: pull upstream, merge to dev/main, push to origin |

---

## Setup

Configure upstream tracking for a forked repository:

```bash
# Basic setup
f upstream setup --upstream-url https://github.com/original/repo

# Specify branch (if not main)
f upstream setup --upstream-url https://github.com/original/repo --upstream-branch master
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--upstream-url <URL>` | `-u` | URL of the upstream repository |
| `--upstream-branch <BRANCH>` | `-b` | Branch name on upstream (default: auto-detected) |

### What Happens

1. Adds `upstream` remote pointing to original repo
2. Fetches upstream branches
3. Creates local `upstream` branch tracking the upstream's default branch
4. Stores configuration in `.git/config`

---

## Pull

Pull latest changes from upstream into local `upstream` branch:

```bash
# Pull into upstream branch
f upstream pull

# Pull and also merge into specific branch
f upstream pull --branch main
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--branch <BRANCH>` | `-b` | Also merge into this branch after pulling |

---

## Sync

Full sync workflow - pulls upstream, merges to your branch, and pushes:

```bash
# Full sync (pull, merge, push)
f upstream sync

# Sync without pushing (for review first)
f upstream sync --no-push
```

### Options

| Option | Description |
|--------|-------------|
| `--no-push` | Skip pushing to origin |

### What Happens

1. Stashes any uncommitted changes
2. Fetches latest from upstream
3. Updates local `upstream` branch
4. Merges upstream into your current branch (e.g., `main`)
5. Pushes to origin (unless `--no-push`)
6. Restores stashed changes

### Branch Detection

Flow auto-detects the upstream default branch:
- Checks `refs/remotes/upstream/HEAD`
- Falls back to checking if `upstream/main` or `upstream/master` exists
- Uses `main` as final fallback

---

## Status

Show current upstream configuration:

```bash
f upstream status
```

Output:
```
Upstream Configuration
  Remote: https://github.com/original/repo
  Branch: main
  Local tracking: upstream -> upstream/main
  Last sync: 2 hours ago
```

---

## Workflow Example

### Initial Fork Setup

```bash
# 1. Clone your fork
git clone https://github.com/youruser/project
cd project

# 2. Set up upstream tracking
f upstream setup --upstream-url https://github.com/original/project

# 3. Verify
f upstream status
```

### Regular Sync

```bash
# When you want to sync with upstream:
f upstream sync

# Or if you want to review before pushing:
f upstream sync --no-push
git log --oneline main..upstream  # See what's new
git push  # Push when ready
```

### Handling Conflicts

If sync encounters merge conflicts:

```bash
$ f upstream sync
Merging upstream into main...
CONFLICT (content): Merge conflict in src/lib.rs

# Fix conflicts manually
vim src/lib.rs
git add src/lib.rs
git commit

# Then push
git push
```

---

## Configuration

Upstream configuration is stored in `.git/config`:

```ini
[remote "upstream"]
    url = https://github.com/original/repo
    fetch = +refs/heads/*:refs/remotes/upstream/*

[branch "upstream"]
    remote = upstream
    merge = refs/heads/main
```

You can also manually configure:

```bash
git remote add upstream https://github.com/original/repo
git fetch upstream
git branch upstream upstream/main
```

---

## Troubleshooting

### "upstream remote not found"

Run `f upstream setup` first with the upstream URL.

### "git stash pop failed"

This can happen if there were no changes to stash. Flow handles this automatically by tracking stash state.

### Upstream uses master instead of main

Flow auto-detects the default branch. If detection fails, specify explicitly:

```bash
f upstream setup --upstream-url https://github.com/original/repo --upstream-branch master
```

### Merge conflicts

Resolve conflicts manually:
1. Fix conflicting files
2. `git add <files>`
3. `git commit`
4. `git push`

## See Also

- [commit](commit.md) - Commit changes after sync
- [publish](publish.md) - Publish to GitHub
