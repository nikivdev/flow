# Verify `curl -fsSL https://myflow.sh/install.sh | sh` Installs the Latest Flow Release

Use this runbook whenever you need to prove that the public installer is actually pulling the current latest stable Flow release.

The fastest repo-local check is now:

```sh
./scripts/verify-install-latest-release.sh
```

Or through Flow:

```sh
f verify-install-latest-release
```

This is the check that matters for users:

```sh
curl -fsSL https://myflow.sh/install.sh | sh
```

## What This Must Prove

After a stable release, these values must all agree:

- `Cargo.toml` package version
- pushed release tag `vX.Y.Z`
- GitHub `releases/latest` tag
- version reported by a fresh temp-home install of `~/.flow/bin/f`

If any one of those differs, the public install story is broken.

## One-Command Verification

The script performs all of these checks:

1. validate expected tag vs `Cargo.toml` via `scripts/check_release_tag_version.py`
2. poll GitHub `releases/latest` until it matches the expected tag
3. run the real public installer in a fresh temp `HOME`
4. verify the installed binary version
5. download the direct release asset for the current platform and verify that too

Default usage:

```sh
./scripts/verify-install-latest-release.sh
```

Useful options:

```sh
./scripts/verify-install-latest-release.sh --latest-timeout 300
./scripts/verify-install-latest-release.sh --tag v0.1.3
./scripts/verify-install-latest-release.sh --skip-asset
./scripts/verify-install-latest-release.sh --keep-temp
```

## When To Run This

Run this after:

- cutting a new stable release tag
- changing `install.sh`
- changing release packaging
- changing versioning logic
- fixing a release mismatch bug

## Fast Pass Criteria

The installer is correct only if all of these are true:

1. the release tag matches `Cargo.toml`
2. GitHub marks that tag as latest stable
3. a fresh temp-home install gets that same version
4. a direct release asset download reports that same version

## Manual Debug Procedure

If the one-command script fails, use the manual steps below to see exactly where the mismatch is.

### Step 1: Confirm the Expected Version Locally

Read the package version from the repo:

```sh
python3 - <<'PY'
import pathlib, re
text = pathlib.Path("Cargo.toml").read_text(encoding="utf-8")
match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
if not match:
    raise SystemExit("failed to read Cargo.toml version")
print(match.group(1))
PY
```

If you already know the expected tag, validate it directly:

```sh
python3 scripts/check_release_tag_version.py v0.1.3
```

That script should fail hard on mismatches.

### Step 2: Confirm GitHub Latest Stable

Check the public API that the installer uses:

```sh
curl -fsSL https://api.github.com/repos/nikivdev/flow/releases/latest \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["tag_name"])'
```

This should print the expected stable tag, for example:

```text
v0.1.3
```

Optional cross-checks:

```sh
gh release list --limit 5
gh release view v0.1.3 --json tagName,publishedAt,isDraft,isPrerelease,url
```

### Step 3: Run the Real Public Installer in a Fresh Temp HOME

This is the main test. Use a fresh `HOME` and a minimal `PATH` so an existing install cannot leak in.

```sh
tmp_home="$(mktemp -d)"
echo "$tmp_home"

HOME="$tmp_home" PATH="/usr/bin:/bin:/usr/sbin:/sbin" sh -c \
  'curl -fsSL https://myflow.sh/install.sh | sh'

HOME="$tmp_home" "$tmp_home/.flow/bin/f" --version
```

Expected result:

- install succeeds
- `~/.flow/bin/f` exists under the temp home
- `f --version` reports the latest stable version

Example expected output:

```text
flow 0.1.3
```

### Step 4: Compare Installed Version to Latest Tag

Use this one-shot comparison:

```sh
latest_tag="$(curl -fsSL https://api.github.com/repos/nikivdev/flow/releases/latest \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["tag_name"])')"

tmp_home="$(mktemp -d)"
HOME="$tmp_home" PATH="/usr/bin:/bin:/usr/sbin:/sbin" sh -c \
  'curl -fsSL https://myflow.sh/install.sh | sh >/dev/null'

installed_version="$(HOME="$tmp_home" "$tmp_home/.flow/bin/f" --version \
  | python3 -c 'import sys,re; m=re.search(r"flow ([0-9][^ ]*)", sys.stdin.read()); print(m.group(1) if m else "")')"

echo "latest_tag=$latest_tag"
echo "installed_version=$installed_version"

test "v$installed_version" = "$latest_tag"
```

If that final `test` fails, the installer path is not trustworthy.

### Step 5: Isolate Installer Bug vs Release Artifact Bug

If the fresh install reports the wrong version, download the release asset directly.

Choose the target for your machine:

- macOS Apple Silicon: `aarch64-apple-darwin`
- macOS Intel: `x86_64-apple-darwin`
- Linux x64: `x86_64-unknown-linux-gnu`
- Linux arm64: `aarch64-unknown-linux-gnu`

Example for macOS Apple Silicon:

```sh
latest_tag="$(curl -fsSL https://api.github.com/repos/nikivdev/flow/releases/latest \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["tag_name"])')"

tmp_dir="$(mktemp -d)"
cd "$tmp_dir"

curl -fsSLO \
  "https://github.com/nikivdev/flow/releases/download/${latest_tag}/flow-aarch64-apple-darwin.tar.gz"

tar -xzf flow-aarch64-apple-darwin.tar.gz
./f --version
```

Interpretation:

- direct asset wrong too: release artifact/versioning bug
- direct asset correct but installer wrong: installer selection logic bug
- API still shows old tag: release publication/propagation is not complete yet

## Common Failure Modes

### 1. Release tag does not match Cargo version

Symptom:

- `python3 scripts/check_release_tag_version.py vX.Y.Z` fails

Meaning:

- the release was tagged from the wrong crate version

Fix:

- bump `Cargo.toml`
- refresh generated artifacts if needed
- cut a new release tag

### 2. GitHub `releases/latest` still returns the old tag

Symptom:

- release workflow is green
- release page shows the new version
- `releases/latest` still returns the old tag for a short time

Meaning:

- GitHub publication or cache propagation delay

Fix:

- wait and retry until `releases/latest` flips
- do not declare success until the API itself returns the new tag

### 3. Installer reports old version but latest tag is correct

Symptom:

- `releases/latest` returns the new tag
- temp-home install still reports an older version

Meaning:

- likely wrong binary inside the published release asset

Fix:

- test the direct asset
- if direct asset is also wrong, cut a corrected release

### 4. Temp-home test passes locally but users still get old `f`

Symptom:

- your test is clean
- user machine still reports an older version by plain `f --version`

Meaning:

- user shell is resolving another binary earlier on `PATH`

Fix:

- ask them to run:

```sh
which -a f
~/.flow/bin/f --version
```

## Recommended Release Checklist

After publishing a stable tag:

1. run `python3 scripts/check_release_tag_version.py vX.Y.Z`
2. wait for the Release workflow to complete
3. verify `releases/latest` returns `vX.Y.Z`
4. run `./scripts/verify-install-latest-release.sh`
5. if there is any mismatch, test the direct asset before debugging the installer

Do not mark the release done until step 4 is green.
