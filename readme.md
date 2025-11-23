# flow

> Your second OS. SDK that has it all. Streaming, OS control with agents. Declarative. Synced.

The goal of this CLI is to first parse out `flow.toml` files like the one in [this repo](flow.toml).

Install this CLI, for now have to manually compile this as we don't have setup release builds yet.

But once you have `f` (also available as `flow`) CLI. Create `flow.toml` in some project with tasks like:

```
[[tasks]]
name = "deploy-cli-local"
command = "./scripts/deploy.sh"
description = "Build the CLI/daemon and copy the binary to ~/.local/bin/flow"
```

And if you run `f` in the project it will fuzzy search through all tasks you can run. You can also run tasks with `f <task>`.

## Hub

There is component in flow that is a hub. It's a daemon that does things. Flow is not responsible for what the daemon does, all it does is it makes sure this daemon runs and in future perhaps auto heals or restarts as the idea is that the daemon should always be running.

There is an implementation of such hub built in private called `lin`. Will be possible to use soon, for now it's being tested in private as there are bugs. The goal of lin is to declaratively specify servers to run and trace all terminal I/O. In future more.

There are also plans for flow to handle communication between hubs. But flow will always try to abstract away the job of the actual hub to the hub itself as the hub can do many things. Right now it is assumed there is only 1 hub but in future there could be multiple hubs in theory.

I like to think of flow as a program that is first top in class project manager with AI deeply embedded. But also as a small kubernetes like orchestrator of servers that run on the OS. Perhaps it will also handle the job of ingesting and streaming data from these hubs. i.e. in theory it can protect the user host from external potentially malicious hubs by making sure the hub has limited rights to do things.

## Current state

Lightweight CLI that reads project-local `flow.toml` files, surfaces tasks, and delegates long-running background work to the `lin` hub. Flow itself no longer tries to manage servers, watchers, or tracing—that all belongs to `lin`. Flow’s job is to keep the hub running and give you a fast task entry point.

### Tasks

- Put a `flow.toml` next to your project.
- Define tasks (see example at the top of this README).
- Run `f` to fuzzy-pick a task (falls back to a numbered list if `fzf` is missing) or `f <task>` / `f run <task>` to execute directly.

### Hub delegation

- `lin` is the hub implementation; it reads `~/.config/lin/config.ts` (or `config.toml`) and owns servers/watchers/tracing.
- Flow does not read `~/.config/flow/flow.toml` anymore; point `lin` at its config and keep it running (e.g., `lin -- daemon` or `lin hub start` if you use the helper).
- Future Flow features will talk to the hub over HTTP instead of reimplementing those capabilities.

### Installing

No released binaries yet. Build locally:

```bash
cargo build --bin f
```

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
