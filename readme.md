# flow

> Your second OS. SDK that has it all. Streaming, OS control with agents. Sync.

Eventually. For now its going to be a rust server that starts and starts to provide functions and nice programmable SDK into it via TS library most likely.

## Current state

An Axum powered daemon + CLI sandbox for building the foundations of an always-on personal operating system. The daemon exposes HTTP endpoints that can be called from any tool, while the CLI offers fast local utilities that share the same internals.

### What it does today

- **Axum daemon** – `f daemon` boots the HTTP server with `/health`, `/screen/latest`, and `/screen/stream` endpoints (mocked frames for now).
- **Screen preview CLI** – `f screen --frames 15 --fps 4` reuses the frame generator outside of HTTP for quick tuning.
- **Project task runner** – define tasks in `flow.toml`, list them with `f tasks`, run via `f run <task>`/`f <task>`, and capture descriptions for discoverability.
- **Dependency checks** – optional `[dependencies]` entries ensure required binaries (e.g., `fast`) exist on `PATH` before a task executes.
- **Shell aliases** – declare `[[alias]]` tables and load them into your shell with `eval "$(f setup)"` so commands like `fr` or `fc` are always available.
- **Hub launcher** – `f hub` checks whether the background daemon is listening on `localhost:6000`, spawns it (using `~/.config/flow/flow.toml`) if missing, and then opens an aggregated Ratatui dashboard so you can follow logs across every managed server. `f hub --no-ui` skips the TUI, and `f hub stop` terminates the managed daemon when you’re done.
- **Watchers** – `[[watchers]]` entries automatically run commands (e.g., `~/bin/goku`) whenever their files change.
- **Secrets sync** – declare `[storage]` environments in `flow.toml`, list them with `f secrets list`, and fetch remote `.env` payloads from the hosted hub (`flow.1focus.ai` by default) via `f secrets pull <env>`.
- **Deploy helper** – `f run deploy` (or `./scripts/deploy.sh`) builds the debug binary and keeps both `~/bin/f` and `~/bin/flow` symlinked to the latest build for fast iteration.
- **Command palette** – running `f` with no arguments pipes built-ins + project tasks into `fzf`, so you can fuzzy select anything (fallback to a plain list if `fzf` isn’t installed).
- **Codanna indexing** – `f index` bootstraps `.codanna/` if needed, runs `codanna index .`, captures `codanna mcp get_index_info --json`, and stores the JSON payload under `~/.db/flow/flow.sqlite` for other automations to consume.

> Tip: run `f <command> -h` (e.g. `f hub -h`, `f servers -h`) to see flags and detailed instructions for any subcommand.

## Requirements

- Rust 1.79+ (matches stable rustup toolchain)
- [`fzf`](https://github.com/junegunn/fzf) on your `PATH` (optional, used for the `f` command palette—falls back to a plain list when missing)
- Run `f doctor` after installing to verify `direnv` is available and that your shell sources the recommended hook so Flow tasks auto-activate when you `cd` into a repo.

## Running the daemon

```bash
cargo run -- daemon --host 0.0.0.0 --port 9050 --fps 10
```

This starts:

- `GET /health` – readiness probe
- `GET /screen/latest` – returns the most recently captured frame (currently mock ASCII data)
- `GET /screen/stream` – SSE stream that pushes frames in real-time

The mock frame generator runs at the provided FPS and keeps a broadcast buffer (default 512). As real screen capture code lands, it can be swapped into the `ScreenBroadcaster`.

## CLI preview utility

Use the shared generator outside of the daemon for quick tests:

```bash
cargo run -- screen --frames 15 --fps 4
```

This prints the frames along with timestamps, which is a lightweight way to validate performance and tune buffer sizes without wiring up HTTP.

## Monitoring the hub

1. Build the production binary with `f deploy-cli-release` (this keeps `~/bin/f` and `~/bin/flow` symlinked to `target/release/f`).
2. Run `f hub`. Flow will ensure the daemon is online at `127.0.0.1:6000` (or your overridden host/port) and then open a Ratatui dashboard that lists every managed server plus a live, aggregated log stream. Use `j`/`k` (or arrow keys) to change selection, `f` to focus logs on the highlighted server, `a` to go back to the all-server view, `PgUp`/`PgDn` to scroll, `r` to force-refresh, and `q` to exit. The daemon keeps running in the background after you quit the UI.
3. If you just want to ensure the daemon is up without opening the UI—e.g., from a script—pass `f hub --no-ui`. You can always fall back to `f logs` or `curl http://127.0.0.1:6000/logs` to verify output in that mode.

## Project automation

`flow.toml` doubles as a lightweight task runner. Example:

```toml
[[tasks]]
name = "dev"
command = "bun dev"
description = "Start the web server"

[[tasks]]
name = "commit"
dependencies = ["github.com/1focus-ai/fast"]
command = "fast commitPush"
description = "Commit with AI"

[dependencies]
"github.com/1focus-ai/fast" = "fast"

[[commands]]
path = "commands-more.toml"
description = "Extra task/alias bundle"
```

The optional `[[commands]]` tables let you split `flow.toml` into multiple files (great for sharing aliases or task packs). Each entry points at another TOML file using a path relative to the parent config (or an absolute path). Those included files can declare their own `[[tasks]]`, `[[alias]]`, dependencies, watchers, etc., and everything is merged at load time.

### Auto activation when entering the repo

Need certain tasks to fire automatically whenever you `cd` into the project root? Mark them with `activate_on_cd_to_root = true` and wire `f activate` into a shell hook (Direnv, zsh `chpwd`, etc.). Flow will load `flow.toml`, ensure dependencies exist, and run each flagged task exactly like `f run` would.

```toml
[[tasks]]
name = "setup"
command = "cargo check"
activate_on_cd_to_root = true
```

Then add something like this to `.envrc` so Direnv fires `flow activate` every time you enter the repo, but lets your shell return immediately while the setup task runs in the background:

```sh
if command -v flow >/dev/null 2>&1; then
    (flow activate >/dev/null 2>&1 &)
fi
```

`f activate --config other.toml` also works for nested repos or monorepos with dedicated configs. Because the command inherits stdout/stderr, you still see logs from tasks like `cargo check` even though they were spawned asynchronously.

### Task shortcuts

You can run any task via `f run <name>` or just `f <name>` (so `f commit` works once you’re inside a project). For long names, Flow now auto-generates an abbreviation from the initials of kebab/underscore separated words—`deploy-cli-release` becomes `dcr`—as long as the shortcut is unique. You can also pin explicit shortcuts:

```toml
[[tasks]]
name = "deploy-cli-release"
shortcuts = ["dcr", "deploy-release"]
command = "FLOW_PROFILE=release ./scripts/deploy.sh"
description = "Release build + symlink"
```

After that, `f run dcr`, `f dcr`, or even `f run deploy-release` all resolve to the same task. Shortcuts are case-insensitive and don’t require you to edit `[alias]` tables.

````

### Watchers

Keep background automations in sync with your dotfiles or code whenever files change. Flow now treats watchers as a first-class primitive with two drivers:

#### Shell driver (default)

```toml
[[watchers]]
name = "karabiner"
path = "~/config/i/karabiner"
match = "karabiner.edn"
command = "~/bin/goku"
debounce_ms = 150
run_on_start = true

[watchers.env]
PATH = "/opt/homebrew/bin:${PATH}"
````

Shell watchers observe `path` recursively and execute the configured command via `/bin/sh -c`. Use `match` to filter filenames, `debounce_ms` to control how quickly successive changes retrigger, `run_on_start` when the command should fire as soon as the daemon boots, and the optional `[watchers.env]` table for per-watcher environment overrides.

#### Poltergeist driver

For native build loops and hot reload flows, Flow can now manage [Poltergeist](https://github.com/steipete/poltergeist)—the "ghost" that keeps your builds fresh. Flow spawns `poltergeist haunt` (or any other Poltergeist subcommand you choose) for each configured watcher so your projects instantly gain its universal file-watching, Watchman-powered queueing, and panel UI.

```toml
[[watchers]]
driver = "poltergeist"
name = "peekaboo"
path = "~/src/org/1f/peekaboo"

[watchers.poltergeist]
# Default is "haunt"; switch to "panel" to keep the Ink dashboard open.
mode = "haunt"
args = ["--git-mode", "ai"]

[watchers.env]
POLTERGEIST_GIT_MODE = "ai"
```

When the hub starts, Flow expands `path`, launches the configured Poltergeist binary (`poltergeist` by default), and keeps the process alive until shutdown. Set `mode = "haunt"` for the background daemon, `mode = "panel"` for the Ink status dashboard, or `mode = "status"` if you want a long-running `poltergeist status --watch` loop. Additional `args` are appended to the command so you can enable features like Claude-powered git summaries (`--git-mode ai`) or pass a custom config file. Standard Poltergeist installations from Homebrew (`brew install poltergeist`) or npm (`npm install -g @steipete/poltergeist`) work out of the box—just remember to install Watchman as required by Poltergeist—and Flow inherits all of its debounced rebuilds, priority queues, and native notifications.

### Config hot reload

The daemon now watches `~/.config/flow/flow.toml` (falling back to `config.toml`) and automatically reapplies the configuration whenever the file changes. Save the file, and Flow restarts managed servers whose definitions changed, tears down ones you removed, and reloads every watcher so long-running tasks always match what’s declared on disk—no manual restarts required.

### Log streaming

Need to inspect build output or see why a server failed? Use `f logs`:

```bash
# Dump the last 200 lines for every managed server
f logs

# Focus on a single server
f logs --server la --limit 100

# Follow live output via SSE (requires --server)
f logs --server la --follow
```

`f logs` talks to the daemon over HTTP, so it works against both local development daemons (`--host 127.0.0.1 --port 9050`) and the background hub (`--host 127.0.0.1 --port 6000`). When `--follow` is set, the CLI keeps the SSE connection alive, automatically reconnects when the daemon restarts, and colorizes stderr/stdout prefixes (pass `--no-color` if you need plain text).

## Terminal tracing

Let the daemon capture every command you run plus its output so agents can replay context. Flip the feature on in your global config (`~/.config/flow/config.toml` by default):

```toml
[options]
trace_terminal_io = true
```

With tracing enabled, flow will:

- create `~/.flow/tmux-logs` plus a helper script under `~/.config/flow/tmux-enable-tracing.sh`.
- install tmux hooks (`pane-add`, `client-session-changed`, `session-created`) so each pane runs `tmux pipe-pane -o` and appends raw TTY data into per-pane logs.
- drop Fish hooks into `~/.config/fish/conf.d/flow-trace.fish` that log command metadata (timestamp, cwd, exit status) to `~/.flow/tty-meta/`.
- expose the data via `f trace` (live stream) or `f trace --last-command` (dump the most recent command + captured stdout/stderr).

Because tmux is the transport for terminal I/O, Flow now auto-attaches interactive Fish shells to tmux whenever tracing is on: opening a new terminal jumps into the `flow` session (created on demand). Override the session name by exporting `FLOW_AUTO_TMUX_SESSION`, or skip the auto-attach logic entirely with `FLOW_SKIP_AUTO_TMUX=1`.

## Secrets sync

Flow can keep API keys and service credentials in a hosted (or self-hosted) hub and hydrate them locally on demand. Define environments in `flow.toml`:

```toml
[storage]
provider = "1focus"
env_var = "1F_KEY"           # API token pulled from your shell env

[[storage.envs]]
name = "local"
description = "Local development defaults"
variables = [
  { key = "DATABASE_URL", default = "" },
  { key = "OPENAI_API_KEY", default = "" },
  { key = "ANTHROPIC_API_KEY", default = "" },
]

[[storage.envs]]
name = "dev"
description = "Shared development cluster"
variables = [
  { key = "DATABASE_URL" },
  { key = "OPENAI_API_KEY" },
  { key = "ANTHROPIC_API_KEY" },
  { key = "S3_ACCESS_KEY" },
  { key = "S3_SECRET_KEY" },
]

[[storage.envs]]
name = "prod"
description = "Production runtime"
variables = [
  { key = "DATABASE_URL" },
  { key = "OPENAI_API_KEY" },
  { key = "ANTHROPIC_API_KEY" },
  { key = "S3_ACCESS_KEY" },
  { key = "S3_SECRET_KEY" },
  { key = "SLACK_WEBHOOK_URL" },
]
```

Usage:

```bash
# Show configured environments
f secrets list

# Pull the "dev" env from the hosted hub (flow.1focus.ai) and write .env.dev
f secrets pull dev --output .env.dev --format dotenv

# Point at a self-hosted hub
f secrets pull prod --hub https://hub.mycompany.dev
```

Set the API token via the configured `env_var` (e.g., `export 1F_KEY=...`). The hub URL defaults to `https://flow.1focus.ai`, but you can self-host by overriding `storage.hub_url` or passing `--hub` at runtime.

Run `f tasks` to list everything, `f run dev` (or simply `f dev`) to execute a task, and `f run deploy` to build + refresh the local `f` binary via `scripts/deploy.sh`. Optional `[dependencies]` entries make sure the referenced commands exist on `PATH` before the task’s shell is launched, so failures surface early.

## Remote hubs

Use `scripts/remote-hub-setup.sh <ssh-host> [config-path]` to stand up a second hub on a vanilla Linux box (great for a homelab or cloud VM reachable via Tailscale). The helper will:

- Build a release `f` binary locally and copy it plus your config to the remote host over SSH.
- Optionally sync extra folders by setting `REMOTE_SYNC_PATHS=dir1:dir2` (handy for pushing dotfiles, agent state, etc.).
- Install and start a `systemd` unit so the daemon survives reboots (`sudo systemctl status flowd` on the remote to inspect logs).

Pairing two hubs over Tailscale now takes a single command, and once both daemons are online you can push files or configs by re-running the script or using the same `REMOTE_SYNC_PATHS` env for incremental rsyncs.

## Next steps

- Replace the mock screen generator with a real capture backend and push binary payloads (e.g. raw RGBA or compressed video chunks).
- Add WebSocket and RPC endpoints for sending commands into the daemon.
- Add persistence + state management (sled/sqlite/postgres) to model “second OS” workflows over time.

## Shell helpers

Define aliases in `flow.toml` to speed up commands and load them with `f setup`:

```toml
[[alias]]
fr = "f run"    # fuzzy search through tasks
fc = "f commit" # run the "commit" task via the shorthand `f commit`
```

Apply them in a shell session via `eval "$(f setup)"`, or add the same expression to your shell rc file. After `f setup`, you can run tasks directly with `f <task>` (e.g. `f commit`) or via custom shell aliases such as `fr`/`fc`.

## Examples in the wild

- [Global config: nikiv](https://github.com/nikivdev/config/tree/main/flow)
- [Project config: linsa](https://github.com/linsa-io/linsa)

## Contributing

Any PR to improve is welcome. [codex](https://github.com/openai/codex) & [cursor](https://cursor.com) are nice for dev. Great **working** & **useful** patches are most appreciated (ideally). Issues with bugs or ideas are welcome too.

### 🖤

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
