#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

lock_file="${FLOW_VENDOR_LOCK_FILE:-vendor.lock.toml}"

usage() {
  cat <<'USAGE'
Usage:
  scripts/vendor/vendor-repo.sh <command> [args]

Commands:
  init                 Ensure vendor repo checkout exists and has base layout
  create-remote [slug] Create GitHub repo via gh and wire origin + lock repo URL
  import-local         Copy current lib/vendor + manifests into vendor repo and commit
  hydrate              Materialize lib/vendor from pinned commit in vendor.lock.toml
  pin [commit]         Pin vendor.lock.toml commit (defaults to checkout HEAD)
  status               Show lock/checkout/remote status summary
  push                 Push checkout HEAD to origin/<branch>

Environment:
  FLOW_VENDOR_LOCK_FILE  Override lock file path (default: vendor.lock.toml)
USAGE
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" || $# -lt 1 ]]; then
  usage
  exit 0
fi

command="$1"
shift || true

if [[ ! -f "$lock_file" ]]; then
  echo "error: missing lock file: $lock_file"
  exit 1
fi

read_lock_value() {
  local key="$1"
  awk -F'"' -v key="$key" '
    /^\/\// { next }
    /^\[/ { section = $0; next }
    section == "[flow_vendor]" && $1 ~ ("^" key " = ") { print $2; exit }
  ' "$lock_file"
}

list_crates() {
  awk -F'"' '
    BEGIN {
      in_crate = 0
      name = ""
      repo_path = ""
      manifest_path = ""
      materialized_path = ""
    }
    /^\[\[crate\]\]/ {
      if (in_crate && name != "") {
        printf "%s\t%s\t%s\t%s\n", name, repo_path, manifest_path, materialized_path
      }
      in_crate = 1
      name = ""
      repo_path = ""
      manifest_path = ""
      materialized_path = ""
      next
    }
    in_crate && $1 ~ /^name = / { name = $2; next }
    in_crate && $1 ~ /^repo_path = / { repo_path = $2; next }
    in_crate && $1 ~ /^manifest_path = / { manifest_path = $2; next }
    in_crate && $1 ~ /^materialized_path = / { materialized_path = $2; next }
    END {
      if (in_crate && name != "") {
        printf "%s\t%s\t%s\t%s\n", name, repo_path, manifest_path, materialized_path
      }
    }
  ' "$lock_file"
}

set_lock_commit() {
  local new_commit="$1"
  set_lock_value "commit" "$new_commit"
}

set_lock_value() {
  local key="$1"
  local new_value="$2"
  local tmp
  tmp="$(mktemp)"
  awk -v key="$key" -v new_value="$new_value" '
    BEGIN { in_vendor = 0; replaced = 0 }
    /^\[flow_vendor\]$/ { in_vendor = 1; print; next }
    /^\[/ {
      if (in_vendor == 1 && replaced == 0) {
        print key " = \"" new_value "\""
        replaced = 1
      }
      in_vendor = 0
    }
    in_vendor == 1 && $0 ~ ("^" key " = \"") {
      print key " = \"" new_value "\""
      replaced = 1
      next
    }
    { print }
    END {
      if (in_vendor == 1 && replaced == 0) {
        print key " = \"" new_value "\""
      }
    }
  ' "$lock_file" >"$tmp"
  mv "$tmp" "$lock_file"
}

ensure_checkout() {
  local repo_url branch checkout
  repo_url="$(read_lock_value repo)"
  branch="$(read_lock_value branch)"
  checkout="$(read_lock_value checkout)"

  if [[ -z "$repo_url" || -z "$branch" || -z "$checkout" ]]; then
    echo "error: lock file missing repo/branch/checkout in [flow_vendor]"
    exit 1
  fi

  if [[ -d "$checkout/.git" ]]; then
    echo "$checkout"
    return
  fi

  mkdir -p "$(dirname "$checkout")"
  if git clone "$repo_url" "$checkout" >/dev/null 2>&1; then
    echo "cloned $repo_url -> $checkout" >&2
  else
    https_url=""
    if [[ "$repo_url" =~ ^git@github\.com:(.+)\.git$ ]]; then
      https_url="https://github.com/${BASH_REMATCH[1]}.git"
    fi

    if [[ -n "$https_url" ]] && git clone "$https_url" "$checkout" >/dev/null 2>&1; then
      echo "cloned $https_url -> $checkout (fallback from SSH URL)" >&2
      git -C "$checkout" remote set-url origin "$repo_url" >/dev/null 2>&1 || true
    else
      echo "warning: failed to clone $repo_url" >&2
      echo "initializing local checkout at $checkout (set remote for later push)" >&2
      git init "$checkout" >/dev/null
      git -C "$checkout" checkout -q -B "$branch"
      git -C "$checkout" remote add origin "$repo_url"
    fi
  fi

  if ! git -C "$checkout" rev-parse --verify "$branch" >/dev/null 2>&1; then
    git -C "$checkout" checkout -q -B "$branch"
  else
    git -C "$checkout" checkout -q "$branch"
  fi

  echo "$checkout"
}

ensure_git_identity() {
  local checkout="$1"
  if ! git -C "$checkout" config user.email >/dev/null; then
    git -C "$checkout" config user.email "vendor-bot@localhost"
  fi
  if ! git -C "$checkout" config user.name >/dev/null; then
    git -C "$checkout" config user.name "vendor-bot"
  fi
}

ensure_repo_layout() {
  local checkout="$1"
  mkdir -p "$checkout/crates" "$checkout/manifests" "$checkout/profiles"

  if [[ ! -f "$checkout/README.md" ]]; then
    cat > "$checkout/README.md" <<'README'
# flow-vendor

Canonical vendored dependency source for `nikivdev/flow`.

- `crates/<crate>/`: vendored source trees used by Flow.
- `manifests/<crate>.toml`: upstream/version metadata per crate.
- `profiles/flow.toml`: crate list used by Flow hydration.
README
  fi
}

generate_flow_profile() {
  local output_file="$1"
  {
    echo "[profile]"
    echo "name = \"flow\""
    echo "generated_by = \"scripts/vendor/vendor-repo.sh\""
    echo
    while IFS=$'\t' read -r name repo_path manifest_path _materialized_path; do
      [[ -n "$name" ]] || continue
      echo "[[crate]]"
      echo "name = \"$name\""
      echo "repo_path = \"$repo_path\""
      echo "manifest_path = \"$manifest_path\""
      echo
    done < <(list_crates)
  } > "$output_file"
}

cmd_init() {
  local checkout
  checkout="$(ensure_checkout)"
  ensure_repo_layout "$checkout"
  generate_flow_profile "$checkout/profiles/flow.toml"
  echo "vendor checkout ready: $checkout"
}

cmd_create_remote() {
  local checkout slug ssh_url
  checkout="$(ensure_checkout)"
  slug="${1:-nikivdev/flow-vendor}"
  ssh_url="git@github.com:${slug}.git"

  if ! command -v gh >/dev/null 2>&1; then
    echo "error: gh CLI is required for create-remote"
    exit 1
  fi

  if gh repo view "$slug" >/dev/null 2>&1; then
    echo "remote repo exists: $slug"
  else
    gh repo create "$slug" --public --source "$checkout" --remote origin --disable-issues >/dev/null
    echo "created remote repo: $slug"
  fi

  if git -C "$checkout" remote get-url origin >/dev/null 2>&1; then
    git -C "$checkout" remote set-url origin "$ssh_url"
  else
    git -C "$checkout" remote add origin "$ssh_url"
  fi

  set_lock_value "repo" "$ssh_url"
  echo "updated lock repo URL to $ssh_url"
}

cmd_import_local() {
  local checkout
  checkout="$(ensure_checkout)"
  ensure_repo_layout "$checkout"
  ensure_git_identity "$checkout"

  while IFS=$'\t' read -r name repo_path manifest_path materialized_path; do
    [[ -n "$name" ]] || continue

    local_src="$repo_root/$materialized_path"
    local_manifest_src="$repo_root/lib/vendor-manifest/${name}.toml"

    if [[ ! -d "$local_src" ]]; then
      echo "warning: missing local vendored crate source: $local_src"
      continue
    fi

    mkdir -p "$checkout/$(dirname "$repo_path")"
    rm -rf "$checkout/$repo_path"
    mkdir -p "$checkout/$repo_path"
    rsync -a --delete --exclude '.git' "$local_src"/ "$checkout/$repo_path"/

    if [[ -f "$local_manifest_src" ]]; then
      mkdir -p "$checkout/$(dirname "$manifest_path")"
      cp "$local_manifest_src" "$checkout/$manifest_path"
    else
      echo "warning: missing local manifest: $local_manifest_src"
    fi
  done < <(list_crates)

  generate_flow_profile "$checkout/profiles/flow.toml"

  git -C "$checkout" add -A
  if git -C "$checkout" diff --cached --quiet; then
    echo "no changes to import into vendor repo"
  else
    git -C "$checkout" commit -m "vendor(flow): import local materialized crates" >/dev/null
    echo "committed vendor repo import"
  fi

  head_sha="$(git -C "$checkout" rev-parse HEAD)"
  set_lock_commit "$head_sha"
  echo "pinned $lock_file commit=$head_sha"
}

cmd_hydrate() {
  local checkout commit
  checkout="$(ensure_checkout)"
  commit="$(read_lock_value commit)"

  if [[ -z "$commit" ]]; then
    commit="$(git -C "$checkout" rev-parse HEAD)"
    echo "warning: lock commit empty; hydrating from checkout HEAD $commit"
  fi

  if git -C "$checkout" remote get-url origin >/dev/null 2>&1; then
    git -C "$checkout" fetch -q origin "$(read_lock_value branch)" >/dev/null 2>&1 || true
  fi

  if ! git -C "$checkout" cat-file -e "${commit}^{commit}" 2>/dev/null; then
    echo "error: commit $commit not found in $checkout"
    echo "hint: run scripts/vendor/vendor-repo.sh init (or pin a commit present locally)"
    exit 1
  fi

  while IFS=$'\t' read -r name repo_path manifest_path materialized_path; do
    [[ -n "$name" ]] || continue

    dst_src="$repo_root/$materialized_path"
    dst_manifest="$repo_root/lib/vendor-manifest/${name}.toml"

    if ! git -C "$checkout" cat-file -e "${commit}:${repo_path}" 2>/dev/null; then
      echo "error: crate path missing at pinned commit: ${repo_path}"
      exit 1
    fi

    tmp_dir="$(mktemp -d)"

    git -C "$checkout" archive --format=tar "$commit" "$repo_path" | tar -xf - -C "$tmp_dir"
    rm -rf "$dst_src"
    mkdir -p "$dst_src"
    rsync -a --delete "$tmp_dir/$repo_path"/ "$dst_src"/

    if git -C "$checkout" cat-file -e "${commit}:${manifest_path}" 2>/dev/null; then
      mkdir -p "$(dirname "$dst_manifest")"
      git -C "$checkout" show "${commit}:${manifest_path}" > "$dst_manifest"
    fi

    scripts/vendor/apply-trims.sh "$name"

    rm -rf "$tmp_dir"

    echo "hydrated $name -> $materialized_path"
  done < <(list_crates)
}

cmd_pin() {
  local checkout commit
  checkout="$(ensure_checkout)"
  commit="${1:-}"
  if [[ -z "$commit" ]]; then
    commit="$(git -C "$checkout" rev-parse HEAD)"
  fi
  if ! git -C "$checkout" cat-file -e "${commit}^{commit}" 2>/dev/null; then
    echo "error: commit does not exist in checkout: $commit"
    exit 1
  fi
  set_lock_commit "$commit"
  echo "pinned $lock_file commit=$commit"
}

cmd_status() {
  local repo_url branch checkout commit
  repo_url="$(read_lock_value repo)"
  branch="$(read_lock_value branch)"
  checkout="$(read_lock_value checkout)"
  commit="$(read_lock_value commit)"

  echo "lock_file: $lock_file"
  echo "repo:      $repo_url"
  echo "branch:    $branch"
  echo "checkout:  $checkout"
  echo "pinned:    ${commit:-<empty>}"

  if [[ -d "$checkout/.git" ]]; then
    local head_sha
    if head_sha="$(git -C "$checkout" rev-parse --verify HEAD 2>/dev/null)"; then
      echo "head:      $head_sha"
    else
      echo "head:      <no commits yet>"
    fi

    if git -C "$checkout" remote get-url origin >/dev/null 2>&1; then
      if git -C "$checkout" fetch -q origin "$branch" >/dev/null 2>&1; then
        :
      else
        echo "origin:    unreachable (fetch failed)"
      fi
      if git -C "$checkout" rev-parse --verify "origin/$branch" >/dev/null 2>&1; then
        local counts
        counts="$(git -C "$checkout" rev-list --left-right --count "origin/$branch...HEAD")"
        echo "origin:    origin/$branch ($counts: behind ahead)"
      fi
    fi
  else
    echo "head:      <checkout missing>"
  fi

  echo
  echo "crates:"
  while IFS=$'\t' read -r name repo_path manifest_path materialized_path; do
    [[ -n "$name" ]] || continue
    local_exists="no"
    [[ -d "$repo_root/$materialized_path" ]] && local_exists="yes"
    echo "- $name"
    echo "  repo_path:        $repo_path"
    echo "  manifest_path:    $manifest_path"
    echo "  materialized:     $materialized_path (exists: $local_exists)"
  done < <(list_crates)
}

cmd_push() {
  local checkout branch
  checkout="$(ensure_checkout)"
  branch="$(read_lock_value branch)"

  if [[ -z "$(git -C "$checkout" status --porcelain)" ]]; then
    :
  else
    echo "error: checkout has uncommitted changes; commit before push"
    exit 1
  fi

  git -C "$checkout" push origin "HEAD:${branch}"
  echo "pushed ${checkout} HEAD -> origin/${branch}"
}

case "$command" in
  init) cmd_init "$@" ;;
  create-remote) cmd_create_remote "$@" ;;
  import-local) cmd_import_local "$@" ;;
  hydrate) cmd_hydrate "$@" ;;
  pin) cmd_pin "$@" ;;
  status) cmd_status "$@" ;;
  push) cmd_push "$@" ;;
  *)
    usage
    exit 1
    ;;
esac
