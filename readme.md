# [flow](https://myflow.sh)

> Everything you need to move your project faster

## Install

Install the latest release (macOS/Linux):

```sh
curl -fsSL https://myflow.sh/install.sh | sh
```

Then run:

```sh
~/.flow/bin/f --version
~/.flow/bin/f doctor
```

If `f` is not found by name immediately, open a new shell (`exec zsh -l` on zsh).

The installer verifies SHA-256 checksums when available. If you are installing a legacy release
that doesn't ship `checksums.txt`, it will warn and continue (GitHub download only). To bypass
verification explicitly (not recommended), set `FLOW_INSTALL_INSECURE=1`.

## Upgrade

Upgrade to the latest release:

```sh
f upgrade
```

Upgrade to the latest canary build:

```sh
f upgrade --canary
```

Switch back to stable:

```sh
f upgrade --stable
```

If you fork Flow (or publish releases under a different repo), set:

- `FLOW_UPGRADE_REPO=owner/repo`
- `FLOW_GITHUB_TOKEN` (or `GITHUB_TOKEN` / `GH_TOKEN`) to avoid GitHub API rate limits

If you are upgrading to a very old tag that doesn't ship `checksums.txt`, you can force bypassing
checksum verification with `FLOW_UPGRADE_INSECURE=1` (not recommended).

## Build From Source

Clone Flow, hydrate the pinned vendor snapshot, and install an optimized local build:

```sh
git clone https://github.com/nikivdev/flow.git
cd flow
./scripts/vendor/vendor-repo.sh hydrate
FLOW_PROFILE=release ./scripts/deploy.sh
~/bin/f --version
```

- `./scripts/vendor/vendor-repo.sh hydrate` reuses `.vendor/flow-vendor` if it already exists.
- If `.vendor/flow-vendor` is missing, it clones the pinned vendor repo from [`vendor.lock.toml`](vendor.lock.toml) and materializes `lib/vendor/*` from that exact commit.
- The pinned vendor repo is public: `https://github.com/nikivdev/flow-vendor`
- `FLOW_PROFILE=release ./scripts/deploy.sh` builds the optimized release binary and installs `f` / `flow` / `lin` into `~/bin` (and symlinks into `~/.local/bin` if that directory exists).

If you want to populate the vendor checkout yourself first, that works too:

```sh
git clone https://github.com/nikivdev/flow-vendor.git .vendor/flow-vendor
./scripts/vendor/vendor-repo.sh hydrate
```

## Dev Fast

Typical local loop:

```sh
f setup
f test
f deploy
```

- `f setup` checks the workspace and toolchain.
- `f test` runs the test suite.
- `f deploy` builds and installs the local CLI into your path.

If you want to inspect tasks first:

```sh
f tasks list
```

## Features

To see the current CLI surface:

```sh
f --help
```

For deeper docs, read [`docs/`](docs).

## Supported Platforms

Release artifacts are built for:

- macOS: `arm64`, `x86_64`
- Linux (glibc): `arm64`, `x86_64`

## Contributing

Use Flow and AI. For the full command surface, run `f --help`. For project docs and workflows, read [`docs/`](docs).

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
