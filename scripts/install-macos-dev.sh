#!/usr/bin/env bash
set -euo pipefail

# macOS-only dev installer for Flow + local Jazz/Groove.
# Clones Flow + Jazz, patches Cargo to use local groove crates, builds, and
# symlinks binaries into ~/.local/bin.

fail() {
  echo "flow macos dev install: $*" >&2
  exit 1
}

info() {
  echo "flow macos dev install: $*"
}

if [[ "$(uname -s)" != "Darwin" ]]; then
  fail "this script is macOS-only"
fi

BASE_DIR="${FLOW_DEV_ROOT:-$HOME/code/org/1f}"
FLOW_REPO_URL="${FLOW_REPO_URL:-https://github.com/nikivdev/flow}"
JAZZ_REPO_URL="${FLOW_JAZZ_URL:-https://github.com/1focus-ai/jazz}"
FLOW_DIR="${FLOW_DEV_FLOW_DIR:-$BASE_DIR/flow}"
JAZZ_DIR="${FLOW_DEV_JAZZ_DIR:-$BASE_DIR/jazz}"
BIN_DIR="${FLOW_BIN_DIR:-$HOME/.local/bin}"
USE_SSH="${FLOW_GIT_SSH:-}"
GITHUB_TOKEN="${FLOW_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JAZZ_OPTIONAL="${FLOW_JAZZ_OPTIONAL:-1}"
JAZZ_AVAILABLE=1
DIST_DIR="${FLOW_DIST_DIR:-${SCRIPT_DIR}/../dist}"
FORCE_HTTPS=0

ensure_brew() {
  if command -v brew >/dev/null 2>&1; then
    return 0
  fi
  info "installing Homebrew..."
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  if [[ -f "/opt/homebrew/bin/brew" ]]; then
    eval "$(/opt/homebrew/bin/brew shellenv)"
  elif [[ -f "/usr/local/bin/brew" ]]; then
    eval "$(/usr/local/bin/brew shellenv)"
  fi
}

ensure_fnm_and_node() {
  if ! command -v fnm >/dev/null 2>&1; then
    info "installing fnm..."
    brew install fnm
  fi

  # Ensure fnm is active in this shell
  eval "$(fnm env)"

  if ! command -v node >/dev/null 2>&1; then
    info "installing Node.js (LTS) via fnm..."
    install_out="$(fnm install --lts 2>&1)" || fail "fnm install --lts failed"
    echo "$install_out"
    installed_version="$(printf "%s\n" "$install_out" | grep -Eo 'v[0-9]+\.[0-9]+\.[0-9]+' | tail -n1 || true)"
    if [[ -n "${installed_version}" ]]; then
      fnm default "${installed_version}" || true
    fi
  fi
}

ensure_fzf() {
  if command -v fzf >/dev/null 2>&1; then
    return 0
  fi
  info "installing fzf..."
  brew install fzf
}

ensure_rust() {
  if command -v cargo >/dev/null 2>&1; then
    return 0
  fi
  info "installing Rust..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env"
}

check_github_ssh() {
  if [[ "${FLOW_FORCE_HTTPS:-}" = "1" ]]; then
    FORCE_HTTPS=1
    return 0
  fi
  if [[ "${FLOW_SSH_MODE:-}" = "https" ]]; then
    FORCE_HTTPS=1
    return 0
  fi
  if ! command -v ssh >/dev/null 2>&1; then
    return 0
  fi

  local out
  out="$(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -T git@github.com 2>&1 || true)"
  if echo "${out}" | grep -qi "successfully authenticated"; then
    info "GitHub SSH auth OK."
    return 0
  fi
  if echo "${out}" | grep -qi "Permission denied"; then
    FORCE_HTTPS=1
    info "GitHub SSH auth failed; configuring Flow to prefer HTTPS."
  fi
}

clone_or_update() {
    mkdir -p "${BASE_DIR}"

    local resolved_jazz_url
    local resolved_flow_url

    resolved_jazz_url="$(resolve_repo_url "${JAZZ_REPO_URL}")"
    resolved_flow_url="$(resolve_repo_url "${FLOW_REPO_URL}")"

    if [[ -d "${JAZZ_DIR}/.git" ]]; then
        info "updating Jazz..."
        (cd "${JAZZ_DIR}" && GIT_TERMINAL_PROMPT=0 git pull --rebase) || true
    else
        info "cloning Jazz to ${JAZZ_DIR}..."
        if ! GIT_TERMINAL_PROMPT=0 git clone "${resolved_jazz_url}" "${JAZZ_DIR}"; then
            if [[ "${JAZZ_OPTIONAL}" != "0" ]]; then
                info "Jazz clone failed; continuing without local Jazz (release fallback)."
                info "To build with local Jazz, set FLOW_GIT_SSH=1 or FLOW_GITHUB_TOKEN=... and rerun."
                if [[ -x "${SCRIPT_DIR}/setup-github-ssh.sh" ]]; then
                    info "running SSH setup helper so you can add the key to GitHub if needed..."
                    "${SCRIPT_DIR}/setup-github-ssh.sh" || true
                fi
                JAZZ_AVAILABLE=0
            else
                fail_clone "${JAZZ_REPO_URL}"
            fi
        fi
    fi

    if [[ -d "${FLOW_DIR}/.git" ]]; then
        info "updating Flow..."
        (cd "${FLOW_DIR}" && GIT_TERMINAL_PROMPT=0 git pull --rebase) || true
    else
        info "cloning Flow to ${FLOW_DIR}..."
        GIT_TERMINAL_PROMPT=0 git clone "${resolved_flow_url}" "${FLOW_DIR}" || fail_clone "${FLOW_REPO_URL}"
    fi
}

resolve_repo_url() {
  local url="$1"

  if [[ -n "${USE_SSH}" ]]; then
    if [[ "${url}" =~ ^https://github.com/([^/]+)/([^/]+)(\.git)?$ ]]; then
      echo "git@github.com:${BASH_REMATCH[1]}/${BASH_REMATCH[2]}.git"
      return
    fi
  fi

  if [[ -n "${GITHUB_TOKEN}" ]]; then
    if [[ "${url}" =~ ^https://github.com/(.+)$ ]]; then
      echo "https://x-access-token:${GITHUB_TOKEN}@github.com/${BASH_REMATCH[1]}"
      return
    fi
  fi

  echo "${url}"
}

fail_clone() {
  local url="$1"
  info ""
  info "clone failed for ${url}"
  if [[ -x "${SCRIPT_DIR}/setup-github-ssh.sh" ]]; then
    info "attempting to provision GitHub SSH key..."
    "${SCRIPT_DIR}/setup-github-ssh.sh" || true
  fi
  info "If this repo is private, set one of:"
  info "  FLOW_GITHUB_TOKEN=... (or GITHUB_TOKEN=...)"
  info "  FLOW_GIT_SSH=1 (uses git@github.com:... and your SSH key)"
  info "  FLOW_JAZZ_OPTIONAL=0 to require Jazz and fail fast"
  fail "unable to clone ${url}"
}

write_cargo_patch() {
  if [[ "${JAZZ_AVAILABLE}" = "0" ]]; then
    return 0
  fi
  mkdir -p "${FLOW_DIR}/.cargo"
  cat > "${FLOW_DIR}/.cargo/config.toml" <<EOF
# Local path overrides for faster builds (auto-generated by install-macos-dev.sh)
[patch."https://github.com/1focus-ai/jazz"]
groove = { path = "${JAZZ_DIR}/crates/groove" }
groove-rocksdb = { path = "${JAZZ_DIR}/crates/groove-rocksdb" }
EOF
}

build_and_link() {
  cleanup_stale_links
  if [[ "${JAZZ_AVAILABLE}" = "0" ]]; then
    install_release_fallback
    return 0
  fi
  info "building Flow..."
  (cd "${FLOW_DIR}" && cargo build --release)

  mkdir -p "${BIN_DIR}"
  ln -sf "${FLOW_DIR}/target/release/f" "${BIN_DIR}/f"
  ln -sf "${FLOW_DIR}/target/release/f" "${BIN_DIR}/flow"
  if [[ -f "${FLOW_DIR}/target/release/lin" ]]; then
    ln -sf "${FLOW_DIR}/target/release/lin" "${BIN_DIR}/lin"
  fi
}

install_release_fallback() {
  local root_installer="${SCRIPT_DIR}/../install.sh"

  if install_local_dist; then
    return 0
  fi

  info "installing Flow from release (no Jazz access)..."

  if [[ -x "${root_installer}" ]]; then
    if ! FLOW_INSTALL_PATH="${BIN_DIR}/f" "${root_installer}"; then
      info "release install failed."
      info "If you don't have access to the private Jazz repo, you need a public Flow release."
      fail "release install unavailable"
    fi
    ln -sf "${BIN_DIR}/f" "${BIN_DIR}/flow"
    return 0
  fi

  if [[ -x "${SCRIPT_DIR}/install.sh" ]]; then
    if ! FLOW_SKIP_DEPS=1 FLOW_BIN_DIR="${BIN_DIR}" FLOW_RELEASE_ONLY=1 "${SCRIPT_DIR}/install.sh"; then
      info "release install failed."
      info "If you don't have access to the private Jazz repo, you need a public Flow release."
      fail "release install unavailable"
    fi
    return 0
  fi

  fail "no installer found for release fallback"
}

install_local_dist() {
  local arch
  local pattern
  local tarball
  local tmpdir
  local binary

  if [[ ! -d "${DIST_DIR}" ]]; then
    return 1
  fi

  arch="$(uname -m)"
  case "${arch}" in
    arm64) pattern="*_darwin_arm64.tar.gz" ;;
    x86_64) pattern="*_darwin_x64.tar.gz" ;;
    *) return 1 ;;
  esac

  tarball="$(ls -t "${DIST_DIR}"/${pattern} 2>/dev/null | head -n1 || true)"
  if [[ -z "${tarball}" ]]; then
    # fallback for amd64 naming
    if [[ "${arch}" = "x86_64" ]]; then
      tarball="$(ls -t "${DIST_DIR}"/*_darwin_amd64.tar.gz 2>/dev/null | head -n1 || true)"
    fi
  fi

  if [[ -z "${tarball}" ]]; then
    return 1
  fi

  info "installing Flow from local dist: ${tarball}"
  tmpdir="$(mktemp -d)"
  tar -xzf "${tarball}" -C "${tmpdir}"

  if [[ -f "${tmpdir}/f" ]]; then
    binary="${tmpdir}/f"
  else
    binary="$(find "${tmpdir}" -type f \( -name "f" -o -name "flow" \) 2>/dev/null | head -n1 || true)"
  fi

  if [[ -z "${binary}" ]]; then
    rm -rf "${tmpdir}"
    return 1
  fi

  cleanup_stale_links
  mkdir -p "${BIN_DIR}"
  cp "${binary}" "${BIN_DIR}/f"
  chmod +x "${BIN_DIR}/f"
  ln -sf "${BIN_DIR}/f" "${BIN_DIR}/flow"
  if [[ -f "${tmpdir}/lin" ]]; then
    cp "${tmpdir}/lin" "${BIN_DIR}/lin"
    chmod +x "${BIN_DIR}/lin"
  fi

  rm -rf "${tmpdir}"
  return 0
}

cleanup_stale_links() {
  local home_bin="$HOME/bin"
  rm -f "${BIN_DIR}/f" "${BIN_DIR}/flow"
  if [[ -d "${home_bin}" ]]; then
    rm -f "${home_bin}/f" "${home_bin}/flow"
  fi
}

ensure_shell_setup() {
  local zshrc="$HOME/.zshrc"
  local bashrc="$HOME/.bashrc"
  local bash_profile="$HOME/.bash_profile"

  local path_line="export PATH=\"${BIN_DIR}:\$PATH\""
  local fnm_line='eval "$(fnm env --use-on-cd)"'
  local https_line='export FLOW_FORCE_HTTPS=1'

  if [[ -f "${zshrc}" ]]; then
    grep -qF "${path_line}" "${zshrc}" || echo "${path_line}" >> "${zshrc}"
    grep -qF "${fnm_line}" "${zshrc}" || echo "${fnm_line}" >> "${zshrc}"
    if [[ "${FORCE_HTTPS}" = "1" ]]; then
      grep -qF "${https_line}" "${zshrc}" || echo "${https_line}" >> "${zshrc}"
    fi
  elif [[ -f "${bashrc}" ]]; then
    grep -qF "${path_line}" "${bashrc}" || echo "${path_line}" >> "${bashrc}"
    grep -qF "${fnm_line}" "${bashrc}" || echo "${fnm_line}" >> "${bashrc}"
    if [[ "${FORCE_HTTPS}" = "1" ]]; then
      grep -qF "${https_line}" "${bashrc}" || echo "${https_line}" >> "${bashrc}"
    fi
  elif [[ -f "${bash_profile}" ]]; then
    grep -qF "${path_line}" "${bash_profile}" || echo "${path_line}" >> "${bash_profile}"
    grep -qF "${fnm_line}" "${bash_profile}" || echo "${fnm_line}" >> "${bash_profile}"
    if [[ "${FORCE_HTTPS}" = "1" ]]; then
      grep -qF "${https_line}" "${bash_profile}" || echo "${https_line}" >> "${bash_profile}"
    fi
  else
    info "add to your shell config:"
    info "  ${path_line}"
    info "  ${fnm_line}"
    if [[ "${FORCE_HTTPS}" = "1" ]]; then
      info "  ${https_line}"
    fi
  fi
}

main() {
  info "starting macOS dev install"
  ensure_brew
  ensure_fzf
  ensure_fnm_and_node
  check_github_ssh
  clone_or_update
  if [[ "${JAZZ_AVAILABLE}" = "0" ]]; then
    install_release_fallback
    ensure_shell_setup
    info ""
    info "done."
    info "flow: ${FLOW_DIR}"
    info "jazz: (skipped)"
    info "bin: ${BIN_DIR}"
    info "restart your shell, then run: f --help"
    return
  fi
  ensure_rust
  write_cargo_patch
  build_and_link
  ensure_shell_setup

  info ""
  info "done."
  info "flow: ${FLOW_DIR}"
  info "jazz: ${JAZZ_DIR}"
  info "bin: ${BIN_DIR}"
  info "restart your shell, then run: f --help"
}

main "$@"
