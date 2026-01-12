#!/bin/bash
set -e

# Publish Flow to npm
# Usage: ./scripts/npm-publish.sh <version>
# Example: ./scripts/npm-publish.sh 0.1.0

VERSION="${1:-}"
if [ -z "$VERSION" ]; then
  echo "Usage: ./scripts/npm-publish.sh <version>"
  exit 1
fi

echo "Publishing npm package v$VERSION..."

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! command -v f >/dev/null 2>&1; then
  echo "Missing 'f' on PATH. Build flow first or run 'cargo run --bin f -- publish npm publish'."
  exit 1
fi

f publish npm publish --build --version "$VERSION"
