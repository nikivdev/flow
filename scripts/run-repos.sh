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
  run-repos.sh r <flow-task> [args...]
  run-repos.sh ri <flow-task> [args...]
  run-repos.sh rp <project> <flow-task> [args...]
  run-repos.sh rip <project> <flow-task> [args...]
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

validate_relative_path() {
  local rel="$1"
  local label="$2"
  if [ -z "$rel" ]; then
    echo "ERROR: $label cannot be empty"
    exit 1
  fi
  case "$rel" in
    /*)
      echo "ERROR: $label must be relative to \$RUN_ROOT (got absolute path: $rel)"
      exit 1
      ;;
    ..|../*|*/..|*/../*)
      echo "ERROR: $label must not contain '..' segments: $rel"
      exit 1
      ;;
  esac
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

print_repo_row() {
  local name="$1"
  local dir="$2"
  if is_git_repo "$dir"; then
    local remote=""
    local branch=""
    remote="$(git -C "$dir" remote get-url origin 2>/dev/null || true)"
    branch="$(git -C "$dir" rev-parse --abbrev-ref HEAD 2>/dev/null || true)"
    echo "$name | git | ${branch:-?} | ${remote:-no-origin} | $dir"
  else
    echo "$name | no-git | - | - | $dir"
  fi
}

display_name_for_dir() {
  local dir="$1"
  if [ "$dir" = "$RUN_ROOT" ]; then
    printf 'root'
    return 0
  fi
  printf '%s' "${dir#$RUN_ROOT/}"
}

run_task_in_dir() {
  local dir="$1"
  local label="$2"
  shift 2

  if [ ! -d "$dir" ]; then
    echo "ERROR: run repo/project not found: $dir"
    exit 1
  fi

  if [ ! -f "$dir/flow.toml" ]; then
    echo "ERROR: no flow.toml in: $dir"
    exit 1
  fi

  if [ "${RUN_AUTO_SYNC:-0}" = "1" ] && is_git_repo "$dir"; then
    sync_git_repo "$dir"
  fi

  local config_path="$dir/flow.toml"
  echo "[run] $label -> f run --config $config_path $*"
  (
    cd "$dir"
    f run --config "$config_path" "$@"
  )
}

resolve_project_dir() {
  local project="$1"
  validate_relative_path "$project" "project"

  local direct
  local internal
  direct="$(repo_dir "$project")"
  internal="$(repo_dir "i/$project")"

  if [ "$project" != i/* ] && [ -d "$direct" ] && [ -d "$internal" ]; then
    echo "ERROR: project '$project' is ambiguous."
    echo "Use explicit path: 'i/$project' for internal, or '$project' for public."
    exit 1
  fi

  if [ -d "$direct" ]; then
    printf '%s\n' "$direct"
    return 0
  fi

  if [ "$project" != i/* ] && [ -d "$internal" ]; then
    printf '%s\n' "$internal"
    return 0
  fi

  echo "ERROR: project not found under \$RUN_ROOT:"
  echo "  tried: $direct"
  if [ "$project" != i/* ]; then
    echo "  tried: $internal"
  fi
  exit 1
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
  if [ -f "$RUN_ROOT/flow.toml" ]; then
    print_repo_row "root" "$RUN_ROOT"
    has_any=1
  fi

  while IFS= read -r toml; do
    local dir
    local name
    dir="$(dirname "$toml")"
    [ "$dir" = "$RUN_ROOT" ] && continue
    name="${dir#$RUN_ROOT/}"
    print_repo_row "$name" "$dir"
    has_any=1
  done < <(find "$RUN_ROOT" -mindepth 1 -maxdepth 6 -type f -name flow.toml 2>/dev/null | sort)

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
  validate_relative_path "$name" "name"
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
    validate_relative_path "$name" "name"
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
  while IFS= read -r git_dir; do
    [ -n "$git_dir" ] || continue
    local repo
    repo="$(dirname "$git_dir")"
    found=1
    sync_git_repo "$repo"
  done < <(find "$RUN_ROOT" -type d -name .git -prune 2>/dev/null | sort)

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
  validate_relative_path "$name" "name"
  local dir
  dir="$(repo_dir "$name")"

  run_task_in_dir "$dir" "$name" "$@"
}

cmd_ri() {
  # Shortcut: run task in $RUN_ROOT/i
  cmd_task i "$@"
}

cmd_r() {
  # Shortcut: run task in $RUN_ROOT (the public run repo itself)
  if [ "$#" -lt 1 ]; then
    echo "ERROR: r requires <flow-task> [args...]"
    exit 1
  fi
  ensure_root
  run_task_in_dir "$RUN_ROOT" "root" "$@"
}

cmd_rp() {
  # Run a task in a run project by path/name (resolves internal fallback).
  if [ "$#" -lt 2 ]; then
    echo "ERROR: rp requires <project> <flow-task> [args...]"
    usage
    exit 1
  fi
  local project="$1"
  shift
  local dir
  local label
  dir="$(resolve_project_dir "$project")"
  label="$(display_name_for_dir "$dir")"
  run_task_in_dir "$dir" "$label" "$@"
}

cmd_rip() {
  # Run a task in an internal run project: $RUN_ROOT/i/<project>.
  if [ "$#" -lt 2 ]; then
    echo "ERROR: rip requires <project> <flow-task> [args...]"
    usage
    exit 1
  fi
  local project="$1"
  shift
  validate_relative_path "$project" "project"
  local dir
  dir="$(repo_dir "i/$project")"
  run_task_in_dir "$dir" "i/$project" "$@"
}

cmd_exec() {
  if [ "$#" -lt 3 ]; then
    echo "ERROR: exec requires <name> <repo-ssh-url> [--branch <branch>] <flow-task> [args...]"
    usage
    exit 1
  fi

  local name="$1"
  validate_relative_path "$name" "name"
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
    if is_git_repo "$RUN_ROOT"; then
      echo "[run] syncing monorepo root: $RUN_ROOT"
      if ! sync_git_repo "$RUN_ROOT"; then
        echo "[run] WARN: failed to sync monorepo root; using local checkout"
      fi
    fi
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
    ri) cmd_ri "$@" ;;
    r) cmd_r "$@" ;;
    rp) cmd_rp "$@" ;;
    rip) cmd_rip "$@" ;;
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
