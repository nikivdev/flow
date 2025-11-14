# flow

> Your second OS. SDK that has it all. Streaming, OS control with agents. Sync.

Eventually. For now its going to be a rust server that starts and starts to provide functions and nice programmable SDK into it via TS library most likely.

## Generated below

An Axum powered daemon + CLI sandbox for building the foundations of an always-on personal operating system. The daemon exposes HTTP endpoints that can be called from any tool, while the CLI offers fast local utilities that share the same internals.

## Requirements

- Rust 1.79+ (matches stable rustup toolchain)

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

There are [streams of development of this](https://www.youtube.com/@nikivdev/streams).

### 🖤

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
