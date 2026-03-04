# f clone

Clone a repository with git-like destination behavior.

## Overview

`f clone` behaves like `git clone` for destination paths:

- `f clone <url>` clones into the current working directory using Git's default folder naming.
- `f clone <url> <dir>` clones into an explicit destination directory.

For GitHub inputs, Flow normalizes clone URLs to SSH:

- `https://github.com/owner/repo` -> `git@github.com:owner/repo.git`
- `owner/repo` -> `git@github.com:owner/repo.git`

This command does not force clones into `~/repos` and does not auto-configure `upstream`.

## Usage

```bash
f clone <url-or-owner/repo> [directory]
```

## Examples

```bash
# GitHub URL -> SSH clone URL
f clone https://github.com/genxai/new

# owner/repo shorthand -> SSH clone URL
f clone genxai/new

# explicit destination folder (same as git clone)
f clone genxai/new my-local-new
```

## When To Use

- Use `f clone` when you want standard `git clone` destination behavior.
- Use [`f repos clone`](repos.md) when you want managed placement under `~/repos/<owner>/<repo>` plus optional upstream automation.
