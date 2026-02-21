#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

usage() {
  cat <<'EOF'
Usage:
  scripts/vendor/apply-trims.sh [crate]

Examples:
  scripts/vendor/apply-trims.sh
  scripts/vendor/apply-trims.sh reqwest
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

target_crate="${1:-}"

apply_reqwest_trims() {
  local file="lib/vendor/reqwest/Cargo.toml"
  [[ -f "$file" ]] || return 0

  # Keep hyper surfaces as explicit as possible; avoid implicit default feature fan-out.
  perl -0777 -i -pe '
    s/(\[target\.\x27cfg\(not\(target_arch = "wasm32"\)\)\x27\.dependencies\.hyper\]\nversion = "1\.1"\nfeatures = \[\n    "http1",\n    "client",\n\]\n)(?!default-features = false\n)/$1default-features = false\n/s;
    s/(\[target\.\x27cfg\(not\(target_arch = "wasm32"\)\)\x27\.dependencies\.hyper-util\]\nversion = "0\.1\.12"\nfeatures = \[\n    "http1",\n    "client",\n    "client-legacy",\n    "client-proxy",\n    "tokio",\n\]\n)(?!default-features = false\n)/$1default-features = false\n/s;
  ' "$file"
}

apply_axum_trims() {
  local file="lib/vendor/axum/Cargo.toml"
  [[ -f "$file" ]] || return 0

  perl -0777 -i -pe '
    s/(\[dependencies\.hyper\]\nversion = "1\.1\.0"\noptional = true\n)(?!default-features = false\n)/$1default-features = false\n/s;
    s/(\[dependencies\.hyper-util\]\nversion = "0\.1\.3"\nfeatures = \[\n    "tokio",\n    "server",\n    "service",\n\]\noptional = true\n)(?!default-features = false\n)/$1default-features = false\n/s;
  ' "$file"
}

if [[ -n "$target_crate" ]]; then
  case "$target_crate" in
    reqwest) apply_reqwest_trims ;;
    axum) apply_axum_trims ;;
    *)
      echo "warning: no trim rules defined for crate '$target_crate'"
      ;;
  esac
else
  apply_reqwest_trims
  apply_axum_trims
fi
