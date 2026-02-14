# Dev Server Management

Flow's supervisor manages dev server lifecycle automatically. Edit `devServers` in your `config.ts` and Flow handles starting, stopping, and port cleanup.

## Config Chain

```
config.ts → (watcher) → flow.toml regenerated → (supervisor poll) → processes start/stop
```

1. You edit `devServers` in `config.ts`
2. The config watcher detects the change and regenerates `flow.toml`
3. The supervisor detects the new `flow.toml` mtime and reconciles

Latency: ~750ms (500ms watcher debounce + 250ms supervisor poll).

## `[[server]]` vs `[[daemon]]`

- **`[[server]]`** entries in `flow.toml` represent dev-time HTTP servers (Next.js, Vite, etc.)
- **`[[daemon]]`** entries represent long-running background services

Internally, `[[server]]` entries are converted to daemon configs via `ServerConfig::to_daemon_config()`. The supervisor manages both uniformly.

## Port Eviction

Before starting any daemon or server, Flow kills any foreign process occupying the target port using `lsof` (macOS/Linux) or `netstat` (Windows). This ensures a clean start even if another tool left a process behind.

Port is determined from:
1. Explicit `port` field on the config
2. Port extracted from the `health_url`

## Adding Servers

Add an entry to `devServers` in `config.ts`. The watcher regenerates `flow.toml`, the supervisor picks it up, evicts any port squatter, and starts the server.

## Removing Servers

Remove the entry from `devServers` in `config.ts`. The supervisor's reconciliation loop detects that the managed process is no longer in config, stops it, and frees the port.

## Troubleshooting

**Port not freed after removing a server:**
- Check if `flow.toml` was regenerated: `cat flow.toml | grep server-name`
- Check supervisor is running: `f supervisor status`
- Manually kill the process: `lsof -ti :PORT | xargs kill`

**Server not starting:**
- Check `f daemon status` for error messages
- Verify the command/binary exists and is on PATH
- Check the working directory is correct

**Process restarting in a loop:**
- Servers use `on-failure` restart policy with max 3 retries
- Check logs for crash reasons
