# How Flow Daemon Manages macOS Services

Flow provides a declarative alternative to macOS launchd for managing background services. Instead of scattered `.plist` files, all services are defined in a single `flow.toml` configuration.

## Why Use Flow Instead of launchd?

| launchd | Flow |
|---------|------|
| Plist files scattered across ~/Library/LaunchAgents | Single `~/.config/flow/flow.toml` |
| Services auto-start on login (hidden cost) | Explicit control: autostart or on-demand |
| Hard to audit what's running | `f daemon status` shows everything |
| XML format, verbose | TOML format, readable |
| No easy way to temporarily disable | `f daemon stop <name>` |

## Quick Start

```bash
# See what's running
f daemon status

# Start a service
f daemon start glm4

# Stop a service
f daemon stop glm4

# Audit macOS launchd services
f macos audit

# Disable a launchd service and migrate to Flow
f macos disable com.example.service
# Then add [[daemon]] entry to flow.toml
```

## Migrating from launchd to Flow

### Step 1: Audit Your Current Services

```bash
# List all non-Apple launchd services
f macos list

# See what's currently running
f macos status

# Get recommendations
f macos audit
```

### Step 2: Get Service Details

```bash
# View plist contents for a service
f macos info com.example.service
```

This shows the binary, arguments, working directory, and environment variables needed to recreate the service in Flow.

### Step 3: Disable launchd Service

```bash
f macos disable com.example.service
```

This runs `launchctl bootout` and `launchctl disable` to prevent the service from starting.

### Step 4: Add to flow.toml

```toml
[[daemon]]
name = "example"
binary = "/path/to/binary"
args = ["--port", "8080"]
working_dir = "/path/to/workdir"
port = 8080
health_url = "http://127.0.0.1:8080/health"
autostart = false  # true = starts on login
boot = false       # true = starts on system boot
restart = "on-failure"  # "never", "on-failure", "always"
description = "Example service"

# Optional: environment variables
[daemon.env]
API_KEY = "secret"
```

### Step 5: Start the Service

```bash
f daemon start example
```

## Daemon Configuration Reference

### Required Fields

| Field | Description |
|-------|-------------|
| `name` | Unique identifier for the daemon |
| `binary` | Path to executable |

### Optional Fields

| Field | Default | Description |
|-------|---------|-------------|
| `command` | - | Subcommand to run (e.g., "server") |
| `args` | `[]` | Arguments passed to binary/command |
| `working_dir` | - | Working directory for the process |
| `port` | - | Port the service listens on |
| `host` | `127.0.0.1` | Host the service binds to |
| `health_url` | - | URL to check if service is healthy |
| `autostart` | `false` | Start automatically when Flow starts |
| `boot` | `false` | Start on system boot |
| `autostop` | `false` | Stop when leaving project |
| `restart` | - | Restart policy: `never`, `on-failure`, `always` |
| `retry` | - | Max restart attempts |
| `ready_delay` | - | Ms to wait before considering ready |
| `ready_output` | - | Output pattern to match for readiness |
| `description` | - | Human-readable description |
| `env` | `{}` | Environment variables |

## Example Configurations

### Local LLM Server (On-Demand)

```toml
[[daemon]]
name = "glm4"
binary = "/path/to/venv/bin/python"
args = ["-m", "mlx_lm.server", "--model", "mlx-community/Qwen2.5-7B-Instruct-4bit", "--port", "8080"]
working_dir = "/path/to/mlx-lm"
port = 8080
health_url = "http://127.0.0.1:8080/health"
autostart = false
description = "MLX local LLM server"
```

### File Watcher (Always Running)

```toml
[[daemon]]
name = "watchman"
binary = "/opt/homebrew/bin/watchman"
args = ["--foreground", "--logfile=/path/to/log"]
autostart = true
boot = true
restart = "on-failure"
description = "Facebook Watchman file watcher"
```

### Node.js Service with Environment

```toml
[[daemon]]
name = "api"
binary = "/path/to/node"
args = ["/path/to/server.js"]
working_dir = "/path/to/project"
port = 3000
health_url = "http://127.0.0.1:3000/health"
autostart = false
restart = "on-failure"
retry = 3
description = "API server"

[daemon.env]
NODE_ENV = "production"
DATABASE_URL = "postgres://localhost/db"
```

## macOS Service Audit Configuration

Configure which services are allowed or should be blocked in your `flow.toml`:

```toml
[macos]
# Services matching these patterns won't be flagged
allowed = [
  "com.nikiv.*",
  "com.github.facebook.watchman",
  "limit.maxfiles",
]

# Services matching these patterns will be recommended for removal
blocked = [
  "com.google.*",      # Google updaters
  "com.adobe.*",       # Adobe background services
  "us.zoom.*",         # Zoom daemon
  "com.microsoft.update.*",
  "com.dropbox.*",
  "com.spotify.webhelper",
]
```

## Commands Reference

### Daemon Management

```bash
f daemon status              # Show all daemon status
f daemon start <name>        # Start a daemon
f daemon stop <name>         # Stop a daemon
f daemon restart <name>      # Restart a daemon
f daemon logs <name>         # View daemon logs
```

### macOS Service Audit

```bash
f macos list [--user] [--system] [--json]   # List launchd services
f macos status                               # Show running non-Apple services
f macos audit [--json]                       # Audit with recommendations
f macos info <service>                       # Show service details
f macos disable <service> [-y]               # Disable a service
f macos enable <service>                     # Re-enable a service
f macos clean [--dry-run] [-y]               # Disable known bloatware
```

## Best Practices

1. **Start with audit**: Run `f macos audit` to see what's running unnecessarily
2. **Disable bloatware first**: Run `f macos clean` to disable known bloatware
3. **Migrate essential services**: Add services you need to `flow.toml`
4. **Use autostart sparingly**: Only set `autostart = true` for truly essential services
5. **Set health URLs**: Enables Flow to verify services are actually running
6. **Use restart policies**: `restart = "on-failure"` for production services

## Troubleshooting

### Service Won't Start

```bash
# Check if binary exists
ls -la /path/to/binary

# Try running manually
/path/to/binary --args

# Check logs
f daemon logs <name>
```

### launchd Service Still Running

```bash
# Force disable
f macos disable <service> -y

# Verify
launchctl list | grep <service>

# Manual removal if needed
launchctl bootout gui/$(id -u)/<service>
launchctl disable gui/$(id -u)/<service>
```

### Finding the Right Binary Path

```bash
# Get details from plist
f macos info <service>

# Or manually
plutil -convert json -o - ~/Library/LaunchAgents/<service>.plist | jq
```
