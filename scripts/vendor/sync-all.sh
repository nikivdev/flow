#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'EOF'
Usage:
  scripts/vendor/sync-all.sh [--important] [--dry-run] [--allow-minor] [--allow-major] [--no-vendor-import]
EOF
}

important_only=false
dry_run=false
allow_minor=false
allow_major=false
import_vendor_repo=true
for arg in "$@"; do
  case "$arg" in
    --important) important_only=true ;;
    --dry-run) dry_run=true ;;
    --allow-minor) allow_minor=true ;;
    --allow-major) allow_major=true ;;
    --no-vendor-import) import_vendor_repo=false ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 1 ;;
  esac
done

important_file="scripts/vendor/important-crates.txt"
is_important() {
  local crate="$1"
  [[ -f "$important_file" ]] || return 1
  rg -n "^${crate}$" "$important_file" >/dev/null 2>&1
}

synced_any=false
while read -r crate current latest level status; do
  [[ "$status" == "update-available" ]] || continue
  if [[ "$important_only" == true ]] && ! is_important "$crate"; then
    continue
  fi

  case "$level" in
    patch) ;;
    minor)
      [[ "$allow_minor" == true || "$allow_major" == true ]] || {
        echo "skip ${crate} ${current} -> ${latest} (minor; pass --allow-minor)"
        continue
      }
      ;;
    major)
      [[ "$allow_major" == true ]] || {
        echo "skip ${crate} ${current} -> ${latest} (major; pass --allow-major)"
        continue
      }
      ;;
    *)
      echo "skip ${crate} ${current} -> ${latest} (unknown level)"
      continue
      ;;
  esac

  if [[ "$dry_run" == true ]]; then
    echo "would sync ${crate} ${current} -> ${latest}"
  else
    scripts/vendor/sync-crate.sh "$crate" "$latest" --no-vendor-import
    synced_any=true
  fi
done < <(scripts/vendor/check-upstream.sh)

if [[ "$dry_run" == false && "$synced_any" == true && "$import_vendor_repo" == true && -f vendor.lock.toml ]]; then
  scripts/vendor/vendor-repo.sh import-local
fi
