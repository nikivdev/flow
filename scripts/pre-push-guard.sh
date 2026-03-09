#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

remote_name="${1:-origin}"
zero_sha="0000000000000000000000000000000000000000"
should_verify_pinned_origin=0

range_changes_vendor_lock() {
  local local_ref="$1"
  local local_sha="$2"
  local remote_ref="$3"
  local remote_sha="$4"
  local branch_name=""
  local base_ref=""

  [[ "$local_ref" == refs/heads/* ]] || return 1
  [[ "$local_sha" != "$zero_sha" ]] || return 1

  if [[ "$remote_sha" != "$zero_sha" ]] && git cat-file -e "${remote_sha}^{commit}" 2>/dev/null; then
    base_ref="$remote_sha"
  elif [[ "$remote_ref" == refs/heads/* ]]; then
    branch_name="${remote_ref#refs/heads/}"
    if git rev-parse --verify "refs/remotes/$remote_name/$branch_name" >/dev/null 2>&1; then
      base_ref="refs/remotes/$remote_name/$branch_name"
    fi
  fi

  if [[ -z "$base_ref" ]] && git rev-parse --verify "refs/remotes/$remote_name/main" >/dev/null 2>&1; then
    base_ref="$(git merge-base "$local_sha" "refs/remotes/$remote_name/main" 2>/dev/null || true)"
  fi

  if [[ -n "$base_ref" ]]; then
    git diff --quiet "$base_ref...$local_sha" -- vendor.lock.toml
    return $?
  fi

  # Fallback for brand-new remotes/branches with no useful remote base yet.
  git diff-tree --quiet --no-commit-id -r "$local_sha" -- vendor.lock.toml
}

while read -r local_ref local_sha remote_ref remote_sha; do
  [[ -n "${local_ref:-}" ]] || continue
  if ! range_changes_vendor_lock "$local_ref" "$local_sha" "$remote_ref" "$remote_sha"; then
    should_verify_pinned_origin=1
    break
  fi
done

if [[ "$should_verify_pinned_origin" == "1" ]]; then
  echo "pre-push: vendor.lock.toml changed in pushed refs; verifying pinned vendor commit is published"
  "$repo_root/scripts/vendor/vendor-repo.sh" verify-pinned-origin
fi
