#!/usr/bin/env bash
set -euo pipefail

# macOS helper to provision an SSH key for GitHub and configure ssh-agent.
# Designed to be run non-interactively; set FLOW_SSH_PASSPHRASE if desired.

fail() {
  echo "flow github ssh: $*" >&2
  exit 1
}

info() {
  echo "flow github ssh: $*"
}

if [[ "$(uname -s)" != "Darwin" ]]; then
  fail "this script is macOS-only"
fi

KEY_PATH="${FLOW_SSH_KEY_PATH:-$HOME/.ssh/id_ed25519}"
EMAIL="${FLOW_SSH_EMAIL:-${USER}@$(hostname -s)}"
PASSPHRASE="${FLOW_SSH_PASSPHRASE:-}"

ensure_key() {
  if [[ -f "${KEY_PATH}" && -f "${KEY_PATH}.pub" ]]; then
    info "existing SSH key found at ${KEY_PATH}"
    return 0
  fi

  info "generating SSH key at ${KEY_PATH}..."
  mkdir -p "$(dirname "${KEY_PATH}")"
  ssh-keygen -t ed25519 -C "${EMAIL}" -f "${KEY_PATH}" -N "${PASSPHRASE}"
}

ensure_agent() {
  if [[ -z "${SSH_AUTH_SOCK:-}" ]]; then
    eval "$(ssh-agent -s)"
  fi

  if ssh-add --apple-use-keychain "${KEY_PATH}" >/dev/null 2>&1; then
    return 0
  fi

  ssh-add "${KEY_PATH}"
}

ensure_config() {
  local config_file="$HOME/.ssh/config"
  mkdir -p "$(dirname "${config_file}")"
  touch "${config_file}"

  if ! grep -q "Host github.com" "${config_file}"; then
    cat >> "${config_file}" <<EOF

Host github.com
  AddKeysToAgent yes
  UseKeychain yes
  IdentityFile ${KEY_PATH}
EOF
    info "updated ${config_file}"
  fi
}

print_next_steps() {
  info ""
  info "add this public key to GitHub (Settings -> SSH and GPG keys -> New SSH key):"
  if command -v pbcopy >/dev/null 2>&1; then
    pbcopy < "${KEY_PATH}.pub"
    info "public key copied to clipboard"
  else
    cat "${KEY_PATH}.pub"
  fi
  info ""
  info "then run: ssh -T git@github.com"
}

ensure_key
ensure_agent
ensure_config
print_next_steps
