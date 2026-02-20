# [flow](https://myflow.sh)

> Everything you need to move your project faster

## Install (Public)

Install the latest release (macOS/Linux):

```sh
curl -fsSL https://myflow.sh/install.sh | sh
# or:
curl -fsSL https://raw.githubusercontent.com/nikivdev/flow/main/install.sh | sh
```

Then run `f --version` and `f doctor`.

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

## Supported Platforms

Release artifacts are built for:

- macOS: `arm64`, `x86_64`
- Linux (glibc): `arm64`, `x86_64`

## Release Signing (Maintainers)

The GitHub Actions release workflow can code-sign macOS binaries if these repository
secrets are configured:

- `MACOS_SIGN_P12_B64`: base64-encoded `.p12` certificate
- `MACOS_SIGN_P12_PASSWORD`: password for the `.p12`
- `MACOS_SIGN_IDENTITY`: signing identity (e.g. `Developer ID Application: ... (TEAMID)`)

Flow can store these values in the Flow personal env store and sync them to GitHub:

```sh
f release signing status
f release signing store --p12 /path/to/cert.p12 --p12-password '...' --identity 'Developer ID Application: ... (TEAMID)'
f release signing sync
```

Note: Apple provides a `.cer` download, but CI signing needs a `.p12` that includes the private key.
Export the "Developer ID Application: ..." identity as `.p12` from Keychain Access.

Notarization is optional for a CLI distributed via `curl | sh` (downloads via `curl`
typically do not set the quarantine attribute), but can be added later if desired.

## Releasing (Maintainers)

Stable releases are cut by pushing a git tag that starts with `v`:

1. Bump version in `Cargo.toml`
2. Push the commit to `main`
3. Create + push tag: `vX.Y.Z` (this triggers `.github/workflows/release.yml`)

Canary releases are published automatically on every push to `main` via
`.github/workflows/canary.yml` (GitHub release tag: `canary`).

Optional Linux host SIMD lane:

- Flow has first-class tasks for CI runner mode (no GitHub env/repo vars needed):
  - `f ci-blacksmith-status`
  - `f ci-blacksmith-enable`
  - `f ci-blacksmith-enable-apply`
  - `f ci-blacksmith-disable`
  - `f ci-blacksmith-disable-apply`
- `ci-blacksmith-enable` switches Linux jobs to Blacksmith runners and enables
  the Linux host SIMD build job (`--features linux-host-simd-json` with
  `RUSTFLAGS=-C target-cpu=native`).
- `ci-blacksmith-disable` reverts to GitHub-hosted Linux runners and keeps the
  SIMD lane disabled by default for reliability.
- `*-apply` variants also commit and push workflow changes in one command.
- Blacksmith setup and runner tags:
  - https://docs.blacksmith.sh/github-actions/quickstart
  - https://docs.blacksmith.sh/github-actions/runner-tags

## Local Build (macOS, Flow + Jazz/Groove)

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

[Nikita](https://github.com/nikivdev)'s projects run on flow, including flow itself.

## Contributing

[Use AI](https://nikiv.dev/how-i-code) & flow. All meaningful issues and PRs will be merged in. Thank you.

[![Discord](https://go.nikiv.dev/badge-discord)](https://go.nikiv.dev/discord) [![X](https://go.nikiv.dev/badge-x)](https://x.com/nikivdev) [![nikiv.dev](https://go.nikiv.dev/badge-nikiv)](https://nikiv.dev)
