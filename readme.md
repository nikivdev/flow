# [flow](https://myflow.sh)

> Everything you need to move your project faster

<!-- todo: curl install, full automate -->

Install this CLI (currently by cloning repo and buliding rust binary).

## Local build (macOS, Flow + Jazz/Groove)

If you want a local dev build that uses the Jazz/Groove crates from a local
checkout, use the macOS installer script:

```sh
git clone https://github.com/nikivdev/flow.git
cd flow
./scripts/install-macos-dev.sh
```

This script will:

- Install `fnm` + Node (if missing)
- Install Rust (if missing)
- Clone/update Flow + Jazz under `~/code/org/1f`
- Patch Cargo to use local Groove crates
- Build Flow and symlink `f`/`flow` (and `lin` if present) into `~/.local/bin`

Overrides:

- `FLOW_DEV_ROOT` to change the base directory (default `~/code/org/1f`)
- `FLOW_REPO_URL` / `FLOW_JAZZ_URL` to use forks
- `FLOW_BIN_DIR` to change where binaries are linked

## Dev 

With flow, run `f setup`, then `f` will search through list of tasks.

Running `f deploy` will compile and put new version of flow into your path (so its easy to make flow work for you). 

For available features, see [docs](docs) or feed `f --help` to AI.

## Examples

All projects of [Nikita](https://github.com/nikivdev) run on flow. Like [rust](https://github.com/nikivdev/rust) & flow itself.

## Contributing

[Use AI](https://nikiv.dev/how-i-code) & flow. All meaningful issues and PRs will be merged in. Thank you.

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
