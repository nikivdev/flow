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
registry_index="https://github.com/rust-lang/crates.io-index"

find_cached_src_dir() {
  find "$HOME/.cargo/registry/src" -maxdepth 2 -type d -name "${crate}-${version}" 2>/dev/null \
    | head -n 1
}

find_cached_crate_file() {
  find "$HOME/.cargo/registry/cache" -maxdepth 2 -type f -name "${crate}-${version}.crate" 2>/dev/null \
    | head -n 1
}

fetch_into_cache() {
  local fetch_tmp
  fetch_tmp="$(mktemp -d)"
  cat > "${fetch_tmp}/Cargo.toml" <<EOF
[package]
name = "vendor-fetch-${crate}"
version = "0.0.0"
edition = "2021"

[dependencies]
${crate} = "= ${version}"
EOF
  mkdir -p "${fetch_tmp}/src"
  printf '%s\n' 'fn main() {}' > "${fetch_tmp}/src/main.rs"

  cargo fetch --manifest-path "${fetch_tmp}/Cargo.toml" >/dev/null 2>&1 || true
  rm -rf "$fetch_tmp"
}

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

resolve_checksum_from_lock() {
  awk -v crate="$crate" -v version="$version" '
    BEGIN { name = ""; ver = ""; checksum = ""; found = 0 }
    $0 == "[[package]]" {
      if (name == crate && ver == version && checksum != "") {
        print checksum
        found = 1
        exit 0
      }
      name = ""
      ver = ""
      checksum = ""
      next
    }
    /^name = "/ {
      name = $3
      gsub(/"/, "", name)
      next
    }
    /^version = "/ {
      ver = $3
      gsub(/"/, "", ver)
      next
    }
    /^checksum = "/ {
      checksum = $3
      gsub(/"/, "", checksum)
      next
    }
    END {
      if (found == 0 && name == crate && ver == version && checksum != "") {
        print checksum
      }
    }
  ' Cargo.lock
}

resolve_checksum_from_crates_io() {
  if ! command -v curl >/dev/null 2>&1 || ! command -v jq >/dev/null 2>&1; then
    return 0
  fi
  curl -fsSL "https://crates.io/api/v1/crates/${crate}/${version}" 2>/dev/null \
    | jq -r '.version.checksum // empty' 2>/dev/null \
    || true
}

extract_toml_string() {
  local file="$1"
  local key="$2"
  awk -F'"' -v key="$key" '$1 ~ "^" key " = " { print $2; exit }' "$file"
}

sha256_file() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{print $1}'
  else
    echo ""
  fi
}

if [[ -z "$version" ]]; then
  version="$(resolve_version_from_lock)"
fi

if [[ -z "$version" ]]; then
  echo "error: could not resolve registry version for crate '$crate'"
  echo "hint: pass an explicit version: scripts/vendor/inhouse-crate.sh $crate <version>"
  exit 1
fi

src_dir="$(find_cached_src_dir || true)"

if [[ -z "$src_dir" ]]; then
  fetch_into_cache
  src_dir="$(find_cached_src_dir || true)"
  if [[ -z "$src_dir" ]]; then
    echo "error: could not find ${crate}-${version} in cargo cache after auto-fetch"
    echo "hint: check network/cargo registry config, then retry"
    exit 1
  fi
fi

crate_archive_file="$(find_cached_crate_file || true)"

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

upstream_repository="$(extract_toml_string "$src_dir/Cargo.toml" "repository")"
upstream_homepage="$(extract_toml_string "$src_dir/Cargo.toml" "homepage")"
registry_checksum="$(resolve_checksum_from_lock)"
if [[ -z "$registry_checksum" ]]; then
  registry_checksum="$(resolve_checksum_from_crates_io)"
fi
registry_checksum="$(printf '%s' "$registry_checksum" | head -n 1 | tr -d '\r\n')"
archive_sha256=""
if [[ -n "$crate_archive_file" ]]; then
  archive_sha256="$(sha256_file "$crate_archive_file")"
fi
archive_sha256="$(printf '%s' "$archive_sha256" | tr -d '\r\n')"
checksum_match="unknown"
if [[ -n "$registry_checksum" && -n "$archive_sha256" ]]; then
  if [[ "$registry_checksum" == "$archive_sha256" ]]; then
    checksum_match="yes"
  else
    checksum_match="no"
  fi
fi

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
registry_index = "${registry_index}"
cargo_registry_checksum = "${registry_checksum}"
crate_archive_sha256 = "${archive_sha256}"
checksum_match = "${checksum_match}"
upstream_repository = "${upstream_repository}"
upstream_homepage = "${upstream_homepage}"
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
registry_index = "${registry_index}"
cargo_registry_checksum = "${registry_checksum}"
crate_archive_sha256 = "${archive_sha256}"
checksum_match = "${checksum_match}"
upstream_repository = "${upstream_repository}"
upstream_homepage = "${upstream_homepage}"
synced_at_utc = "${synced_at_utc}"
history_repo = "${history_repo_rel}"
history_head = "${history_head}"
sync_cmd = "scripts/vendor/inhouse-crate.sh ${crate} ${version}"
UPSTREAM

echo "inhouse ${crate}@${version} -> ${dest_dir_rel} (history: ${history_repo_rel}, ${commit_state})"
