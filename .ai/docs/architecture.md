# Flow Architecture

## Overview

Flow is a CLI tool and task runner written in Rust. It provides project automation, AI-assisted development workflows, and deployment capabilities.

## Project Structure

```
src/
├── main.rs          # CLI entry point, command routing
├── lib.rs           # Module exports
├── cli.rs           # Clap command definitions
├── config.rs        # Configuration loading (flow.toml, global config)
├── tasks.rs         # Task execution and discovery
├── parallel.rs      # Parallel task runner with TUI
├── commit.rs        # AI-powered git commits and code review
├── ai.rs            # AI session management (Claude, Codex)
├── deploy.rs        # Deployment to hosts/platforms
├── deploy_setup.rs  # Interactive deployment setup
├── docs.rs          # Auto-generated documentation management
├── daemon.rs        # Background daemon management
├── start.rs         # Project bootstrap (.ai/ folder)
├── env.rs           # Environment variable management
├── skills.rs        # Codex skill management
├── tools.rs         # AI tool management
├── agent.rs         # Kode subagent invocation
├── upstream.rs      # Upstream fork workflow
├── hub.rs           # Hub daemon management
├── processes.rs     # Process tracking and management
├── projects.rs      # Project registry
├── history.rs       # Task history
├── palette.rs       # Fuzzy finder UI
├── notify.rs        # Lin notification integration
├── commits.rs       # Git commit browser
├── fixup.rs         # TOML auto-fix
├── doctor.rs        # System diagnostics
├── init.rs          # Project scaffolding
├── discover.rs      # Config discovery
├── flox.rs          # Flox integration
├── task_match.rs    # NL task matching via LM Studio
├── lmstudio.rs      # LM Studio API client
├── log_server.rs    # HTTP log server
├── log_store.rs     # Log storage
├── watchers.rs      # File watchers
├── running.rs       # Running state tracking
├── sync.rs          # Sync utilities
└── db.rs            # Database utilities
```

## Configuration

### Project Config (`flow.toml`)

```toml
name = "project-name"

[tasks.dev]
run = "cargo run"
description = "Run development server"

[tasks.build]
run = "cargo build --release"

[daemons.api]
run = "cargo run --bin api"
port = 8080

[host]
connection = "user@host"
domain = "example.com"

[cloudflare]
name = "worker-name"

[commit]
review_instructions = "Focus on security"
```

### Global Config (`~/.config/flow/flow.toml`)

Global tasks and settings that apply across all projects.

## Key Concepts

### Tasks
Commands defined in `flow.toml` that can be run via `f <task>`. Support descriptions, dependencies, and arguments.

### Daemons
Long-running background processes managed by flow. Can be started, stopped, and monitored.

### AI Sessions
Integration with Claude Code and Codex. Sessions are tracked in `.ai/internal/sessions/` with checkpoints for context management.

### Deployment
Support for:
- **Host**: SSH-based deployment to Linux servers
- **Cloudflare**: Workers deployment
- **Railway**: Platform deployment

### Hub
Central daemon for managing servers and aggregating logs across projects.

## Data Flow

### Commit Flow
1. Stage changes (`git add`)
2. Generate diff
3. (Optional) Code review via Codex/Claude
4. Generate commit message via OpenAI
5. Commit and push
6. (Optional) Sync to gitedit.dev

### Task Execution
1. Load `flow.toml` (project or global)
2. Resolve task by name
3. Execute command with environment
4. Capture output and status
5. Store in history

### Parallel Execution
1. Parse task specifications
2. Create async task handles
3. Run with semaphore-based concurrency limit
4. Render real-time TUI with spinners
5. Collect results and display failures

## File Locations

### Per-Project
- `.ai/` - AI configuration and data
  - `actions/` - Fixer scripts
  - `skills/` - Codex skills
  - `tools/` - TypeScript tools
  - `flox/` - Flox manifest
  - `docs/` - Auto-generated docs
  - `agents.md` - Agent instructions
  - `internal/` - Private data (gitignored)
    - `sessions/` - AI sessions
    - `checkpoints/` - Checkpoints
    - `db/` - SQLite database
- `.claude/` - Symlinks to `.ai/` (gitignored)
- `.codex/` - Symlinks to `.ai/` (gitignored)
- `.flox/` - Symlinks to `.ai/flox/` (gitignored)
- `flow.toml` - Project configuration

### Global
- `~/.config/flow/flow.toml` - Global config
- `~/.config/flow/config.toml` - Flow settings
- `~/.local/share/flow/` - Data storage
  - `history.sqlite` - Task history
  - `projects.json` - Project registry

## Dependencies

Key crates:
- `clap` - CLI parsing
- `tokio` - Async runtime
- `axum` - HTTP server
- `ratatui` - TUI components
- `crossterm` - Terminal manipulation
- `reqwest` - HTTP client
- `rusqlite` - SQLite
- `serde` - Serialization
- `toml` - Config parsing
