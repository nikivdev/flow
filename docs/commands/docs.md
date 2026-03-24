# f docs

Manage documentation for a project. There are two doc systems:

- `.ai/docs` for AI-maintained internal docs.
- `docs/` for human-facing docs rendered by the docs hub.

## Quick Start

```bash
# Create docs/ with starter files
f docs new

# Open docs for the current project (auto-starts the hub)
f docs

# Deploy the current project's docs to Cloudflare Pages
f docs deploy
```

## Commands

### f docs new

Creates a `docs/` folder in the current project using the template in `~/new/docs`.
If `docs/` already exists, it merges in any missing template files and leaves
existing content untouched.

Options:

- `--path <PATH>`: Target directory (defaults to current folder).
- `--force`: Overwrite if `docs/` already exists.

### f docs hub

Runs a single dev server that aggregates docs from `~/code` and `~/org`.
Use `FLOW_DOCS_FOCUS=1` to only index the current project for faster startup.

Options:

- `--host <HOST>` (default: `127.0.0.1`)
- `--port <PORT>` (default: `4410`)
- `--hub-root <PATH>` (default: `~/.config/flow/docs-hub`)
- `--template-root <PATH>` (default: `~/new/docs`)
- `--code-root <PATH>` (default: `~/code`)
- `--org-root <PATH>` (default: `~/org`)
- `--no-ai`: Skip `.ai/docs`.
- `--no-open`: Do not open the browser.
- `--sync-only`: Sync docs content and exit.

### f docs deploy

Deploys the current project's docs to Cloudflare Pages (uses the docs hub template).

Options:

- `--project <NAME>`: Pages project name (defaults to the flow.toml name).
- `--domain <DOMAIN>`: Attach a custom domain (optional).
- `--yes`: Skip confirmation prompts.

### f docs sync

Syncs `.ai/docs` metadata based on recent commits. Intended for AI doc upkeep.

### f docs list

Lists `.ai/docs` files for the current project.

### f docs status

Shows recent commits and `.ai/docs` file modification times.

It now also shows:

- recent `.ai/docs/session-changes/...` packets generated from completed Flow-managed Codex sessions
- doc-review queue state counts for the current project

### f docs review-pending

Runs the bounded daemon-style review pass over pending session-doc packets and records
promotion decisions into queue state plus each packet's `promotion.json`.

Options:

- `-n, --limit <N>`: Maximum number of pending entries to review (default: `25`).

### f docs promote-session

Previews or applies promotion for a daemon-captured session-doc packet.

By default this is a dry run. It prints the chosen target under `.ai/docs/` and the
promoted markdown note.

Options:

- `<SESSION>`: Session id, session id prefix, or session key like `019d035d-...` or `019d035d-1773776290`
- `--apply`: Write the promoted note immediately

### f docs commit-pending

Commits promoted session-doc notes when the git diff is isolated to doc-only files.

This command refuses to commit if the worktree contains unrelated changes.

Options:

- `--dry-run`: Show the candidate files and commit message without staging or committing

### f docs edit

Opens a `.ai/docs` file in `$EDITOR`.

Example:

```bash
f docs edit architecture
```
