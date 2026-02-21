#!/usr/bin/env bash
set -euo pipefail

# Project-specific trim rules for Flow vendored crates.
# Called by scripts/vendor/apply-trims.sh as: apply_vendor_trims "<crate>"

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

apply_vendor_trims() {
  local crate="${1:-}"
  if [[ -n "$crate" ]]; then
    case "$crate" in
      reqwest) apply_reqwest_trims ;;
      axum) apply_axum_trims ;;
      *) echo "warning: no trim rules defined for crate '$crate'" ;;
    esac
    return
  fi

  apply_reqwest_trims
  apply_axum_trims
}
