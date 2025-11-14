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
- **Hub launcher** – `f hub` checks whether the background daemon is listening on `localhost:6000` and spawns it (using `~/.config/flow/flow.toml`) if missing, giving you a one-command way to ensure services are hot.
- **Deploy helper** – `f run deploy` (or `./scripts/deploy.sh`) builds the debug binary and keeps `~/bin/f` symlinked to the latest build for fast iteration.
- **Command palette** – running `f` with no arguments pipes built-ins + project tasks into `fzf`, so you can fuzzy select anything (fallback to a plain list if `fzf` isn’t installed).

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
