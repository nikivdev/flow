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

### f docs edit

Opens a `.ai/docs` file in `$EDITOR`.

Example:

```bash
f docs edit architecture
```
