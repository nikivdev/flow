#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/update-deps.sh [options]

Options:
  --dry-run         Show what would be updated without writing changes.
  --important       Only update crates listed in scripts/vendor/important-crates.txt.
  --no-major        Disallow major updates (default allows major).
  --no-minor        Disallow minor updates (default allows minor).
  --no-cargo-update Skip `cargo update --workspace`.
  --no-audit        Skip strict vendoring audit.
  --no-check        Skip `cargo check -q`.
  --push-vendor     Push .vendor/flow-vendor checkout after import/pin.

Behavior:
  - Updates vendored crates to latest allowed versions.
  - Re-applies deterministic trim/warning-hygiene patches.
  - Imports local vendor state and pins vendor.lock.toml commit.
  - Optionally refreshes Cargo.lock and validates with strict checks.
USAGE
}

dry_run=false
important_only=false
allow_minor=true
allow_major=true
run_cargo_update=true
run_audit=true
run_check=true
push_vendor=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) dry_run=true; shift ;;
    --important) important_only=true; shift ;;
    --no-major) allow_major=false; shift ;;
    --no-minor) allow_minor=false; shift ;;
    --no-cargo-update) run_cargo_update=false; shift ;;
    --no-audit) run_audit=false; shift ;;
    --no-check) run_check=false; shift ;;
    --push-vendor) push_vendor=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "error: unknown arg: $1"
      usage
      exit 1
      ;;
  esac
done

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required"
  exit 1
fi

find_python_with_tomllib() {
  local candidate
  for candidate in python3 python3.12 python3.11 python; do
    command -v "$candidate" >/dev/null 2>&1 || continue
    if "$candidate" - <<'PY' >/dev/null 2>&1
import tomllib  # noqa: F401
PY
    then
      echo "$candidate"
      return 0
    fi
  done
  return 1
}

sync_args=()
check_args=()
if [[ "$important_only" == true ]]; then
  sync_args+=(--important)
  check_args+=(--important)
fi
if [[ "$allow_minor" == true ]]; then
  sync_args+=(--allow-minor)
fi
if [[ "$allow_major" == true ]]; then
  sync_args+=(--allow-major)
fi

echo "== update-deps: upstream scan =="
upstream_json="$(scripts/vendor/check-upstream.sh "${check_args[@]}" --json)"
updates_total="$(printf '%s\n' "$upstream_json" | jq '[.[] | select(.status=="update-available")] | length')"
patch_updates="$(printf '%s\n' "$upstream_json" | jq '[.[] | select(.status=="update-available" and .level=="patch")] | length')"
minor_updates="$(printf '%s\n' "$upstream_json" | jq '[.[] | select(.status=="update-available" and .level=="minor")] | length')"
major_updates="$(printf '%s\n' "$upstream_json" | jq '[.[] | select(.status=="update-available" and .level=="major")] | length')"
echo "updates available: ${updates_total} (patch=${patch_updates}, minor=${minor_updates}, major=${major_updates})"

if [[ "$dry_run" == true ]]; then
  echo
  echo "== update-deps: dry-run sync plan =="
  scripts/vendor/sync-all.sh "${sync_args[@]}" --dry-run
  exit 0
fi

echo
echo "== update-deps: sync vendored crates =="
scripts/vendor/sync-all.sh "${sync_args[@]}" --no-vendor-import

echo
echo "== update-deps: apply trims/warning hygiene =="
scripts/vendor/apply-trims.sh

if [[ -f vendor.lock.toml ]]; then
  echo
  echo "== update-deps: import + pin vendor repo state =="
  scripts/vendor/vendor-repo.sh import-local
fi

if [[ "$run_cargo_update" == true ]]; then
  echo
  echo "== update-deps: cargo lock refresh =="
  cargo update --workspace
fi

if [[ "$run_audit" == true ]]; then
  if ! audit_python="$(find_python_with_tomllib)"; then
    echo "error: strict audit requires Python 3.11+ (tomllib). Use --no-audit to skip."
    exit 1
  fi
  echo
  echo "== update-deps: strict vendoring audit =="
  "$audit_python" ./scripts/vendor/rough_edges_audit.py --project . --strict-warnings
fi

if [[ "$run_check" == true ]]; then
  echo
  echo "== update-deps: cargo check =="
  cargo check -q
fi

if [[ "$push_vendor" == true ]]; then
  echo
  echo "== update-deps: push vendor repo =="
  scripts/vendor/vendor-repo.sh push
fi

echo
echo "update-deps complete"
