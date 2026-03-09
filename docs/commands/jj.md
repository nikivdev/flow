# jj

Flow wraps common Jujutsu (jj) workflows so you can stay in jj while remaining fully Git-compatible.

## Quick start

```bash
# Inspect workspace + home-branch state
f status

# Initialize jj (colocated with git when .git exists)
f jj init

# Create a feature bookmark and track origin
f jj bookmark create feature-x --track

# Fetch + rebase onto main + push bookmark
f jj sync --bookmark feature-x
```

## Commands

- `f status` — Show workflow-aware JJ status (workspace, home branch, leaf branches, working-copy summary, and next-step hints)
- `f jj status` — Show raw `jj st`
- `f jj fetch` — `jj git fetch`
- `f jj rebase --dest <branch>` — Rebase onto `jj.default_branch` (or main/master)
- `f jj push --bookmark <name>` — Push a single bookmark
- `f jj push --all` — Push all bookmarks
- `f jj sync --bookmark <name>` — Fetch, rebase, then push bookmark
- `f jj workspace add <name> [--path <dir>] [--rev <rev>]` — Create a workspace (optionally anchored to a revision)
- `f jj workspace lane <name> [--path <dir>] [--base <rev>] [--remote <name>] [--no-fetch]` — Create an isolated parallel lane from trunk defaults
- `f jj workspace review <branch> [--path <dir>] [--base <rev>] [--remote <name>] [--no-fetch]` — Create or reuse a stable JJ review workspace for a branch without touching the current checkout
- `f jj workspace list` — List workspaces
- `f jj bookmark create <name> [--rev <rev>] [--track]` — Create bookmark
- `f jj bookmark track <name> [--remote <remote>]` — Track remote bookmark

## Status-first workflow

Use `f status` before branch, workspace, commit, or publish operations.

It is the fast way to answer:

- which workspace am I in?
- am I on the long-lived home branch or a branch-specific leaf?
- what review or codex branches currently sit on top of the home branch?
- is the working copy clean enough to mutate safely?

Example:

```bash
cd ~/code/org/project
f status
```

Use `f jj status` only when you want the raw Jujutsu working-copy view.

## Parallel lanes (no interleaving)

Use lanes when you want multiple active tasks in one repo without stash/pop churn:

```bash
# In your current repo, create isolated lanes anchored from trunk
f jj workspace lane fix-otp
f jj workspace lane testflight

# Work each lane independently
cd ~/.jj/workspaces/<repo>/fix-otp
jj st
```

`f jj workspace lane` does:

1. `jj git fetch` (unless `--no-fetch`)
2. chooses base as `<default_branch>@<remote>` when tracked (fallback: `<default_branch>`)
3. creates a dedicated workspace with its own `@` working-copy commit

## Review workspaces

Use a review workspace when you want an isolated JJ working copy for a review branch without
touching the current repo checkout:

```bash
f jj workspace review review/alice-feature
cd ~/.jj/workspaces/<repo>/review-alice-feature
jj st
```

`f jj workspace review` does:

1. `jj git fetch` (unless `--no-fetch`)
2. reuses the existing review workspace when present
3. otherwise anchors the workspace at the local branch commit, then the remote branch commit, then trunk
4. prints the stable workspace path plus the JJ bookmark command to publish later

Important:

- The review workspace is JJ-native. Use `jj` or `f jj` inside it.
- In colocated repos, plain `git` still points at the main checkout, so this command intentionally does not run `flow switch` for you.

## Config

Add to `flow.toml`:

```toml
[git]
remote = "myflow-i"  # optional preferred writable remote

[jj]
default_branch = "main"
home_branch = "alice" # optional long-lived personal integration branch
remote = "origin"     # optional legacy fallback if [git].remote is unset
auto_track = true
```

This keeps jj aligned with Git remotes while you work locally in jj.

## Home-branch model

If you keep a long-lived personal branch on top of trunk, set `jj.home_branch` and treat it as
your integration branch.

Then use short-lived `review/*` or `codex/*` branches on top of that home branch for task-specific
work. `f status` is optimized to make that shape visible.

Recommended flow:

```bash
# Default checkout stays on your home branch
cd ~/code/org/project
f status

# Branch-specific work happens in isolated workspaces
f jj workspace review review/alice-feature
cd ~/.jj/workspaces/project/review-alice-feature
f status
```

See also:

- [`jj-review-workspaces.md`](../jj-review-workspaces.md)
- [`jj-home-branch-workflow.md`](../jj-home-branch-workflow.md)
