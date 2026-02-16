# Flow Features

Flow is a CLI tool for managing project tasks, AI coding sessions, and development workflows.

## Quick Reference

| Command | Alias | Description |
|---------|-------|-------------|
| `f <task>` | - | Run a task directly |
| `f search` | `f s` | Fuzzy search global tasks |
| `f commit` | `f c` | AI-powered git commit |
| `f commitWithCheck` | `f cc` | Commit with Codex code review |
| `f ai` | - | Manage AI sessions (Claude/Codex) |
| `f skills` | - | Manage Codex skills |
| `f daemon` | `f d` | Manage background daemons |
| `f env` | - | Manage environment variables |
| `f match` | `f m` | Natural language task matching |

---

## Task Management

### Running Tasks

```bash
# Run a task directly (most common usage)
f <task-name> [args...]

# Example: run 'dev' task with arguments
f dev --port 3000

# Fuzzy search global tasks (outside project directories)
f search
f s
```

### Task History

```bash
# Show the last task input and output
f last-cmd

# Show full details of last task run
f last-cmd-full

# Re-run the last executed task
f rerun
```

### Process Management

```bash
# List running flow processes for current project
f ps
f ps --all  # List across all projects

# Stop running processes
f kill <task-name>
f kill <pid>
f kill --all
```

### Task Logs

```bash
# View logs from running or recent tasks
f logs <task-name>
f logs -f  # Follow in real-time
```

### Task Failure Hooks

Flow can run a hook automatically when a task fails. This is useful for opening
an AI prompt, collecting diagnostics, or running cleanup scripts.

See `docs/task-failure-hooks.md` for configuration, environment variables, and
default behavior.

---

## AI Session Management

Manage Claude Code and Codex sessions with fuzzy search and session tracking.

### Listing Sessions

```bash
# List all AI sessions for current project (Claude + Codex)
f ai
f ai list

# List only Claude sessions
f ai claude
f ai claude list

# List only Codex sessions
f ai codex
f ai codex list
```

### Resuming Sessions

```bash
# Resume a session (fuzzy search)
f ai resume

# Resume a specific session by name or ID
f ai resume my-session

# Resume Claude-only sessions
f ai claude resume
```

Important resume rules:
- `f ai claude resume <explicit-id-or-name>` is strict (fails instead of opening a different session).
- `f ai codex resume ...` requires an interactive TTY.
- For full details, see `commands/ai.md`.

### Copying Session Content

```bash
# Copy full session history to clipboard (fuzzy search)
f ai copy

# Copy last exchange (prompt + response) to clipboard
f ai context

# Copy last 3 exchanges from a specific project
f ai claude context - /path/to/project 3

# Copy from a specific session
f ai context my-session /path/to/project 2
```

The `-` placeholder triggers fuzzy search for session selection.

### Saving & Managing Sessions

```bash
# Save/bookmark a session with a name
f ai save my-feature-work
f ai save bugfix --id <session-id>

# Open or create notes for a session
f ai notes my-session

# Remove a saved session from tracking
f ai remove my-session

# Initialize .ai folder structure
f ai init

# Import existing sessions for this project
f ai import
```

---

## AI-Powered Git Commits

### Standard Commit

```bash
# Stage all changes, generate AI commit message, commit, and push
f commit
f c

# Skip pushing after commit
f commit --no-push
```

### Commit with Code Review

```bash
# Run Codex code review before committing
f commitWithCheck
f cc

# Review checks for:
# - Bugs
# - Security vulnerabilities
# - Performance issues
#
# Optional config:
# [options]
# commit_with_check_async = false  # force local sync execution
# commit_with_check_use_repo_root = false  # only stage/commit from current subdir
# commit_with_check_timeout_secs = 300  # abort review if it hangs (default 300)
# commit_with_check_review_retries = 2  # retry timed-out review runs (default 2)
#
# Optional env overrides:
# FLOW_COMMIT_WITH_CHECK_TIMEOUT_SECS=600
# FLOW_COMMIT_WITH_CHECK_REVIEW_RETRIES=3
# FLOW_COMMIT_WITH_CHECK_RETRY_BACKOFF_SECS=5

# If issues found, prompts for confirmation before proceeding
```

---

## Background Daemons

Manage long-running processes defined in `flow.toml`.

```bash
# Start a daemon
f daemon start <name>

# Stop a daemon
f daemon stop <name>

# Check daemon status
f daemon status

# List available daemons
f daemon list
f daemon ls
```

Daemon config supports autostart, boot-only daemons, restart policies, and
readiness checks:

```toml
[[daemon]]
name = "lin"
binary = "lin"
command = "daemon"
args = ["--host", "127.0.0.1", "--port", "9050"]
health_url = "http://127.0.0.1:9050/health"
autostart = true
autostop = true
boot = true
restart = "on-failure"
retry = 3
ready_output = "ready"
ready_delay = 500
```

---

## Environment Variables

Manage environment variables via cloud integration.

### Authentication

```bash
# Login to cloud
f env login

# Check auth status
f env status
```

### Managing Variables

```bash
# Pull env vars to .env file
f env pull
f env pull -e staging

# Push local .env to cloud
f env push
f env push -e production

# Apply cloud envs to Cloudflare
f env apply

# Interactive setup (select env file + keys)
f env setup
f env setup -e staging -f .env.staging

# List env vars
f env list
f env ls

# Set a single variable
f env set KEY=value
f env set API_KEY=secret -e production

# Delete variable(s)
f env delete KEY1 KEY2
```

---

## Codex Skills

Manage Codex skills stored in `.ai/skills/` (gitignored by default). Skills help Codex understand project-specific workflows.

### Managing Skills

```bash
# List all skills
f skills
f skills ls

# Create a new skill
f skills new deploy-worker
f skills new deploy-worker -d "Deploy to Cloudflare Workers"

# Show skill details
f skills show deploy-worker

# Edit a skill in your editor
f skills edit deploy-worker

# Remove a skill
f skills remove deploy-worker
```

### Installing Curated Skills

```bash
# Install from Codex skill registry
f skills install linear
f skills install github-pr
```

### Syncing from flow.toml

```bash
# Generate skills from flow.toml tasks
f skills sync

# Force Codex to rescan skills for the current cwd
f skills reload
```

This creates a skill for each task in `flow.toml`, so Codex automatically knows about your project's workflows.

To auto-sync tasks or auto-install curated skills on demand, add a `[skills]` section to `flow.toml`:

```toml
[skills]
sync_tasks = true
install = ["quality-bun-feature-delivery"]

[skills.codex]
generate_openai_yaml = true
force_reload_after_sync = true
task_skill_allow_implicit_invocation = false
```

`[skills.codex]` keeps agent context tight by generating `agents/openai.yaml` for task skills and automatically refreshing Codex’s skill cache after sync/install.

For strict local quality enforcement on commit:

```toml
[commit.testing]
mode = "block"
runner = "bun"
bun_repo_strict = true
require_related_tests = true
ai_scratch_test_dir = ".ai/test"
run_ai_scratch_tests = true
allow_ai_scratch_to_satisfy_gate = false
max_local_gate_seconds = 20

[commit.skill_gate]
mode = "block"
required = ["quality-bun-feature-delivery"]

[commit.skill_gate.min_version]
quality-bun-feature-delivery = 2
```

### Fetching Dependency Skills via seq

```bash
# Fetch by dependency name
f skills fetch dep react

# Auto-discover dependencies and fetch top N per ecosystem
f skills fetch auto --top 3

# Fetch from URLs
f skills fetch url https://docs.python.org/3/library/asyncio.html --name asyncio
```

Optional defaults in `flow.toml`:

```toml
[skills.seq]
seq_repo = "~/code/seq"
out_dir = ".ai/skills"
scraper_base_url = "http://127.0.0.1:7444"
allow_direct_fallback = true
top = 3
ecosystems = "npm,pypi,cargo,swift"
```

### Skill Structure

`.ai/skills/` is generated locally and should not be committed.

```
.ai/skills/
└── deploy-worker/
    └── skill.md
```

Each `skill.md` contains:

```markdown
---
name: deploy-worker
description: Deploy to Cloudflare Workers
---

# deploy-worker

## Instructions

Run this task with `f deploy-worker`

## Examples

...
```

---

## Natural Language Task Matching

Match tasks using natural language via local LM Studio.

```bash
# Match a query to a task
f match "run the tests"
f m "start development server"

# Requires LM Studio running on localhost:1234
```

---

## Project Management

### Projects

```bash
# List registered projects
f projects

# Show or set active project
f active
f active set my-project
```

### Initialization

```bash
# Create a new flow.toml in current directory
f init

# Fix common TOML syntax errors
f fixup
```

`f init` now seeds a Codex-first baseline (`[skills]`, `[skills.codex]`, and commit skill-gate sections) so task sync + skill enforcement are enabled from day one.

### Health Check

```bash
# Verify required tools and shell integrations
f doctor
```

---

## Hub (Background Daemon)

The hub manages background task execution and log aggregation.

```bash
# Ensure hub daemon is running
f hub

# Start the HTTP server for log ingestion
f server
```

---

## flow.toml Configuration

### Basic Task Definition

```toml
[[tasks]]
name = "dev"
description = "Start development server"
command = "npm run dev"

[[tasks]]
name = "test"
description = "Run tests"
command = "cargo test"
dependencies = ["cargo"]
```

### Task with File Watching (Auto-rerun)

```toml
[[tasks]]
name = "build"
command = "cargo build"
rerun_on = ["src/**/*.rs", "Cargo.toml"]
rerun_debounce_ms = 300
```

### Daemons (Background Services)

```toml
[[daemons]]
name = "api"
command = "cargo run --bin server"
description = "API server"

[[daemons]]
name = "worker"
command = "node worker.js"
```

### Dependencies

```toml
[deps]
git = "git"
node = "node"
cargo = "cargo"
```

---

## Shell Integration

### Direnv Integration

Add to `.envrc` for automatic project daemon startup:

```sh
if command -v flow >/dev/null 2>&1; then
    flow project start --detach >/dev/null 2>&1
fi
```

### Aliases (Recommended)

```bash
alias f="flow"
```

---

## File Structure

```
~/.config/flow/
├── flow.toml          # Global config
└── config.toml        # Flow settings

~/.flow/
└── projects/          # Per-project daemon data
    └── <hash>/
        ├── pid
        └── logs/

<project>/
├── flow.toml          # Project tasks
└── .ai/
    ├── sessions/
    │   └── claude/
    │       └── index.json
    └── skills/            # Codex skills (gitignored, materialized locally)
        └── <skill-name>/
            └── skill.md
```
