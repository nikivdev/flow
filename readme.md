# flow

> Everything you need to move your project faster

CLI that parses `flow.toml` files to define and run project tasks with AI-assisted workflows. And more.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/nikivdev/flow/main/scripts/install.sh | bash
```

This downloads prebuilt binaries from GitHub releases. Falls back to building from source if no binary is available for your platform.

**Environment variables:**
- `FLOW_VERSION=v0.1.0` - Install specific version
- `FLOW_BIN_DIR=/custom/path` - Custom install location (default: `~/.local/bin`)

After install, add to your PATH if needed:
```bash
export PATH="$HOME/.local/bin:$PATH"
```

## Upgrade

```bash
f upgrade              # Upgrade to latest version
f upgrade --dry-run    # Check what would be upgraded
f upgrade v0.2.0       # Install specific version
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
fast = "github.com/nikivdev/fast"

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

## Building from Source

```bash
# Clone the repo
git clone https://github.com/nikivdev/flow.git
cd flow

# Build
cargo build --release --bin f --bin lin

# Install
cp target/release/f ~/.local/bin/
cp target/release/lin ~/.local/bin/
ln -sf ~/.local/bin/f ~/.local/bin/flow
```

## Creating Releases

For maintainers to create new releases:

```bash
# Build release binary
cargo build --release --bin f --bin lin

# Create tarball (adjust os/arch as needed)
mkdir -p dist
cd target/release
tar -czvf ../../dist/flow_v0.2.0_darwin_arm64.tar.gz f lin

# Create GitHub release with assets
f release create v0.2.0 -a dist/flow_v0.2.0_darwin_arm64.tar.gz --generate-notes
```

Or use the `gh` CLI directly:
```bash
gh release create v0.2.0 --generate-notes dist/flow_v0.2.0_darwin_arm64.tar.gz
```

## Configuration

Once you have `f` (also available as `flow`) CLI, create `flow.toml` in your project:

```toml
version = 1
name = "myproject"

[[tasks]]
name = "setup"
command = "npm install"
description = "Install dependencies"

[[tasks]]
name = "dev"
command = "npm run dev"
description = "Start development server"

[[tasks]]
name = "test"
command = "npm test"
```

Run `f` in your project to fuzzy search through tasks, or `f <task>` to run directly.

## Commands

**Tasks:**
- `f` — Interactive fuzzy picker for tasks
- `f <task>` — Run a specific task
- `f tasks` — List all tasks from `flow.toml`
- `f init` — Scaffold a starter `flow.toml`
- `f rerun` — Re-run the last task

**Git & Publishing:**
- `f commit` / `f c` — AI-powered commit with code review
- `f sync` — Pull, merge upstream, push
- `f publish` — Create GitHub repository from current folder
- `f release create` — Create GitHub release with assets

**Self-management:**
- `f upgrade` — Upgrade to latest version
- `f doctor` — Check system dependencies

**Other:**
- `f search` / `f s` — Fuzzy search global tasks
- `f hub start|stop` — Manage the lin hub daemon

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

Make issues with bugs/features or submit PRs. [flow](https://github.com/nikivdev/flow) has many utils to make this easy. PRs with ideas if they are great will eventually be merged. Bug fixing is always welcome, perf improvements should ideally have some benchmark attached. Docs can always be better. See [this](https://nikiv.dev/how-i-code) for how to code fast with AI.

### 🖤

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
