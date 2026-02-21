#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required"
  exit 1
fi

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/check-upstream.sh [--important] [--json]

Options:
  --important   Check only crates listed in scripts/vendor/important-crates.txt
  --json        Emit machine-readable JSON array
USAGE
}

important_only=false
json_output=false
for arg in "$@"; do
  case "$arg" in
    --important) important_only=true ;;
    --json) json_output=true ;;
    -h|--help) usage; exit 0 ;;
    *) usage; exit 1 ;;
  esac
done

important_file="scripts/vendor/important-crates.txt"
if [[ "$important_only" == true && ! -f "$important_file" ]]; then
  echo "error: missing $important_file"
  exit 1
fi

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

classify_update_level() {
  local current="$1"
  local latest="$2"
  IFS='.' read -r c1 c2 c3 _ <<<"$current"
  IFS='.' read -r l1 l2 l3 _ <<<"$latest"
  if [[ -z "${c1:-}" || -z "${l1:-}" ]]; then
    echo "unknown"
    return
  fi
  if [[ "$current" == "$latest" ]]; then
    echo "same"
  elif [[ "$c1" != "$l1" ]]; then
    echo "major"
  elif [[ "${c2:-0}" != "${l2:-0}" ]]; then
    echo "minor"
  else
    echo "patch"
  fi
}

collect_metadata_files() {
  local files=()
  shopt -s nullglob
  for f in lib/vendor-manifest/*.toml; do
    files+=("$f")
  done

  # Backward compatibility while migrating from libs/vendor.
  if [[ ${#files[@]} -eq 0 ]]; then
    for f in lib/vendor/*/UPSTREAM.toml libs/vendor/*/UPSTREAM.toml; do
      files+=("$f")
    done
  fi
  shopt -u nullglob

  printf '%s\n' "${files[@]}"
}

rows=()
while IFS= read -r meta_file; do
  [[ -f "$meta_file" ]] || continue

  crate="$(read_field "$meta_file" "crate")"
  current="$(read_field "$meta_file" "version")"
  [[ -n "$crate" && -n "$current" ]] || continue

  if [[ "$important_only" == true ]] && ! is_important "$crate"; then
    continue
  fi

  latest="$(
    curl -fsSL "https://crates.io/api/v1/crates/${crate}" \
      | jq -r '.crate.max_stable_version // .crate.newest_version'
  )"
  level="$(classify_update_level "$current" "$latest")"

  status="up-to-date"
  if [[ "$latest" != "$current" ]]; then
    status="update-available"
  fi
  rows+=("${crate}|${current}|${latest}|${level}|${status}")
done < <(collect_metadata_files)

if [[ ${#rows[@]} -gt 0 ]]; then
  IFS=$'\n' sorted=($(printf '%s\n' "${rows[@]}" | sort))
  unset IFS
else
  sorted=()
fi

if [[ "$json_output" == true ]]; then
  if [[ ${#sorted[@]} -eq 0 ]]; then
    echo "[]"
    exit 0
  fi
  for row in "${sorted[@]}"; do
    IFS='|' read -r crate current latest level status <<<"$row"
    printf '{"crate":"%s","current":"%s","latest":"%s","level":"%s","status":"%s"}\n' \
      "$crate" "$current" "$latest" "$level" "$status"
  done | jq -s '.'
else
  echo "crate current latest level status"
  for row in "${sorted[@]}"; do
    IFS='|' read -r crate current latest level status <<<"$row"
    printf "%s %s %s %s %s\n" "$crate" "$current" "$latest" "$level" "$status"
  done
fi
