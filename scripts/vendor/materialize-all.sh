#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/materialize-all.sh [--important] [--from-cache]

Options:
  --important   Materialize only crates listed in scripts/vendor/important-crates.txt
  --from-cache  Ignore vendor.lock.toml and materialize from local crate cache metadata
USAGE
}

important_only=false
from_cache=false
for arg in "$@"; do
  case "$arg" in
    --important) important_only=true ;;
    --from-cache) from_cache=true ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 1 ;;
  esac
done

if [[ "$from_cache" == false && -f vendor.lock.toml ]]; then
  scripts/vendor/vendor-repo.sh hydrate
  exit 0
fi

important_file="scripts/vendor/important-crates.txt"
is_important() {
  local crate="$1"
  [[ -f "$important_file" ]] || return 1
  rg -n "^${crate}$" "$important_file" >/dev/null 2>&1
}

read_field() {
  local file="$1"
  local key="$2"
  awk -F'"' -v key="$key" '$1 ~ "^" key " = " { print $2; exit }' "$file"
}

shopt -s nullglob
manifest_files=(lib/vendor-manifest/*.toml)
shopt -u nullglob

if [[ ${#manifest_files[@]} -eq 0 ]]; then
  echo "no manifests found in lib/vendor-manifest"
  exit 0
fi

for manifest in "${manifest_files[@]}"; do
  crate="$(read_field "$manifest" "crate")"
  version="$(read_field "$manifest" "version")"
  [[ -n "$crate" && -n "$version" ]] || continue

  if [[ "$important_only" == true ]] && ! is_important "$crate"; then
    continue
  fi

  scripts/vendor/inhouse-crate.sh "$crate" "$version"
  scripts/vendor/apply-trims.sh "$crate"
  echo "materialized ${crate}@${version}"
done
