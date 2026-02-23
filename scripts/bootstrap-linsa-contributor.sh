#!/usr/bin/env bash
set -euo pipefail

FLOW_DIR="${FLOW_DIR:-$HOME/code/flow}"
FLOW_REPO_URL="${FLOW_REPO_URL:-git@github.com:nikivdev/flow.git}"
FLOW_BRANCH="${FLOW_BRANCH:-main}"
RUN_REPO_URL="${RUN_REPO_URL:-git@github.com:nikivdev/run.git}"
FLOW_BIN="${FLOW_BIN:-$HOME/.flow/bin/f}"

can_run_flow() {
  [ -x "$FLOW_BIN" ] && "$FLOW_BIN" --version >/dev/null 2>&1
}

sync_or_clone_flow_repo() {
  mkdir -p "$(dirname "$FLOW_DIR")"
  if [ -d "$FLOW_DIR/.git" ]; then
    # Keep this tolerant for dirty local trees.
    (
      cd "$FLOW_DIR"
      git fetch --all --prune
      if git show-ref --verify --quiet "refs/remotes/origin/$FLOW_BRANCH"; then
        git checkout "$FLOW_BRANCH" || true
        git pull --ff-only origin "$FLOW_BRANCH" || true
      fi
    ) || true
  elif [ -e "$FLOW_DIR" ]; then
    echo "ERROR: $FLOW_DIR exists but is not a git repository"
    exit 1
  else
    git clone --branch "$FLOW_BRANCH" "$FLOW_REPO_URL" "$FLOW_DIR"
  fi
}

ensure_working_flow_bin() {
  if can_run_flow; then
    return 0
  fi

  sync_or_clone_flow_repo
  sh "$FLOW_DIR/install.sh"
  chmod +x "$FLOW_BIN" 2>/dev/null || true

  if ! can_run_flow; then
    echo "ERROR: Flow CLI is not executable at $FLOW_BIN"
    ls -l "$FLOW_BIN" 2>/dev/null || true
    file "$FLOW_BIN" 2>/dev/null || true
    exit 1
  fi
}

ensure_working_flow_bin
"$FLOW_BIN" run-linsa-bootstrap "$RUN_REPO_URL"
