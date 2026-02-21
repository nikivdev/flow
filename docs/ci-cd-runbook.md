# Flow CI/CD Runbook

This runbook documents how Flow CI/CD is wired today and how to debug it quickly when jobs fail.

## Architecture

- Workflows:
  - `.github/workflows/canary.yml`: runs on every push to `main`, publishes/updates the `canary` release/tag.
  - `.github/workflows/release.yml`: runs on tag pushes matching `v*`, publishes stable releases.
- Trigger optimization:
  - Canary `push` skips docs-only changes (`docs/**`, `**/*.md`).
  - Use `workflow_dispatch` to force a Canary run when needed after docs-only changes.
- Vendored deps bootstrap:
  - Both workflows run `scripts/vendor/vendor-repo.sh hydrate` immediately after checkout in each build job.
  - This materializes `lib/vendor/*` from the pinned commit in `vendor.lock.toml` before Cargo builds.
- Build jobs in both workflows:
  - Matrix build: macOS + Linux targets.
  - SIMD build: `build-linux-host-simd` (Linux x64 with `--features linux-host-simd-json`).
  - CI builds `--bin f` only (release artifacts package `f`; avoids duplicate `flow` alias build cost).
- Release jobs:
  - Gather all build artifacts.
  - Publish release assets (and in Canary, force-move `canary` tag to current `main` commit).
  - `release` waits for both `build` and `build-linux-host-simd`.

## Runner Modes

Flow uses task-driven mode switching (not manual workflow edits):

- `github` mode:
  - Standard Linux lanes on `ubuntu-latest`.
  - SIMD lane disabled.
- `blacksmith` mode:
  - Linux lanes on Blacksmith runners.
  - SIMD lane enabled on Blacksmith.
- `host` mode:
  - Standard Linux lanes stay on GitHub-hosted runners.
  - SIMD lane runs on self-hosted label: `[self-hosted, linux, x64, ci-1focus]`.

Check/switch mode:

```bash
f ci-blacksmith-status
f ci-blacksmith-enable
f ci-blacksmith-enable-apply
f ci-host-enable
f ci-host-enable-apply
f ci-blacksmith-disable
f ci-blacksmith-disable-apply
```

## One-Command Host Setup

Preferred path (painless, idempotent):

```bash
f ci-host-setup
```

If infra host is not configured yet:

```bash
f ci-host-setup <user@ip>
```

What `f ci-host-setup` does:

1. Validates `gh` auth and `infra` host config.
2. Installs/registers the `ci-1focus` self-hosted runner on the Linux host.
3. Waits for runner to report online.
4. Switches workflows to `host` mode with commit + push.
5. Prints final runner health/status.

## Daily Operations

- Check current mode: `f ci-blacksmith-status`
- Check runner service + GitHub registration: `f ci-host-runner-status`
- Reinstall runner if needed: `f ci-host-runner-install`
- Remove runner: `f ci-host-runner-remove`

Stable release flow:

1. Merge version bump to `main`.
2. Push tag `vX.Y.Z`.
3. Watch `Release` workflow.

Canary flow:

1. Push to `main`.
2. Watch `Canary` workflow.
3. Confirm `canary` tag moved and release assets updated.

## Debug Playbook

### 1) Workflow failed or stuck

```bash
gh run list --workflow Canary --limit 10
gh run list --workflow Release --limit 10
gh run view <run-id> --log-failed
gh run watch <run-id>
```

If failure shows:

- `failed to load source for dependency 'axum'`
- `Unable to update .../lib/vendor/axum`
- `No such file or directory (os error 2)`

then vendored deps were not hydrated before build.

### 2) SIMD lane queued forever

Usually means self-hosted runner routing issue.

```bash
f ci-blacksmith-status
f ci-host-runner-status
python3 ./scripts/ci_host_runner.py health --repo nikivdev/flow
```

Expected healthy state is:

- Host service: `active`
- GitHub runner status: `online`
- Runner has label `ci-1focus`

If not healthy, run:

```bash
f ci-host-runner-install
python3 ./scripts/ci_host_runner.py wait-online --repo nikivdev/flow --timeout-secs 120 --interval-secs 5
```

### 3) Workflows not using expected runner profile

```bash
f ci-blacksmith-status
```

If wrong:

```bash
f ci-host-enable-apply
# or:
f ci-blacksmith-enable-apply
# or:
f ci-blacksmith-disable-apply
```

### 3b) Vendored repo hydrate issues

Hydration depends on `vendor.lock.toml` pin and vendor repo availability.

Quick checks:

```bash
scripts/vendor/vendor-repo.sh status
scripts/vendor/vendor-repo.sh hydrate
```

Expected:

- pinned commit in `vendor.lock.toml` is non-empty,
- hydrate logs `hydrated <crate> -> lib/vendor/<crate>`,
- `lib/vendor/axum/Cargo.toml` and `lib/vendor/reqwest/Cargo.toml` exist after hydrate.

If CI cannot clone SSH URL from lock, `vendor-repo.sh` now falls back to HTTPS clone for GitHub URLs.

### 4) `curl ... install.sh` does not fetch expected fresh build

Flow installer defaults to `canary` unless `FLOW_VERSION` is set differently. Check if `canary` moved:

```bash
git ls-remote --tags origin canary
```

Then verify in sandbox (recommended) using:

- `docs/rise-sandbox-feature-test-runbook.md`

That runbook gives an isolated `rise sandbox` smoke test for:

```bash
curl -fsSL https://myflow.sh/install.sh | sh
~/.flow/bin/f --version
```

### 5) Setup task fails mid-install

Re-run:

```bash
f ci-host-setup
```

The installer path is idempotent (it removes old service/config before re-registering). If failure persists, inspect:

```bash
f ci-host-runner-status
gh api repos/nikivdev/flow/actions/runners
```

## Notes

- CI/CD execution is defined in repo workflows; GitHub UI is control plane/visibility (runs, logs, runner state), not the source of truth for pipeline logic.
- Current performance balance: keep general Linux matrix jobs on GitHub-hosted runners, offload expensive SIMD build to the Linux host.
