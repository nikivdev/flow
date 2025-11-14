# flow

> Your second OS. SDK that has it all. Streaming, OS control with agents. Sync.

Eventually. For now its going to be a rust server that starts and starts to provide functions and nice programmable SDK into it via TS library most likely.

Mostly writing this tool in background as I want it to automate my entire workflow. I [stream dev of it](https://youtube.com/@nikivdev/streams) too.

## Current state

An Axum powered daemon + CLI sandbox for building the foundations of an always-on personal operating system. The daemon exposes HTTP endpoints that can be called from any tool, while the CLI offers fast local utilities that share the same internals.

### What it does today

- **Axum daemon** – `f daemon` boots the HTTP server with `/health`, `/screen/latest`, and `/screen/stream` endpoints (mocked frames for now).
- **Screen preview CLI** – `f screen --frames 15 --fps 4` reuses the frame generator outside of HTTP for quick tuning.
- **Project task runner** – define tasks in `flow.toml`, list them with `f tasks`, run via `f run <task>`/`f <task>`, and capture descriptions for discoverability.
- **Dependency checks** – optional `[dependencies]` entries ensure required binaries (e.g., `fast`) exist on `PATH` before a task executes.
- **Shell aliases** – declare `[[alias]]` tables and load them into your shell with `eval "$(f setup)"` so commands like `fr` or `fc` are always available.
- **Hub launcher** – `f hub` checks whether the background daemon is listening on `localhost:6000` and spawns it (using `~/.config/flow/flow.toml`) if missing; `f hub stop` terminates the managed daemon when you’re done.
- **Watchers** – `[[watchers]]` entries automatically run commands (e.g., `~/bin/goku`) whenever their files change.
- **Secrets sync** – declare `[storage]` environments in `flow.toml`, list them with `f secrets list`, and fetch remote `.env` payloads from the hosted hub (`flow.1focus.ai` by default) via `f secrets pull <env>`.
- **Deploy helper** – `f run deploy` (or `./scripts/deploy.sh`) builds the debug binary and keeps `~/bin/f` symlinked to the latest build for fast iteration.
- **Command palette** – running `f` with no arguments pipes built-ins + project tasks into `fzf`, so you can fuzzy select anything (fallback to a plain list if `fzf` isn’t installed).
- **Codanna indexing** – `f index` bootstraps `.codanna/` if needed, runs `codanna index .`, captures `codanna mcp get_index_info --json`, and stores the JSON payload under `~/.db/flow/flow.sqlite` for other automations to consume.

> Tip: run `f <command> -h` (e.g. `f hub -h`, `f servers -h`) to see flags and detailed instructions for any subcommand.

## Requirements

- Rust 1.79+ (matches stable rustup toolchain)
- [`fzf`](https://github.com/junegunn/fzf) on your `PATH` (optional, used for the `f` command palette—falls back to a plain list when missing)

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
```

### Watchers

Keep background automations in sync with your dotfiles or code whenever files change:

```toml
[[watchers]]
name = "karabiner"
path = "~/config/i/karabiner"
match = "karabiner.edn"
command = "~/bin/goku"
debounce_ms = 150
run_on_start = true

[[watchers]]
name = "design-system"
path = "~/src/org/1f/1f"
command = "blade --port 4000"
```

Every watcher observes its `path` (recursively) and executes the command via `/bin/sh -c`. Use `match` to filter on file names, `debounce_ms` to control how quickly successive changes trigger, and `run_on_start` when you want the command to fire as soon as the hub daemon boots.

## Secrets sync

Flow can keep API keys and service credentials in a hosted (or self-hosted) hub and hydrate them locally on demand. Define environments in `flow.toml`:

```toml
[storage]
provider = "1focus"
env_var = "1F_KEY"           # API token pulled from your shell env

[[storage.envs]]
name = "local"
description = "Local development defaults"
variables = ["DATABASE_URL", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"]

[[storage.envs]]
name = "dev"
description = "Shared development cluster"
variables = [
  "DATABASE_URL",
  "OPENAI_API_KEY",
  "ANTHROPIC_API_KEY",
  "S3_ACCESS_KEY",
  "S3_SECRET_KEY",
]

[[storage.envs]]
name = "prod"
description = "Production runtime"
variables = [
  "DATABASE_URL",
  "OPENAI_API_KEY",
  "ANTHROPIC_API_KEY",
  "S3_ACCESS_KEY",
  "S3_SECRET_KEY",
  "SLACK_WEBHOOK_URL",
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

## Contributing

Any PR to improve is welcome. [codex](https://github.com/openai/codex) & [cursor](https://cursor.com) are nice for dev. Great **working** & **useful** patches are most appreciated (ideally). Issues with bugs or ideas are welcome too.

### 🖤

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
