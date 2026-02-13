---
title: PR Edit Watcher
---

# PR Edit Watcher

Flow supports editing GitHub PR title/body from local Markdown files stored in `~/.flow/pr-edit/`.

There are two modes:

1. One-shot editor + sync loop: `f pr open edit`
2. Always-on background watcher (recommended): `f server`

## Always-On Watcher (f server)

When `f server` is running, it starts a lightweight watcher that:

- Watches `~/.flow/pr-edit/` (non-recursive)
- Debounces per-file changes (about 1.25s after the last write)
- Parses title/body from the markdown
- Updates the PR via GitHub REST (PATCH issue)
- Writes status to `~/.flow/pr-edit/status.json`

Endpoints:

- `GET /pr-edit/status`
- `POST /pr-edit/rescan`

Default server URL: `http://127.0.0.1:9060`

Example:

```sh
curl -s http://127.0.0.1:9060/pr-edit/status
```

## File Format

Each file must map to a PR. The preferred mapping is YAML frontmatter:

```md
---
repo: owner/repo
pr: 123
---

# Title

My PR title

# Description

Body goes here.
```

If the frontmatter is missing, Flow may fall back to `~/.flow/pr-edit/.index.json` (managed by
`f pr open edit`).

## Title/Body Parsing

- Title: the first non-empty line under `# Title`
- Body: everything under `# Description` (verbatim)

## Using f pr open edit

`f pr open edit`:

- Finds the open PR for the current branch (fallback: queued commit PR)
- Creates `~/.flow/pr-edit/<project>-<pr>.md` if missing
- Ensures the file contains PR frontmatter
- Opens the file in Zed Preview
- Starts a foreground watcher that syncs on save (Ctrl-C to stop)

## Status JSON

`~/.flow/pr-edit/status.json` is written by the always-on watcher and can be used to build a UI.

States:

- `unknown`: file exists but no PR mapping
- `syncing`: change detected and being pushed
- `clean`: last sync succeeded, content matches last pushed digest
- `error`: last sync failed (see `last_error`)

## Auth

The watcher uses `gh auth token` once and caches the token in memory. If syncing fails with auth
errors, run:

```sh
gh auth status
gh auth login
```

## Debugging

Start the server in foreground with debug prints for the PR watcher:

```sh
FLOW_PR_EDIT_DEBUG=1 f server foreground
```

If the watcher failed to start, `GET /pr-edit/status` returns HTTP 503 with a `detail` field.

