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
- Install `fzf` (used by `f` for fuzzy selection)
- Install Rust (if missing)
- Clone/update Flow + Jazz under `~/code/org/1f` (if Jazz is accessible)
- Patch Cargo to use local Groove crates
- Build Flow and symlink `f`/`flow` (and `lin` if present) into `~/.local/bin`
- Fallback to a release install if Jazz is not accessible

Overrides:

- `FLOW_DEV_ROOT` to change the base directory (default `~/code/org/1f`)
- `FLOW_REPO_URL` / `FLOW_JAZZ_URL` to use forks
- `FLOW_BIN_DIR` to change where binaries are linked
- `FLOW_GITHUB_TOKEN` (or `GITHUB_TOKEN`) if a repo is private
- `FLOW_GIT_SSH=1` to use SSH URLs (requires SSH key access)
- `FLOW_JAZZ_OPTIONAL=0` to require Jazz access (otherwise it falls back to release)
- If Jazz is not accessible, the installer tries to use a local dist tarball from `./dist`.
- If there is no local dist tarball and no public release, the installer will fail and you need access to the private repo or a published release.
- If GitHub SSH auth fails, the installer sets `FLOW_FORCE_HTTPS=1` so `f repos clone` uses HTTPS by default.

If a private repo clone fails, the installer will run:

```sh
./scripts/setup-github-ssh.sh
```

That script prints (and copies) a single-line public key that starts with
`ssh-ed25519`. Paste that exact line into GitHub → Settings → SSH and GPG keys
→ New SSH key (Key type: Authentication).

## Dev 

With flow, run `f setup`, then `f` will search through list of tasks.

Running `f deploy` will compile and put new version of flow into your path (so its easy to make flow work for you). 

For available features, see [docs](docs) or feed `f --help` to AI.

## Examples

All projects of [Nikita](https://github.com/nikivdev) run on flow. Like [rust](https://github.com/nikivdev/rust) & flow itself.

## Contributing

[Use AI](https://nikiv.dev/how-i-code) & flow. All meaningful issues and PRs will be merged in. Thank you.

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
