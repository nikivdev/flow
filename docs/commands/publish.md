# f publish

Publish projects to GitHub.

## Overview

Creates a new GitHub repository and pushes the current project. Automatically infers the repo name from the folder name and handles both new repos and existing ones.

## Quick Start

```bash
# Interactive mode - prompts for name and visibility
f publish

# Skip prompts, use folder name, default to private
f publish -y

# Create public repo
f publish --public

# Create with specific name
f publish --name my-awesome-project
```

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `--name <NAME>` | `-n` | Repository name (defaults to folder name) |
| `--public` | | Create as public repository |
| `--private` | | Create as private repository |
| `--description <DESC>` | `-d` | Repository description |
| `--yes` | `-y` | Skip prompts, use defaults (private, folder name) |

## Prerequisites

- [GitHub CLI](https://cli.github.com/) installed (`gh`)
- Authenticated with `gh auth login`

## Usage

### Interactive Mode

Without flags, prompts for configuration:

```bash
$ f publish

Repository name [my-project]:
Visibility (public/private) [private]: public

Create repository:
  Name: username/my-project
  Visibility: public

Proceed? [Y/n]:
```

### Non-Interactive Mode

```bash
# Use all defaults (private, folder name as repo name)
f publish -y

# Public repo with defaults
f publish -y --public

# Specific name and description
f publish -n cool-tool -d "A cool CLI tool" --public
```

### Existing Repositories

If the repository already exists on GitHub:

1. Checks current visibility
2. Updates visibility if different from requested
3. Adds origin remote if missing
4. Pushes current branch

```bash
$ f publish --public
Repository username/my-repo already exists (private).
Updating visibility to public...
✓ Updated to public

✓ https://github.com/username/my-repo
```

## What Happens

1. **Check gh CLI** - Verifies GitHub CLI is installed
2. **Check authentication** - Ensures you're logged in to GitHub
3. **Get username** - Fetches your GitHub username
4. **Determine repo name** - Uses `--name`, folder name, or prompts
5. **Determine visibility** - Uses `--public`/`--private` or prompts
6. **Check if exists** - If repo exists, updates visibility if needed
7. **Initialize git** - If not a git repo, runs `git init`
8. **Create initial commit** - If no commits, stages all and commits
9. **Create repository** - Uses `gh repo create` with `--source=. --push`
10. **Output URL** - Prints the GitHub URL

## Examples

### Quick Publish New Project

```bash
cd my-new-project
f publish -y --public
# ✓ Published to https://github.com/username/my-new-project
```

### Publish with Description

```bash
f publish -n api-server -d "REST API for my app" --private
```

### Update Existing Repo Visibility

```bash
# Repo exists as private, make it public
f publish --public
# Repository username/my-repo already exists (private).
# Updating visibility to public...
# ✓ Updated to public
```

### In Scripts/CI

```bash
#!/bin/bash
cd /path/to/project
f publish -y --public -n release-candidate
```

## Troubleshooting

### "GitHub CLI (gh) is not installed"

Install from https://cli.github.com:

```bash
# macOS
brew install gh

# Linux
sudo apt install gh  # or equivalent
```

### "GitHub authentication required"

Run `gh auth login` and follow the prompts.

### "Could not determine GitHub username"

Ensure you're authenticated: `gh auth status`

### Repository exists but can't update visibility

Some visibility changes may require specific permissions or GitHub plan features.

## See Also

- [deploy](deploy.md) - Deploy after publishing
- [upstream](upstream.md) - Managing forks
