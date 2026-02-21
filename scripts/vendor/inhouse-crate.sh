#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/inhouse-crate.sh <crate> [version]

Examples:
  scripts/vendor/inhouse-crate.sh reqwest
  scripts/vendor/inhouse-crate.sh reqwest 0.12.24

Behavior:
  - Pulls crate source from local Cargo registry cache.
  - Commits snapshot into per-crate git history at lib/vendor-history/<crate>.git.
  - Materializes working copy into lib/vendor/<crate> for Cargo path patches.
  - Writes lib/vendor-manifest/<crate>.toml metadata for sync tracking.
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit 1
fi

crate="$1"
version="${2:-}"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

vendor_root="lib/vendor"
history_root="lib/vendor-history"
manifest_root="lib/vendor-manifest"

resolve_version_from_lock() {
  awk -v crate="$crate" '
    BEGIN { name = ""; version = ""; source = "" }
    $0 == "[[package]]" {
      if (name == crate && source ~ /^registry\+/) {
        registry_versions[version] = 1
      }
      if (name == crate && version != "") {
        any_versions[version] = 1
      }
      name = ""
      version = ""
      source = ""
      next
    }
    /^name = "/ {
      name = $3
      gsub(/"/, "", name)
      next
    }
    /^version = "/ {
      version = $3
      gsub(/"/, "", version)
      next
    }
    /^source = "/ {
      source = $3
      gsub(/"/, "", source)
      next
    }
    END {
      if (name == crate && source ~ /^registry\+/) {
        registry_versions[version] = 1
      }
      if (name == crate && version != "") {
        any_versions[version] = 1
      }
      for (v in registry_versions) print v
      if (length(registry_versions) == 0) {
        for (v in any_versions) print v
      }
    }
  ' Cargo.lock | sort -V | tail -n 1
}

if [[ -z "$version" ]]; then
  version="$(resolve_version_from_lock)"
fi

if [[ -z "$version" ]]; then
  echo "error: could not resolve registry version for crate '$crate'"
  echo "hint: pass an explicit version: scripts/vendor/inhouse-crate.sh $crate <version>"
  exit 1
fi

src_dir="$({
  find "$HOME/.cargo/registry/src" -maxdepth 2 -type d -name "${crate}-${version}" 2>/dev/null \
    | head -n 1
} || true)"

if [[ -z "$src_dir" ]]; then
  echo "error: could not find ${crate}-${version} in cargo cache"
  echo "hint: run 'cargo fetch -p ${crate}@${version}' in a clean Cargo state and retry"
  exit 1
fi

mkdir -p "$history_root" "$vendor_root" "$manifest_root"

history_repo_rel="${history_root}/${crate}.git"
history_repo_abs="${repo_root}/${history_repo_rel}"
if [[ ! -d "$history_repo_abs" ]]; then
  git init --bare "$history_repo_abs" >/dev/null
fi

tmp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmp_dir"
}
trap cleanup EXIT

checkout_dir="${tmp_dir}/${crate}"
git init "$checkout_dir" >/dev/null

git -C "$checkout_dir" remote add origin "$history_repo_abs"
if git -C "$checkout_dir" ls-remote --exit-code --heads origin main >/dev/null 2>&1; then
  git -C "$checkout_dir" fetch -q origin main
  git -C "$checkout_dir" checkout -q -B main FETCH_HEAD
else
  git -C "$checkout_dir" checkout -q -B main
fi

# Keep script usable on fresh machines without requiring global git identity.
if ! git -C "$checkout_dir" config user.email >/dev/null; then
  git -C "$checkout_dir" config user.email "vendor-bot@localhost"
fi
if ! git -C "$checkout_dir" config user.name >/dev/null; then
  git -C "$checkout_dir" config user.name "vendor-bot"
fi

find "$checkout_dir" -mindepth 1 -maxdepth 1 ! -name '.git' -exec rm -rf {} +

rsync -a \
  --delete \
  --exclude '.git' \
  --exclude '.cargo-ok' \
  --exclude '.cargo_vcs_info.json' \
  --exclude 'target' \
  "$src_dir"/ "$checkout_dir"/

synced_at_utc="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
git -C "$checkout_dir" add -A

if git -C "$checkout_dir" diff --cached --quiet; then
  commit_state="no-op"
else
  git -C "$checkout_dir" commit -m "sync(${crate}): crates.io ${version}" >/dev/null
  commit_state="committed"
fi

git -C "$checkout_dir" push -q -u origin main

git -C "$checkout_dir" tag -f "v${version}" >/dev/null
git -C "$checkout_dir" push -q -f origin "refs/tags/v${version}"

history_head="$(git -C "$checkout_dir" rev-parse HEAD)"

dest_dir_rel="${vendor_root}/${crate}"
dest_dir_abs="${repo_root}/${dest_dir_rel}"
rm -rf "$dest_dir_abs"
mkdir -p "$dest_dir_abs"
rsync -a \
  --delete \
  --exclude '.git' \
  "$checkout_dir"/ "$dest_dir_abs"/

manifest_file="${manifest_root}/${crate}.toml"
cat > "$manifest_file" <<MANIFEST
crate = "${crate}"
version = "${version}"
source = "crates.io"
synced_at_utc = "${synced_at_utc}"
history_repo = "${history_repo_rel}"
history_head = "${history_head}"
materialized_path = "${dest_dir_rel}"
sync_cmd = "scripts/vendor/inhouse-crate.sh ${crate} ${version}"
MANIFEST

# Compatibility metadata in materialized copy for quick local inspection.
cat > "${dest_dir_abs}/UPSTREAM.toml" <<UPSTREAM
crate = "${crate}"
version = "${version}"
source = "crates.io"
synced_at_utc = "${synced_at_utc}"
history_repo = "${history_repo_rel}"
history_head = "${history_head}"
sync_cmd = "scripts/vendor/inhouse-crate.sh ${crate} ${version}"
UPSTREAM

echo "inhouse ${crate}@${version} -> ${dest_dir_rel} (history: ${history_repo_rel}, ${commit_state})"
