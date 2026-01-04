# f repos

Clone repositories into a structured local directory.

## Overview

`f repos clone` clones GitHub repositories into `~/repos/<owner>/<repo>` using SSH URLs. By default it does a shallow clone for speed, then fetches full history in the background. It always sets up an `upstream` remote and local tracking branch unless you pass `--no-upstream`.

## Quick Start

```bash
# Clone a GitHub repo into ~/repos/<owner>/<repo>
f repos clone https://github.com/owner/repo

# Short form
f repos clone owner/repo

# Skip upstream auto-setup
f repos clone owner/repo --no-upstream

# Full clone (skip background history fetch)
f repos clone owner/repo --full
```

## Options

### f repos clone

| Option | Short | Description |
|--------|-------|-------------|
| `<URL>` | | Repository URL or `owner/repo` |
| `--root <PATH>` | | Root directory for clones (default: `~/repos`) |
| `--full` | | Full clone (skip shallow clone + background history fetch) |
| `--no-upstream` | | Skip upstream setup |
| `--upstream-url <URL>` | `-u` | Upstream URL override (skips GitHub lookup) |

## Upstream Automation

Flow will:

1. Query GitHub via `gh api repos/<owner>/<repo>`
2. Detect the parent repository
3. Run `f upstream setup --url <parent>` inside the cloned repo

If the repo is not a fork (or `gh` is unavailable), flow sets `upstream` to the `origin` URL.

## Background History Fetch

When cloning in fast mode (default), flow spawns a background fetch:

- `git fetch --unshallow --tags origin`
- `git fetch --tags upstream` (if upstream was configured)

## Examples

```bash
# Clone into a custom root
f repos clone https://github.com/owner/repo --root ~/work/repos

# Override upstream manually
f repos clone https://github.com/your-user/repo -u git@github.com:upstream-org/repo.git
```
