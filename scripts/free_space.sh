#!/usr/bin/env bash

set -euo pipefail

TARGET_GB="${FLOW_SPACE_TARGET_GB:-100}"
APPLY=false

usage() {
  cat <<'EOF'
Usage: f free-space [--apply] [--target-gb N]

Default mode is dry-run. Use --apply to perform the cleanup.

This task runs three Mole flows in order:
  1. installer
  2. purge
  3. clean

Examples:
  f free-space
  f free-space --apply
  f free-space --target-gb 120
EOF
}

trim() {
  local value="$1"
  value="${value#"${value%%[![:space:]]*}"}"
  value="${value%"${value##*[![:space:]]}"}"
  printf '%s\n' "$value"
}

find_mole_bin() {
  local -a candidates=()

  if command -v mo >/dev/null 2>&1; then
    candidates+=("$(command -v mo)")
  fi

  candidates+=(
    "$HOME/repos/tw93/mole/mo"
    "$HOME/repos/nikivdev/mole-i/mo"
    "$HOME/code/mole/mo"
  )

  local candidate=""
  for candidate in "${candidates[@]}"; do
    [[ -n "$candidate" && -x "$candidate" ]] || continue
    printf '%s\n' "$candidate"
    return 0
  done

  return 1
}

free_kb() {
  df -Pk / | awk 'NR==2 {print $4}'
}

free_gb_int() {
  local kb
  kb="$(free_kb)"
  printf '%s\n' "$((kb / 1024 / 1024))"
}

free_human() {
  df -h / | awk 'NR==2 {print $4}'
}

purge_config_has_path() {
  local wanted="${1%/}"
  local config_file="$HOME/.config/mole/purge_paths"

  [[ -f "$config_file" ]] || return 1

  local line=""
  while IFS= read -r line; do
    line="$(trim "$line")"
    [[ -n "$line" && "${line:0:1}" != "#" ]] || continue
    line="${line/#\~/$HOME}"
    [[ "${line%/}" == "$wanted" ]] && return 0
  done < "$config_file"

  return 1
}

print_candidate_sizes() {
  local -a paths=(
    "$HOME/Downloads"
    "$HOME/.Trash"
    "$HOME/Library/Caches"
    "$HOME/Library/Caches/Homebrew"
    "$HOME/Library/Developer/Xcode/DerivedData"
    "$HOME/Library/Developer/CoreSimulator/Devices"
    "$HOME/code"
    "$HOME/repos"
  )

  echo "Likely large locations:"
  local path=""
  for path in "${paths[@]}"; do
    [[ -e "$path" ]] || continue
    du -sh "$path" 2>/dev/null || true
  done
  echo ""
}

run_step() {
  local title="$1"
  shift

  echo "== $title =="
  printf 'Command:'
  printf ' %q' "$@"
  printf '\n\n'
  "$@"
  echo ""
  echo "Free space now: $(free_human)"
  echo ""
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --apply)
      APPLY=true
      ;;
    --target-gb)
      shift
      if [[ $# -eq 0 ]] || [[ ! "$1" =~ ^[0-9]+$ ]]; then
        echo "error: --target-gb requires an integer" >&2
        exit 1
      fi
      TARGET_GB="$1"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown arg: $1" >&2
      echo "" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

if ! [[ "$TARGET_GB" =~ ^[0-9]+$ ]]; then
  echo "error: target must be an integer number of GB" >&2
  exit 1
fi

MOLE_BIN="$(find_mole_bin || true)"
if [[ -z "$MOLE_BIN" ]]; then
  echo "error: could not find Mole. Install it or keep the repo at ~/repos/tw93/mole." >&2
  exit 1
fi

CURRENT_FREE_GB="$(free_gb_int)"
CURRENT_FREE_HUMAN="$(free_human)"
MODE_LABEL="dry-run"
if [[ "$APPLY" == "true" ]]; then
  MODE_LABEL="apply"
fi

echo "Mac Free Space Helper"
echo "Mode: $MODE_LABEL"
echo "Target free space: ${TARGET_GB}GB"
echo "Current free space: ${CURRENT_FREE_GB}GB (${CURRENT_FREE_HUMAN})"
echo "Mole binary: $MOLE_BIN"
echo ""

if [[ "$CURRENT_FREE_GB" -ge "$TARGET_GB" ]]; then
  echo "Target already met."
  exit 0
fi

echo "Need to reclaim about $((TARGET_GB - CURRENT_FREE_GB))GB more."
echo ""

if ! purge_config_has_path "$HOME/code" || ! purge_config_has_path "$HOME/repos"; then
  echo "Warning: Mole purge paths do not explicitly include both ~/code and ~/repos."
  echo "If the purge menu looks too small, run:"
  echo "  $MOLE_BIN purge --paths"
  echo "Then add:"
  echo "  ~/code"
  echo "  ~/repos"
  echo ""
fi

print_candidate_sizes

INSTALLER_ARGS=("$MOLE_BIN" "installer")
PURGE_ARGS=("$MOLE_BIN" "purge")
CLEAN_ARGS=("$MOLE_BIN" "clean")

if [[ "$APPLY" != "true" ]]; then
  INSTALLER_ARGS+=("--dry-run")
  PURGE_ARGS+=("--dry-run")
  CLEAN_ARGS+=("--dry-run" "--debug")
fi

echo "This will open Mole's interactive flows one after another."
echo "Review selections carefully before confirming."
echo ""

run_step "Installer Cleanup" "${INSTALLER_ARGS[@]}"
run_step "Project Artifact Purge" "${PURGE_ARGS[@]}"
run_step "Caches, Logs, and Trash Cleanup" "${CLEAN_ARGS[@]}"

FINAL_FREE_GB="$(free_gb_int)"
FINAL_FREE_HUMAN="$(free_human)"

echo "Summary"
echo "Free space after run: ${FINAL_FREE_GB}GB (${FINAL_FREE_HUMAN})"

if [[ "$FINAL_FREE_GB" -ge "$TARGET_GB" ]]; then
  echo "Target reached."
  exit 0
fi

echo "Still below target by about $((TARGET_GB - FINAL_FREE_GB))GB."

if [[ "$APPLY" == "true" ]]; then
  echo "Next step: build Mole's analyzer if needed, then inspect large folders manually."
  echo "Example:"
  echo "  cd $HOME/repos/tw93/mole && make build && ./mo analyze"
else
  echo "Dry-run complete. When ready, rerun with:"
  echo "  f free-space --apply"
fi
