#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is required"
  exit 1
fi

tmp_meta="$(mktemp)"
trap 'rm -f "$tmp_meta"' EXIT

cargo metadata --format-version 1 >"$tmp_meta"

echo "== Registry Footprint =="
echo -n "unique registry crates: "
jq -r '
  [.packages[] | select(.source != null and (.source | startswith("registry+"))) | .name]
  | unique
  | length
' "$tmp_meta"

echo -n "proc-macro crates: "
jq -r '
  [
    .packages[]
    | select(any(.targets[]?; any(.kind[]?; . == "proc-macro")))
    | .name
  ]
  | unique
  | length
' "$tmp_meta"

echo
echo "== Direct Dependencies Ranked By Tree Size =="
deps="$(
  sed -n '/^\[dependencies\]/,/^\[/p' Cargo.toml \
    | rg -o '^[A-Za-z0-9_.-]+' \
    | sort -u
)"

while IFS= read -r dep; do
  [[ -z "$dep" ]] && continue
  if lines="$(cargo tree -p "$dep" --depth 20 2>/dev/null | wc -l | tr -d ' ')"; then
    printf "%5d  %s\n" "$lines" "$dep"
  fi
done <<<"$deps" | sort -nr

echo
echo "== Duplicate Versions (cargo tree -d) =="
cargo tree -d
