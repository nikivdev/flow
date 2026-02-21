#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

# Enforce lowercase readme filenames in tracked files.
bad="$(git ls-files | rg '(^|/)README\.md$' || true)"

if [[ -n "$bad" ]]; then
  echo "error: uppercase README.md paths are not allowed; use lowercase readme.md"
  echo "$bad" | sed 's/^/  - /'
  exit 1
fi

echo "ok: no uppercase README.md paths found"
