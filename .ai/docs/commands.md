# Flow CLI Commands

Auto-generated documentation for flow CLI commands.

## Task Execution

### `f` (no args)
Opens fuzzy finder to browse and run project tasks from `flow.toml`.

### `f <task>`
Run a task directly by name. Additional arguments are passed to the task command.

### `f run <task>`
Explicit task execution (same as `f <task>`).

### `f tasks`
List all tasks from the current project's `flow.toml` with descriptions.

### `f rerun`
Re-run the last executed task in this project.

### `f last-cmd`
Show the last task's input and output/error.

### `f last-cmd-full`
Show full details of the last task run (command, status, output).

### `f search` (alias: `f s`)
Fuzzy search global commands/tasks from `~/.config/flow/flow.toml`. Useful outside project directories.

### `f match <query>` (alias: `f m`)
Match natural language query to a task using LM Studio. Requires LM Studio running on localhost:1234.

Options:
- `--model <name>` - LM Studio model (default: qwen3-8b)
- `--port <port>` - LM Studio port (default: 1234)
- `-n, --dry-run` - Show match without running

## Parallel Execution

### `f parallel <tasks...>` (alias: `f p`)
Run multiple tasks in parallel with pretty status display.

```bash
# Auto-labeled (uses first word as label)
f parallel 'echo hello' 'cargo build' 'cargo test'

# Custom labels with label:command syntax
f parallel 'build:cargo build' 'test:cargo test' 'lint:cargo clippy'
```

Options:
- `-j, --jobs <n>` - Max concurrent jobs (default: CPU count)
- `-f, --fail-fast` - Stop all tasks on first failure

Features:
- Animated spinners with color cycling
- Real-time status (pending/running/success/failure)
- Shows last output line during execution
- Timing for completed tasks
- Full output for failed tasks

## Git & Commits

### `f commit` (alias: `f c`)
AI-powered git commit: stages all changes, generates commit message from diff, commits, and pushes.

Options:
- `-n, --no-push` - Skip pushing after commit
- `--sync` - Run synchronously (don't delegate to hub)

### `f commit-with-check` (alias: `f cc`, `f commitWithCheck`)
Like `commit` but first runs code review for bugs and performance issues.

Options:
- `-n, --no-push` - Skip pushing
- `--sync` - Run synchronously
- `--no-context` - Skip AI session context in review
- `--dry` - Show context without committing
- `--claude` - Use Claude instead of Codex for review
- `--review-model <model>` - Choose model (claude-opus, codex-high, codex-mini)
- `-m, --message <msg>` - Custom message to include
- `-t, --tokens <n>` - Max tokens for context (default: 4000)

### `f commit-with-check-with-gitedit` (alias: `f ccg`)
Like `commit-with-check` but also syncs to gitedit.dev for browsing alongside GitHub history.

### `f commits`
Browse git commits with AI session metadata using fuzzy search.

Options:
- `-n, --limit <n>` - Number of commits (default: 100)
- `--all` - Show all branches

### `f fixup`
Fix common TOML syntax errors in `flow.toml`.

Options:
- `-n, --dry-run` - Show fixes without applying

## Process Management

### `f ps`
List running flow processes for current project.

Options:
- `--all` - Show processes across all projects

### `f kill [task]`
Stop running flow processes.

Options:
- `--pid <pid>` - Kill by PID
- `--all` - Kill all project processes
- `-f, --force` - Force kill (SIGKILL)
- `--timeout <secs>` - SIGKILL timeout (default: 5)

### `f logs [task]`
View logs from running or recent tasks.

Options:
- `-f, --follow` - Follow in real-time
- `-n, --lines <n>` - Lines to show (default: 50)
- `--all` - All projects
- `-l, --list` - List available logs
- `-p, --project <name>` - By project name
- `-q, --quiet` - Suppress headers

## Daemons

### `f daemon` (alias: `f d`)
Manage background daemons defined in `flow.toml`.

Subcommands:
- `start <name>` - Start a daemon
- `stop <name>` - Stop a daemon
- `restart <name>` - Restart a daemon
- `status` - Show all daemon status
- `list` (alias: `ls`) - List available daemons

## AI Sessions

### `f ai`
Manage AI coding sessions (Claude Code, Codex).

Subcommands:
- `list` (alias: `ls`) - List all sessions
- `claude [action]` - Claude sessions only
- `codex [action]` - Codex sessions only
- `resume [session]` - Resume a session
- `save <name>` - Bookmark current session
- `notes <session>` - Open/create session notes
- `remove <session>` - Remove from tracking
- `init` - Initialize .ai folder
- `import` - Import existing sessions
- `copy [session]` - Copy history to clipboard
- `context [session]` - Copy last exchange for context passing

### `f sessions` (alias: `f ss`)
Fuzzy search AI sessions across all projects, copy context to clipboard.

Options:
- `-p, --provider <name>` - Filter by provider (claude, codex, all)
- `-c, --count <n>` - Number of exchanges to copy
- `-l, --list` - Show without copying
- `-f, --full` - Full context, ignore checkpoints
- `--summarize` - Generate summaries for stale sessions

### `f agent` (alias: `f a`)
Invoke kode AI subagents.

Subcommands:
- `list` (alias: `ls`) - List available agents
- `run <agent> <prompt>` - Run agent (codify, explore, general)

## Project Setup

### `f init`
Scaffold a new `flow.toml` in current directory.

Options:
- `--path <path>` - Output path

### `f start`
Bootstrap project with `.ai/` folder structure:
- `.ai/actions/` - Fixer scripts (tracked)
- `.ai/skills/` - Shared skills (tracked)
- `.ai/tools/` - Shared tools (tracked)
- `.ai/flox/` - Flox manifest (tracked)
- `.ai/docs/` - AI-generated docs (tracked)
- `.ai/agents.md` - Agent instructions (tracked)
- `.ai/internal/` - Private data (gitignored)

Also materializes `.claude/`, `.codex/`, `.flox/` with symlinks.

### `f doctor`
Verify required tools and shell integrations (flox, lin, direnv).

### `f projects`
List all registered projects (those with `name` in `flow.toml`).

### `f active [project]`
Show or set the active project (fallback for commands outside project dirs).

Options:
- `-c, --clear` - Clear active project

## Environment

### `f env`
Sync project environment and manage env vars via 1focus.

Subcommands:
- `login` - Authenticate with 1focus
- `pull` - Fetch env vars to .env
- `push` - Push .env to 1focus
- `list` (alias: `ls`) - List env vars
- `set <KEY=VALUE>` - Set a var
- `delete <keys...>` - Delete vars
- `status` - Show auth status

Options (for pull/push/list/set/delete):
- `-e, --environment <env>` - Environment (dev, staging, production)

## Deployment

### `f deploy`
Deploy to various platforms.

Subcommands:
- `host` (alias: `h`) - Deploy to Linux host via SSH
  - `--remote-build` - Build on remote instead of syncing artifacts
  - `--setup` - Run setup script even if deployed
- `cloudflare` (alias: `cf`) - Deploy to Cloudflare Workers
  - `--secrets` - Also set secrets from env_file
  - `--dev` - Run in dev mode
- `setup` - Interactive deploy setup
- `railway` - Deploy to Railway
- `status` - Show deployment status
- `logs` - View deployment logs
  - `-f, --follow` - Follow logs
  - `-n, --lines <n>` - Lines to show
- `restart` - Restart deployed service
- `stop` - Stop deployed service
- `shell` - SSH into host
- `set-host <connection>` - Configure host (user@host:port)
- `show-host` - Show host configuration
- `health` - Check deployment health
  - `--url <url>` - Custom URL
  - `--status <code>` - Expected status (default: 200)

## Upstream Forks

### `f upstream` (alias: `f up`)
Manage upstream fork workflow.

Subcommands:
- `status` - Show upstream configuration
- `setup` - Set up upstream remote and tracking
  - `-u, --upstream-url <url>` - Upstream repo URL
  - `-b, --upstream-branch <branch>` - Branch name (default: main)
- `pull` - Pull from upstream into local 'upstream' branch
  - `-b, --branch <branch>` - Also merge into this branch
- `sync` - Full sync: pull upstream, merge, push to origin
  - `--no-push` - Skip pushing

## Skills & Tools

### `f skills`
Manage Codex skills in `.ai/skills/`.

Subcommands:
- `list` (alias: `ls`) - List skills
- `new <name>` - Create skill
  - `-d, --description <desc>` - Description
- `show <name>` - Show skill details
- `edit <name>` - Edit in editor
- `remove <name>` - Remove skill
- `install <name>` - Install from registry
- `search [query]` - Search registry
- `sync` - Sync flow.toml tasks as skills

### `f tools` (alias: `f t`)
Manage AI tools in `.ai/tools/*.ts`.

Subcommands:
- `list` (alias: `ls`) - List tools
- `run <name> [args...]` - Run a tool
- `new <name>` - Create tool
  - `-d, --description <desc>` - Description
  - `--ai` - Use AI to generate implementation
- `edit <name>` - Edit in editor
- `remove <name>` - Remove tool

## Hub & Server

### `f hub`
Ensure hub daemon is running, launch TUI for inspection.

Subcommands:
- `start` - Start hub daemon
- `stop` - Stop hub daemon

Options:
- `--host <ip>` - Hub host (default: 127.0.0.1)
- `--port <port>` - Hub port (default: 9050)
- `--config <path>` - Config path
- `--no-ui` - Skip TUI

### `f server`
Start HTTP server for log ingestion and queries.

Options:
- `--host <host>` - Bind host (default: 127.0.0.1)
- `--port <port>` - Port (default: 9060)

Subcommands:
- `foreground` - Run in foreground
- `stop` - Stop background server

## Notifications

### `f notify <action>`
Send proposal notification to Lin for approval (human-in-the-loop).

Options:
- `-t, --title <title>` - Proposal title
- `-c, --context <ctx>` - Context/description
- `-e, --expires <secs>` - Expiration (default: 300)
