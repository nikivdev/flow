# flow

> Project level config for insanely fast feedback loops

The goal of this CLI is to parse out `flow.toml` files like the one in [this repo](flow.toml).

## Install

> [!NOTE]
> Below `curl ` seems to be breaking, unclear why so for now to use `flow`, build it from source. PRs welcome to fix the `curl ` command ♥️

```bash
curl -fsSL https://raw.githubusercontent.com/nikivdev/flow/main/scripts/install.sh | bash
```

## Short summary

For longer list of features available, see [docs/features.md](docs/features.md).

Currently [this thread](https://x.com/nikivdev/status/1997297174074499247) gives good overview of how you can use this tool to move fast with AI.

I would suggest to open the repo and ask questions with claude code or codex how to make best use of the app. What it does well now is that you can define tasks in config.

Like so:

```
version = 1
name = "ts"

[deps]
fast = "github.com/1focus-ai/fast"

[[tasks]]
name = "setup"
command = "bun i"

[[tasks]]
name = "dev"
command = "bun --watch run.ts"

[[tasks]]
name = "commit"
command = "fast commitPush"
description = "Commit with AI"
dependencies = ["fast"]
delegate_to_hub = true
```

Above is from [ts repo](https://github.com/nikivdev/ts). Then you can run `f` to fuzzy search through tasks to run. Or do `f <task>` to run specific task.

If you setup [LM Studio](https://lmstudio.ai) & load MLX model like OpenAI 20B one, you can even make mistakes in `f <task>` and it would do a tool call match for you.

All flow tasks are traced for output/error after you ran them. `f last-cmd` would return the last commands output. In practice you can do this, open [warp](https://warp.dev) or [cursor](https://cursor.com) and then just ask agents things, you can literally say, `make me a flow.toml task to do..` then you run the task with `f <task>` or `f rerun` to rerun last ran task.

And on errors, what I do at least is have [Keyboard Maestro](https://keyboardmaestro.com/main) macro to paste the output instantly into the agent. This way the feedback loop is insanely tight and you can iterate very fast.

Below readme is mostly generated with AI so feel free to ignore.

I also use `flow` CLI to manage [hubs](#hub) but its experimental as the hub implementation I am running is closed code. The big idea is that `flow` just keeps the hub alive and that's it.

## Building (does not work..)

- macOS/Linux. Needs `curl` + `tar`. Installs `f` and a `flow` symlink to `~/.local/bin` (set `FLOW_BIN_DIR=/usr/local/bin` to change).
- Defaults to the latest GoReleaser binary; only falls back to building from source if a release isn’t found.
- Optional envs: `FLOW_VERSION=vX.Y.Z` (pin), `FLOW_INSTALL_LIN=0` (skip lin), `FLOW_NO_RELEASE=1` (force source build).
- Add the bin dir to PATH if needed (e.g. `echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile`), then run `f --version`.

Manual build remains available:

```bash
cargo build --bin f
# Optional: build the bundled lin watcher/hub helper if you don't already have lin installed
cargo build --bin lin
```

## Release builds (GoReleaser) (does not work..)

- One-time deps: `brew install goreleaser zig` (or get binaries), `cargo install --locked cargo-zigbuild`, `rustup default stable`.
- Quick local dry run: `goreleaser release --snapshot --clean`
- Publish a tag: `git tag v0.1.0 && git push origin v0.1.0` then `GITHUB_TOKEN=... goreleaser release --clean` (or let CI run on the tag).
- Outputs `flow_<version>_<os>_<arch>.tar.gz` + checksums for darwin/linux (amd64/arm64); installer pulls these by default.

Once you have `f` (also available as `flow`) CLI, create `flow.toml` in some project with tasks like:

```
version = 1

[deps]
fast = "fast"  # single-command dependency; use ["cmd1", "cmd2"] for multiples

# Optional: Flox install set to keep tools reproducible (parsed for future integration)
[flox]
[flox.install]
eza.pkg-path = "eza"
bat.pkg-path = "bat"
edgedb.pkg-path = "edgedb"
coreutils.pkg-path = "coreutils"
parallel.pkg-path = "parallel"
nil.pkg-path = "nil"
nixfmt.pkg-path = "nixfmt"
nixd.pkg-path = "nixd"
deploy-rs.pkg-path = "deploy-rs"
nixos-rebuild.pkg-path = "nixos-rebuild"
nix-index.pkg-path = "nix-index"
xcpretty.pkg-path = "xcpretty"

[[tasks]]
name = "deploy-cli-local"
command = "./scripts/deploy.sh"
description = "Build the CLI/daemon and copy the binary to ~/.local/bin/flow"
```

And if you run `f` in the project it will fuzzy search through all tasks you can run. You can also run tasks with `f <task>`.

## Commands

- `f init` — scaffold a starter `flow.toml` in the current directory with stub `setup`/`dev` tasks.
- `f tasks` — list tasks from the current `flow.toml` (name + description).
- `f` (or `f <task>`) — interactive picker / direct task execution when a local `flow.toml` exists.
- `f search` / `f s` — fuzzy search global commands/tasks (falls back to `~/.config/flow/flow.toml` if present).
- `f hub start|stop` — ensure the `lin` hub is running (or stop it). Use `f hub --help` for flags.

## Hub

There is component in flow that is a hub. It's a daemon that does things. Flow is not responsible for what the daemon does, all it does is it makes sure this daemon runs and in future perhaps auto heals or restarts as the idea is that the daemon should always be running.

There is an implementation of such hub built in private called `lin`. Will be possible to use soon, for now it's being tested in private as there are bugs. The goal of lin is to declaratively specify servers to run and trace all terminal I/O. In future more. Flow keeps a pointer to your production lin binary at `~/.config/flow/hub-runtime.json` (written by `lin register`); `f hub start` health-checks `http://127.0.0.1:9050/health` and launches that registered binary in daemon mode (passing `--config` if you supply one). If you want to experiment with a dev build, run it on another port so Flow can keep the production copy alive on the default port.

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

## Contributing

Make issues with bugs/features. Any PR to improve project is welcome. Ideally with **working** & **useful** patches but non finished ideas are great too. If idea/feature is sound, it will be merged eventually.

[This](https://nikiv.dev/how-i-code) is nice overview of a coding workflow that works that you can adapt.

### 🖤

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
