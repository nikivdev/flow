# jj

Flow wraps common Jujutsu (jj) workflows so you can stay in jj while remaining fully Git-compatible.

## Quick start

```bash
# Initialize jj (colocated with git when .git exists)
f jj init

# Create a feature bookmark and track origin
f jj bookmark create feature-x --track

# Fetch + rebase onto main + push bookmark
f jj sync --bookmark feature-x
```

## Commands

- `f jj status` — Show `jj st`
- `f jj fetch` — `jj git fetch`
- `f jj rebase --dest <branch>` — Rebase onto `jj.default_branch` (or main/master)
- `f jj push --bookmark <name>` — Push a single bookmark
- `f jj push --all` — Push all bookmarks
- `f jj sync --bookmark <name>` — Fetch, rebase, then push bookmark
- `f jj workspace add <name> [--path <dir>]` — Create a workspace
- `f jj workspace list` — List workspaces
- `f jj bookmark create <name> [--rev <rev>] [--track]` — Create bookmark
- `f jj bookmark track <name> [--remote <remote>]` — Track remote bookmark

## Config

Add to `flow.toml`:

```toml
[git]
remote = "myflow-i"  # optional preferred writable remote

[jj]
default_branch = "main"
remote = "origin"     # optional legacy fallback if [git].remote is unset
auto_track = true
```

This keeps jj aligned with Git remotes while you work locally in jj.
