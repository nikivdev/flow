# Flow Commands Reference

Complete documentation for all `f` (flow) commands.

## Quick Reference

| Command | Description |
|---------|-------------|
| [`deploy`](deploy.md) | Deploy to Linux hosts, Cloudflare Workers, or Railway |
| [`release`](release.md) | Publish a release to registry or GitHub |
| [`publish`](publish.md) | Publish project to GitHub |
| [`install`](install.md) | Install a CLI/tool via registry, parm, or flox |
| [`repos`](repos.md) | Clone repositories into ~/repos |
| [`commit`](commit.md) | AI-powered commit with code review |
| [`upstream`](upstream.md) | Manage upstream fork workflow |
| [`env`](env.md) | Sync project environment and manage env vars |
| [`invariants`](invariants.md) | Validate project invariants from `flow.toml` |
| [`fast`](fast.md) | Low-latency AI task invocation via fast client |
| [`tasks`](tasks.md) | List and run project tasks |
| [`global`](global.md) | Run tasks from global flow config |
| [`setup`](setup.md) | Print aliases or run setup task |
| [`ai`](ai.md) | Manage AI coding sessions (Claude + Codex) |
| [`daemon`](daemon.md) | Manage background daemons |
| [`parallel`](parallel.md) | Run tasks in parallel |
| [`docs`](docs.md) | Manage auto-generated documentation |
| [`web`](web.md) | Open the Flow web UI for a project |

## Getting Started

```bash
# Show all commands
f --help

# Get help for a specific command
f deploy --help
f commit --help
```

## Command Categories

### Deployment

- **[deploy](deploy.md)** - Deploy to hosts and cloud platforms
- **[release](release.md)** - Publish releases to registries
- **[publish](publish.md)** - Publish project to GitHub

### Version Control

- **[commit](commit.md)** - AI-powered commits with review
- **[repos](repos.md)** - Clone repos into a structured directory
- **[upstream](upstream.md)** - Fork management and sync
- **[fixup](fixup.md)** - Fix common TOML syntax errors

### Task Management

- **[tasks](tasks.md)** - List project tasks
- **[fast](fast.md)** - Run AI tasks through the low-latency fast client path
- **[global](global.md)** - Run tasks from ~/.config/flow/flow.toml
- **[setup](setup.md)** - Print aliases or run setup task
- **[run](run.md)** - Run a specific task
- **[parallel](parallel.md)** - Run tasks in parallel
- **[rerun](rerun.md)** - Re-run last task
- **[search](search.md)** - Fuzzy search global commands

### Process Management

- **[ps](ps.md)** - List running flow processes
- **[kill](kill.md)** - Stop running processes
- **[logs](logs.md)** - View task logs
- **[daemon](daemon.md)** - Manage background daemons

### AI & Development

- **[ai](ai.md)** - Manage AI coding sessions
- **[agent](agent.md)** - Invoke AI subagents
- **[match](match.md)** - Match natural language to tasks
- **[sessions](sessions.md)** - Search AI sessions across projects

### Environment & Configuration

- **[env](env.md)** - Manage environment variables
- **[invariants](invariants.md)** - Validate invariant policies in `flow.toml`
- **[init](init.md)** - Scaffold a new flow.toml
- **[doctor](doctor.md)** - Verify tools and integrations

### Project Management

- **[projects](projects.md)** - List registered projects
- **[active](active.md)** - Show or set active project
- **[hub](hub.md)** - Ensure hub daemon is running

### Documentation

- **[docs](docs.md)** - Manage auto-generated documentation
- **[commits](commits.md)** - Browse commits with AI metadata

### Legacy Compatibility

- **[recipe](recipe.md)** - Legacy recipe command (hidden; prefer `tasks` + `.ai/tasks/*.mbt`)

### Other

- **[skills](skills.md)** - Manage Codex skills
- **[install](install.md)** - Install binaries via registry/parm/flox
- **[db](db.md)** - Manage databases and providers
- **[tools](tools.md)** - Manage AI tools
- **[notify](notify.md)** - Send proposal notifications
- **[server](server.md)** - Start HTTP server for logs

## Global Options

```bash
-h, --help     Print help
-V, --version  Print version
```

## Configuration

Flow uses `flow.toml` for project configuration. See [flow.toml reference](../flow-toml.md) for full documentation.

## See Also

- [Getting Started Guide](../getting-started.md)
- [flow.toml Reference](../flow-toml.md)
