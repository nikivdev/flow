#!/usr/bin/env bash
set -euo pipefail

cmd="${1:-}"
profile="${2:-}"
repo="${3:-$(pwd)}"

if [[ -z "$cmd" ]]; then
  echo "Usage:"
  echo "  f agents <profile> [repo]"
  echo "  f agents rules [profile] [repo]"
  exit 1
fi

if [[ "$cmd" == "rules" ]]; then
  if [[ -n "$profile" && -d "$profile" && -z "${3:-}" ]]; then
    repo="$profile"
    profile=""
  elif [[ -n "${3:-}" ]]; then
    repo="${3}"
  fi

  if [[ ! -d "$repo" ]]; then
    echo "Repo not found: $repo"
    exit 1
  fi
  if [[ ! -d "$repo/agents" ]]; then
    echo "No agents/ directory in $repo"
    exit 1
  fi
  mapfile -t profiles < <(ls "$repo"/agents/agents.*.md "$repo"/agents/AGENTS.*.md 2>/dev/null | sed -E 's#.*/(AGENTS|agents)\\.##; s#\\.md$##' | sort -u)
  if [[ ${#profiles[@]} -eq 0 ]]; then
    echo "No profiles found in $repo/agents"
    exit 1
  fi

  if [[ -n "$profile" ]]; then
    if [[ ! -f "$repo/agents/agents.${profile}.md" && ! -f "$repo/agents/AGENTS.${profile}.md" ]]; then
      echo "Missing profile: $repo/agents/agents.${profile}.md"
      exit 1
    fi
  else
    if command -v fzf >/dev/null 2>&1; then
      profile="$(printf '%s\n' "${profiles[@]}" | fzf --prompt="agents> " --height=40% --border)"
    else
      echo "fzf not found; using numbered selection."
      select choice in "${profiles[@]}"; do
        profile="$choice"
        break
      done
    fi
    if [[ -z "$profile" ]]; then
      echo "No profile selected."
      exit 1
    fi
  fi
elif [[ -z "$profile" ]]; then
  if [[ -d "$repo/agents" && -f "$repo/agents/.default" ]]; then
    profile="$(cat "$repo/agents/.default" | tr -d '[:space:]')"
  else
    echo "Usage:"
    echo "  f agents <profile> [repo]"
    echo "  f agents rules [profile] [repo]"
    exit 1
  fi
fi

if [[ ! -d "$repo" ]]; then
  echo "Repo not found: $repo"
  exit 1
fi

candidate="$repo/agents/agents.${profile}.md"
if [[ ! -f "$candidate" ]]; then
  candidate="$repo/agents/AGENTS.${profile}.md"
  if [[ ! -f "$candidate" ]]; then
    echo "Missing profile: $candidate"
    exit 1
  fi
fi

cp "$candidate" "$repo/agents.md"

echo "$profile" > "$repo/agents/.default"
echo "Activated agents.md -> $candidate"
echo "Default profile set to: $profile"
