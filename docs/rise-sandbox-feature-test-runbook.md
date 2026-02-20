# Rise Sandbox Feature Test Runbook (Flow)

Use this when you want deterministic, isolated feature checks in a VM and fast feedback for infra tuning.

## Goal

- Verify a feature works in a clean sandbox.
- Avoid host-machine state leaks.
- Capture timings/logs so CI/CD and install paths can be optimized.

## Prereqs

- macOS host.
- `rise` available.
- `vibe` VM binary from `~/repos/lynaghk/vibe`.

`rise sandbox` expects the VM-oriented `vibe`, not the unrelated CLI binary some PATHs contain.

Preflight:

```bash
cd ~/repos/lynaghk/vibe
cargo build --release
```

## Canonical Sandbox Command

From `~/code/rise`:

```bash
rise sandbox "set -euo pipefail; <your commands>; echo SANDBOX_OK" \
  --root /Users/nikiv/code/flow \
  --expect SANDBOX_OK
```

Why this shape:

- `set -euo pipefail` fails hard on the first real issue.
- `--expect` gives a strict pass/fail marker.
- `--root /Users/nikiv/code/flow` mounts the Flow repo into `/root/project`.

## Feature Test Template

Replace with your feature command:

```bash
rise sandbox "set -euo pipefail; cd /root/project; <feature command>; echo FEATURE_OK" \
  --root /Users/nikiv/code/flow \
  --expect FEATURE_OK
```

## Installer/Release Verification (Flow)

Use this to verify `curl -fsSL https://myflow.sh/install.sh | sh` pulls the newest canary:

```bash
rise sandbox "set -euo pipefail; curl -fsSL https://myflow.sh/install.sh | sh; ~/.flow/bin/f --version; echo INSTALL_OK" \
  --root /Users/nikiv/code/flow \
  --expect INSTALL_OK
```

Then verify canary tag points to the commit you expect:

```bash
git ls-remote --tags https://github.com/nikivdev/flow.git canary
```

## Infra Optimization Loop

1. Run the same sandbox test 3-5 times.
2. Record:
   - VM boot + script duration from `rise sandbox` output.
   - Feature command duration inside script (`time <cmd>` if needed).
   - Artifact install/build timing (`f --version`, compile/install steps).
3. Compare before/after infra changes:
   - CI runner mode (`github` vs `host` vs `blacksmith`).
   - Caching changes.
   - Installer path changes.

Sandbox logs are emitted under:

```bash
~/code/flow/out/logs/sandbox-<timestamp>.log
```

## Common Failures

### `vibe: error: unrecognized arguments: --cpus --ram ...`

Cause: wrong `vibe` binary in PATH.

Fix: build/use `~/repos/lynaghk/vibe/target/release/vibe` (rise resolves this first when present).

### Sandbox passes but installed version seems old

Check:

1. Canary tag head:
   ```bash
   git ls-remote --tags https://github.com/nikivdev/flow.git canary
   ```
2. Latest canary workflow status:
   ```bash
   gh run list -R nikivdev/flow --limit 5
   ```

If canary tag already points to your target commit, installer should fetch that version even if other optional jobs are still running.

