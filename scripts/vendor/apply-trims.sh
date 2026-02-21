#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/apply-trims.sh [crate]

Examples:
  scripts/vendor/apply-trims.sh
  scripts/vendor/apply-trims.sh regex

Behavior:
  - By default, this script is a no-op (safe baseline).
  - If scripts/vendor/trim-hooks.sh exists, it is sourced and called for extensible project-specific trims.
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

target_crate="${1:-}"

if [[ -f scripts/vendor/trim-hooks.sh ]]; then
  # shellcheck source=/dev/null
  source scripts/vendor/trim-hooks.sh
  if declare -F apply_vendor_trims >/dev/null 2>&1; then
    apply_vendor_trims "${target_crate:-}"
    exit 0
  fi
fi

if [[ -n "$target_crate" ]]; then
  echo "note: no default trim rules for '$target_crate' (create scripts/vendor/trim-hooks.sh)"
else
  echo "note: no default trim rules (create scripts/vendor/trim-hooks.sh)"
fi
