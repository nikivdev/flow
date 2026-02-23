#!/usr/bin/env bash
set -euo pipefail

RUN_ROOT="${RUN_ROOT:-$HOME/code/run}"

usage() {
  cat <<'USAGE'
Usage:
  run-repos.sh root
  run-repos.sh ensure
  run-repos.sh list
  run-repos.sh load <name> <repo-ssh-url> [branch]
  run-repos.sh sync [name]
  run-repos.sh task <name> <flow-task> [args...]
  run-repos.sh exec <name> <repo-ssh-url> [--branch <branch>] <flow-task> [args...]

Environment:
  RUN_ROOT              Run repo root (default: ~/code/run)
  RUN_AUTO_SYNC         If set to 1, run-repos.sh task auto-syncs git repos before running task
USAGE
}

ensure_root() {
  mkdir -p "$RUN_ROOT"
}

repo_dir() {
  local name="$1"
  printf '%s/%s' "$RUN_ROOT" "$name"
}

is_git_repo() {
  local dir="$1"
  [ -d "$dir/.git" ]
}

sync_git_repo() {
  local dir="$1"
  if ! is_git_repo "$dir"; then
    echo "[run] skip sync (not git): $dir"
    return 0
  fi

  local branch=""
  branch="$(git -C "$dir" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"

  echo "[run] syncing: $dir"
  git -C "$dir" fetch --all --prune

  if [ -n "$branch" ] && git -C "$dir" show-ref --verify --quiet "refs/remotes/origin/$branch"; then
    git -C "$dir" pull --ff-only origin "$branch"
  else
    git -C "$dir" pull --ff-only || true
  fi
}

cmd_root() {
  echo "$RUN_ROOT"
}

cmd_ensure() {
  ensure_root
  echo "[run] root ready: $RUN_ROOT"
}

cmd_list() {
  ensure_root

  local has_any=0
  for dir in "$RUN_ROOT"/*; do
    [ -d "$dir" ] || continue
    [ -f "$dir/flow.toml" ] || continue
    has_any=1
    local name="$(basename "$dir")"
    if is_git_repo "$dir"; then
      local remote=""
      local branch=""
      remote="$(git -C "$dir" remote get-url origin 2>/dev/null || true)"
      branch="$(git -C "$dir" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
      echo "$name | git | ${branch:-?} | ${remote:-no-origin} | $dir"
    else
      echo "$name | no-git | - | - | $dir"
    fi
  done

  if [ "$has_any" -eq 0 ]; then
    echo "[run] no run repos found in $RUN_ROOT"
  fi
}

cmd_load() {
  if [ "$#" -lt 2 ]; then
    echo "ERROR: load requires <name> <repo-ssh-url> [branch]"
    usage
    exit 1
  fi

  local name="$1"
  local repo_url="$2"
  local branch="${3:-}"
  local dir
  dir="$(repo_dir "$name")"

  ensure_root

  if [ -e "$dir" ] && ! [ -d "$dir" ]; then
    echo "ERROR: target exists and is not a directory: $dir"
    exit 1
  fi

  if is_git_repo "$dir"; then
    echo "[run] already loaded: $name ($dir)"
    sync_git_repo "$dir"
    return 0
  fi

  if [ -d "$dir" ] && [ ! -d "$dir/.git" ]; then
    echo "ERROR: directory exists but is not a git repo: $dir"
    echo "Remove it manually or choose another run repo name."
    exit 1
  fi

  if [ -n "$branch" ]; then
    echo "[run] cloning $repo_url (branch: $branch) -> $dir"
    git clone --branch "$branch" "$repo_url" "$dir"
  else
    echo "[run] cloning $repo_url -> $dir"
    git clone "$repo_url" "$dir"
  fi

  if [ ! -f "$dir/flow.toml" ]; then
    echo "WARN: cloned repo has no flow.toml: $dir"
  fi
}

cmd_sync() {
  ensure_root

  if [ "$#" -gt 0 ]; then
    local name="$1"
    local dir
    dir="$(repo_dir "$name")"
    if [ ! -d "$dir" ]; then
      echo "ERROR: run repo not found: $dir"
      exit 1
    fi
    sync_git_repo "$dir"
    return 0
  fi

  local found=0
  for dir in "$RUN_ROOT"/*; do
    [ -d "$dir" ] || continue
    if is_git_repo "$dir"; then
      found=1
      sync_git_repo "$dir"
    fi
  done

  if [ "$found" -eq 0 ]; then
    echo "[run] no git run repos to sync in $RUN_ROOT"
  fi
}

cmd_task() {
  if [ "$#" -lt 2 ]; then
    echo "ERROR: task requires <name> <flow-task> [args...]"
    usage
    exit 1
  fi

  local name="$1"
  shift
  local dir
  dir="$(repo_dir "$name")"

  if [ ! -d "$dir" ]; then
    echo "ERROR: run repo not found: $dir"
    exit 1
  fi

  if [ ! -f "$dir/flow.toml" ]; then
    echo "ERROR: run repo has no flow.toml: $dir"
    exit 1
  fi

  if [ "${RUN_AUTO_SYNC:-0}" = "1" ] && is_git_repo "$dir"; then
    sync_git_repo "$dir"
  fi

  echo "[run] $name -> f $*"
  (
    cd "$dir"
    f "$@"
  )
}

cmd_exec() {
  if [ "$#" -lt 3 ]; then
    echo "ERROR: exec requires <name> <repo-ssh-url> [--branch <branch>] <flow-task> [args...]"
    usage
    exit 1
  fi

  local name="$1"
  local repo_url="$2"
  shift 2

  local branch=""
  if [ "${1:-}" = "--branch" ]; then
    branch="${2:-}"
    if [ -z "$branch" ]; then
      echo "ERROR: --branch requires a value"
      usage
      exit 1
    fi
    shift 2
  fi

  if [ "$#" -lt 1 ]; then
    echo "ERROR: exec requires a flow task after repo parameters"
    usage
    exit 1
  fi

  local dir
  dir="$(repo_dir "$name")"
  if [ -d "$dir" ] && [ -f "$dir/flow.toml" ] && ! is_git_repo "$dir"; then
    echo "[run] using existing run task directory (non-git): $dir"
  else
    if [ -n "$branch" ]; then
      cmd_load "$name" "$repo_url" "$branch"
    else
      cmd_load "$name" "$repo_url"
    fi
  fi

  cmd_task "$name" "$@"
}

main() {
  local cmd="${1:-help}"
  shift || true

  case "$cmd" in
    root) cmd_root "$@" ;;
    ensure) cmd_ensure "$@" ;;
    list) cmd_list "$@" ;;
    load) cmd_load "$@" ;;
    sync) cmd_sync "$@" ;;
    task) cmd_task "$@" ;;
    exec) cmd_exec "$@" ;;
    help|-h|--help) usage ;;
    *)
      echo "ERROR: unknown command: $cmd"
      usage
      exit 1
      ;;
  esac
}

main "$@"
