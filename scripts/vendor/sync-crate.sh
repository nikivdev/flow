#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required"
  exit 1
fi

usage() {
  cat <<'EOF'
Usage:
  scripts/vendor/sync-crate.sh <crate> [version] [--no-vendor-import]

Examples:
  scripts/vendor/sync-crate.sh reqwest
  scripts/vendor/sync-crate.sh reqwest 0.12.24
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

import_vendor_repo=true
args=()
for arg in "$@"; do
  case "$arg" in
    --no-vendor-import) import_vendor_repo=false ;;
    *) args+=("$arg") ;;
  esac
done

if [[ ${#args[@]} -lt 1 || ${#args[@]} -gt 2 ]]; then
  usage
  exit 1
fi

crate="${args[0]}"
version="${args[1]:-}"

if [[ -z "$version" ]]; then
  version="$(
    curl -fsSL "https://crates.io/api/v1/crates/${crate}" \
      | jq -r '.crate.max_stable_version // .crate.newest_version'
  )"
fi

scripts/vendor/inhouse-crate.sh "$crate" "$version"
scripts/vendor/apply-trims.sh "$crate"

if [[ "$import_vendor_repo" == true && -f vendor.lock.toml ]]; then
  scripts/vendor/vendor-repo.sh import-local
fi

echo "synced ${crate}@${version} and re-applied local trims"
