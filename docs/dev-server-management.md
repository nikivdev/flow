# Dev Server Management

Flow's supervisor manages dev server lifecycle declaratively. Define servers in `config.ts`, Flow handles starting, stopping, port cleanup, and restart on failure.

## Config Chain

```
~/config/i/lin/config.ts  (source of truth: devServers array)
         ↓  lin daemon watches, runs `bun ./config.ts`
~/.config/flow/flow.toml  (generated [[server]] entries)
         ↓  supervisor polls mtime every 2s
supervisor  (starts/stops/restarts processes)
         ↓
running processes (bash → rise dev, wrangler, etc.)
```

## Lifecycle

### On Mac Reboot

1. macOS launchd starts the Flow supervisor (if installed via `f supervisor install --boot`)
2. Supervisor reads `~/.config/flow/flow.toml`
3. Starts daemons with `autostart = true` or `boot = true`
4. Servers with `autostart = false` wait for `f daemon start <name>`

### Starting Dev Servers After Reboot

```bash
# See what's defined vs running
f daemon status

# Start a specific server
f daemon start myflow-web

# Start multiple
f daemon start myflow-web && f daemon start myflow-api
```

### Day-to-Day

```bash
f daemon status              # what's running
f daemon start myflow-web    # start
f daemon stop myflow-web     # stop
f daemon restart myflow-web  # restart
f daemon logs myflow-web     # view logs
```

## How Servers Are Defined

### Source: `~/config/i/lin/config.ts`

```typescript
const devServers = [
  {
    name: "myflow-web",
    command: "bash",
    args: ["-c", "bash ./scripts/patch-rise-root.sh && cd web && RISE_WEB_PORT=3000 VITE_API_URL=http://localhost:8780 rise dev --root .. --platform web"],
    working_dir: "~/code/myflow",
    port: 3000,
  },
  {
    name: "myflow-api",
    command: "bash",
    args: ["-c", "cd api/ts && npx wrangler dev --port 8780"],
    working_dir: "~/code/myflow",
    port: 8780,
  },
] as const
```

### Generated: `~/.config/flow/flow.toml`

The lin config watcher runs `bun ./config.ts` which generates:

```toml
[[server]]
name = "myflow-web"
command = "bash"
args = ["-c", "bash ./scripts/patch-rise-root.sh && cd web && RISE_WEB_PORT=3000 VITE_API_URL=http://localhost:8780 rise dev --root .. --platform web"]
working_dir = "/Users/nikiv/code/myflow"
port = 3000
autostart = false
```

### Conversion: `[[server]]` → Daemon

`ServerConfig::to_daemon_config()` in `src/config.rs` converts each server to a daemon with:
- `restart = "on-failure"` (auto-restart on crash)
- `retry = 3` (max 3 restart attempts)
- `boot = false`, `autostop = false`

## `[[server]]` vs `[[daemon]]`

- **`[[server]]`** — dev-time HTTP servers. Auto-get `restart = on-failure` + port eviction.
- **`[[daemon]]`** — any long-running process. Full control over restart/boot/health.

Both are managed the same way by the supervisor.

## Port Eviction

Before starting any server, Flow kills any existing process on the target port:

```
lsof -ti :3000 | xargs kill
```

This prevents "port already in use" errors after crashes or unclean shutdowns.

## autostart vs boot vs on-demand

| Field | When it starts | Use case |
|-------|---------------|----------|
| `autostart = true` | When supervisor starts | Always-on services (AI proxy, watchers) |
| `boot = true` | On system boot only | System services |
| Both false | `f daemon start <name>` | Dev servers (start when needed) |

Dev servers default to `autostart = false` because you don't always need every project running.

## Supervisor

The supervisor is the long-running process that manages all daemons.

```bash
f supervisor status           # is it running?
f supervisor start            # start it
f supervisor install --boot   # install launchd agent (survives reboot)
```

It polls `~/.config/flow/flow.toml` every 2 seconds. When the file changes:
1. Loads new config
2. Starts newly added daemons (if autostart)
3. Stops removed daemons
4. Restarts daemons whose config changed

## PID and Log Locations

| What | Where |
|------|-------|
| PID files | `~/.config/flow/{name}.pid` |
| Daemon logs | `~/.config/flow-state/daemons/{name}/stdout.log` |
| Supervisor socket | `~/.config/flow-state/supervisor.sock` |

## Troubleshooting

**Server shows "started" but port not responding:**
- Dev servers need 10-20s to compile and start (patch → rise compile → vite)
- Check with: `curl -sf http://localhost:3000 | head -5`
- Check process tree: `pgrep -P $(cat ~/.config/flow/myflow-web.pid)`

**"health check failed" warning on start:**
- Normal for dev servers — they take time to boot. Flow will check again.
- If it never comes up, check the command runs manually:
  ```bash
  cd ~/code/myflow && bash ./scripts/patch-rise-root.sh && cd web && RISE_WEB_PORT=3000 rise dev --root .. --platform web
  ```

**Server not in `f daemon status`:**
- Check flow.toml has the entry: `grep myflow ~/.config/flow/flow.toml`
- If missing, the lin daemon may be stopped: check `f daemon status` for `lin`
- Regenerate manually: `cd ~/config/i/lin && bun ./config.ts`

**Port not freed after removing a server:**
- Check if `flow.toml` was regenerated
- Check supervisor is running: `f supervisor status`
- Manual cleanup: `lsof -ti :PORT | xargs kill`

**Process restarting in a loop:**
- Servers use `on-failure` restart with max 3 retries and exponential backoff (2s, 4s, 8s, ... 60s)
- After 3 failures, supervisor gives up. Fix the issue and `f daemon restart <name>`
