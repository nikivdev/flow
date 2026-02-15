#!/bin/sh
set -eu

# Flow CLI installer
# Usage: curl -fsSL https://myflow.sh/install.sh | sh

# Security posture:
# - We require SHA-256 verification by default.
# - Set FLOW_INSTALL_INSECURE=1 (or true/yes) to bypass verification.

#region logging
if [ "${FLOW_DEBUG-}" = "true" ] || [ "${FLOW_DEBUG-}" = "1" ]; then
  debug() { echo "$@" >&2; }
else
  debug() { :; }
fi

if [ "${FLOW_QUIET-}" = "1" ] || [ "${FLOW_QUIET-}" = "true" ]; then
  info() { :; }
else
  info() { echo "$@" >&2; }
fi

error() {
  echo "error: $@" >&2
  exit 1
}

is_truthy() {
  case "${1:-}" in
    1|true|TRUE|yes|YES|y|Y) return 0 ;;
    *) return 1 ;;
  esac
}
#endregion

#region platform detection
get_os() {
  os="$(uname -s)"
  if [ "$os" = Darwin ]; then
    echo "macos"
  elif [ "$os" = Linux ]; then
    echo "linux"
  else
    error "unsupported OS: $os"
  fi
}

get_arch() {
  arch="$(uname -m)"
  if [ "$arch" = x86_64 ]; then
    echo "x64"
  elif [ "$arch" = aarch64 ] || [ "$arch" = arm64 ]; then
    echo "arm64"
  else
    error "unsupported architecture: $arch"
  fi
}

get_target() {
  os="$1"
  arch="$2"
  case "$os-$arch" in
    macos-x64) echo "x86_64-apple-darwin" ;;
    macos-arm64) echo "aarch64-apple-darwin" ;;
    linux-x64) echo "x86_64-unknown-linux-gnu" ;;
    linux-arm64) echo "aarch64-unknown-linux-gnu" ;;
    *) error "unsupported platform: $os-$arch" ;;
  esac
}

shasum_bin() {
  if command -v shasum >/dev/null 2>&1; then
    echo "shasum -a 256"
  elif command -v sha256sum >/dev/null 2>&1; then
    echo "sha256sum"
  else
    echo ""
  fi
}

validate_repo() {
  repo="$1"
  if [ -z "${repo:-}" ]; then
    error "FLOW_UPGRADE_REPO is empty"
  fi

  owner="${repo%/*}"
  name="${repo#*/}"
  if [ "$owner" = "$repo" ] || [ "$name" = "$repo" ]; then
    error "invalid repo '${repo}' (expected owner/repo)"
  fi
  case "$owner" in */*) error "invalid repo '${repo}' (expected owner/repo)" ;; esac
  case "$name" in */*) error "invalid repo '${repo}' (expected owner/repo)" ;; esac

  case "$owner" in *[!A-Za-z0-9._-]*)
    error "invalid repo owner '${owner}' (allowed: A-Z a-z 0-9 . _ -)"
    ;;
  esac
  case "$name" in *[!A-Za-z0-9._-]*)
    error "invalid repo name '${name}' (allowed: A-Z a-z 0-9 . _ -)"
    ;;
  esac
}

validate_token() {
  token="$1"
  if [ -z "${token:-}" ]; then
    error "GitHub token is empty"
  fi
  case "$token" in
    *[!A-Za-z0-9._-]*)
      error "invalid GitHub token characters (refusing to use it)"
      ;;
  esac
}

validate_version() {
  version="$1"
  case "$version" in
    v*) tag="${version#v}" ;;
    *) tag="$version" ;;
  esac
  case "$tag" in
    ""|*[!0-9A-Za-z._-]*)
      error "invalid release version '${version}'"
      ;;
  esac
}
#endregion

#region download helpers
download_file() {
  url="$1"
  file="$2"
  if command -v curl >/dev/null 2>&1; then
    debug ">" curl -fsSL -o "$file" "$url"
    if [ "${FLOW_DEBUG-}" = "true" ] || [ "${FLOW_DEBUG-}" = "1" ]; then
      curl -fsSL --proto '=https' --tlsv1.2 -o "$file" "$url"
    else
      curl -fsSL --proto '=https' --tlsv1.2 -o "$file" "$url" 2>/dev/null
    fi
  elif command -v wget >/dev/null 2>&1; then
    debug ">" wget -qO "$file" "$url"
    wget -qO "$file" "$url"
  else
    error "curl or wget is required"
  fi
}

fetch_url() {
  url="$1"
  if command -v curl >/dev/null 2>&1; then
    case "$url" in
      https://api.github.com/*)
        token="${GITHUB_TOKEN:-${GH_TOKEN:-${FLOW_GITHUB_TOKEN:-}}}"
        if [ -n "${token:-}" ]; then
          validate_token "$token"
          curl -fsSL --proto '=https' --tlsv1.2 -H "Authorization: Bearer ${token}" "$url"
        else
          curl -fsSL --proto '=https' --tlsv1.2 "$url"
        fi
        ;;
      *)
        curl -fsSL --proto '=https' --tlsv1.2 "$url"
        ;;
    esac
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$url"
  else
    error "curl or wget is required"
  fi
}

get_latest_version() {
  repo="${FLOW_UPGRADE_REPO:-}"
  if [ -z "${repo:-}" ] && [ -n "${FLOW_GITHUB_OWNER:-}" ] && [ -n "${FLOW_GITHUB_REPO:-}" ]; then
    repo="${FLOW_GITHUB_OWNER}/${FLOW_GITHUB_REPO}"
  fi
  repo="${repo:-nikivdev/flow}"
  validate_repo "$repo"

  url="https://api.github.com/repos/${repo}/releases/latest"
  version="$(fetch_url "$url" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')"
  validate_version "$version"
  echo "$version"
}

get_checksum() {
  version="$1"
  target="$2"
  repo="${FLOW_UPGRADE_REPO:-}"
  if [ -z "${repo:-}" ] && [ -n "${FLOW_GITHUB_OWNER:-}" ] && [ -n "${FLOW_GITHUB_REPO:-}" ]; then
    repo="${FLOW_GITHUB_OWNER}/${FLOW_GITHUB_REPO}"
  fi
  repo="${repo:-nikivdev/flow}"
  validate_repo "$repo"

  url="https://github.com/${repo}/releases/download/${version}/checksums.txt"
  checksums="$(fetch_url "$url" 2>/dev/null)" || return 1
  echo "$checksums" | grep "flow-${target}.tar.gz" | awk '{print $1}'
}

get_checksum_for_file() {
  version="$1"
  file="$2"
  repo="${FLOW_UPGRADE_REPO:-}"
  if [ -z "${repo:-}" ] && [ -n "${FLOW_GITHUB_OWNER:-}" ] && [ -n "${FLOW_GITHUB_REPO:-}" ]; then
    repo="${FLOW_GITHUB_OWNER}/${FLOW_GITHUB_REPO}"
  fi
  repo="${repo:-nikivdev/flow}"
  validate_repo "$repo"

  url="https://github.com/${repo}/releases/download/${version}/checksums.txt"
  checksums="$(fetch_url "$url" 2>/dev/null)" || return 1
  # checksums.txt format: "<sha256> <filename>"
  echo "$checksums" | awk -v f="$file" '$2==f {print $1}'
}
#endregion

install_flow() {
  version="${FLOW_VERSION:-canary}"
  os="${FLOW_OS:-$(get_os)}"
  arch="${FLOW_ARCH:-$(get_arch)}"
  target="$(get_target "$os" "$arch")"
  install_path="${FLOW_INSTALL_PATH:-$HOME/.flow/bin/f}"
  install_dir="$(dirname "$install_path")"

  info "flow: installing flow CLI..."
  info "flow: platform: $os-$arch ($target)"

  # Get latest version if needed
  if [ "$version" = "latest" ]; then
    info "flow: fetching latest version..."
    version="$(get_latest_version)"
    if [ -z "$version" ]; then
      error "failed to fetch latest version"
    fi
  fi
  validate_version "$version"
  info "flow: version: $version"

  # URLs - try CDN first, fallback to GitHub
  cdn_url="https://cdn.myflow.sh/${version}/flow-${target}.tar.gz"
  repo="${FLOW_UPGRADE_REPO:-}"
  if [ -z "${repo:-}" ] && [ -n "${FLOW_GITHUB_OWNER:-}" ] && [ -n "${FLOW_GITHUB_REPO:-}" ]; then
    repo="${FLOW_GITHUB_OWNER}/${FLOW_GITHUB_REPO}"
  fi
  repo="${repo:-nikivdev/flow}"
  validate_repo "$repo"
  github_url="https://github.com/${repo}/releases/download/${version}/flow-${target}.tar.gz"

  download_dir="$(mktemp -d)"
  tarball="$download_dir/flow.tar.gz"
  download_source="unknown"

  asset_file="flow-${target}.tar.gz"
  legacy_os="$os"
  if [ "$legacy_os" = "macos" ]; then
    legacy_os="darwin"
  fi
  legacy_arch="amd64"
  if [ "$arch" = "arm64" ]; then
    legacy_arch="arm64"
  fi
  legacy_file="flow_${version}_${legacy_os}_${legacy_arch}.tar.gz"
  legacy_url="https://github.com/${repo}/releases/download/${version}/${legacy_file}"

  # Try CDN first (faster)
  info "flow: downloading..."
  if command -v curl >/dev/null 2>&1 && curl -fsSL -o "$tarball" "$cdn_url" 2>/dev/null; then
    debug "flow: downloaded from CDN"
    download_source="cdn"
  else
    debug "flow: trying GitHub..."
    if download_file "$github_url" "$tarball"; then
      asset_file="flow-${target}.tar.gz"
      download_source="github"
    elif download_file "$legacy_url" "$tarball"; then
      asset_file="$legacy_file"
      download_source="legacy"
    else
      error "download failed"
    fi
  fi

  # Verify checksum if available
  shasum="$(shasum_bin)"
  if [ -n "$shasum" ]; then
    expected="$(get_checksum_for_file "$version" "$asset_file" 2>/dev/null)" || true
    if [ -z "${expected:-}" ]; then
      # Back-compat: allow checksums.txt to contain either naming scheme.
      if [ "$asset_file" = "$legacy_file" ]; then
        expected="$(get_checksum_for_file "$version" "flow-${target}.tar.gz" 2>/dev/null)" || true
      elif [ "$asset_file" = "flow-${target}.tar.gz" ]; then
        expected="$(get_checksum_for_file "$version" "$legacy_file" 2>/dev/null)" || true
      fi
    fi
    if [ -z "${expected:-}" ]; then
      if is_truthy "${FLOW_INSTALL_INSECURE-}"; then
        info "flow: warning: checksum not verified (FLOW_INSTALL_INSECURE=1)"
      elif [ "${download_source:-}" = "cdn" ]; then
        rm -rf "$download_dir" "$extract_dir" 2>/dev/null || true
        error "checksum verification failed for CDN download (checksums.txt missing or entry not found). Refusing to install.\nSet FLOW_INSTALL_INSECURE=1 to bypass (not recommended)."
      else
        info "flow: warning: checksum not verified (checksums.txt missing or entry not found; legacy release?)"
        expected=""
      fi
    fi
    if [ -n "${expected:-}" ]; then
      debug "flow: verifying checksum..."
      actual="$($shasum "$tarball" | awk '{print $1}')"
      if [ "$expected" != "$actual" ]; then
        rm -rf "$download_dir"
        error "checksum mismatch"
      fi
      info "flow: checksum verified"
    fi
  else
    if is_truthy "${FLOW_INSTALL_INSECURE-}"; then
      info "flow: warning: sha256 tool not found, skipping checksum verification (FLOW_INSTALL_INSECURE=1)"
    else
      error "sha256 tool not found (need shasum or sha256sum). Refusing to install.\nSet FLOW_INSTALL_INSECURE=1 to bypass (not recommended)."
    fi
  fi

  # Extract and install
  mkdir -p "$install_dir"
  extract_dir="$(mktemp -d)"
  tar -xzf "$tarball" -C "$extract_dir"

  # Find binary
  if [ -f "$extract_dir/f" ]; then
    mv "$extract_dir/f" "$install_path"
  else
    binary="$(find "$extract_dir" -type f \( -name "f" -o -name "flow" \) 2>/dev/null | head -1)"
    if [ -z "$binary" ]; then
      binary="$(find "$extract_dir" -type f -perm +111 2>/dev/null | head -1)"
    fi
    if [ -z "$binary" ]; then
      rm -rf "$download_dir" "$extract_dir"
      error "binary not found in archive"
    fi
    mv "$binary" "$install_path"
  fi
  chmod +x "$install_path"

  # Provide both `f` and `flow` as entrypoints.
  base="$(basename "$install_path")"
  if [ "$base" = "f" ]; then
    if [ -e "$install_dir/flow" ] && [ -d "$install_dir/flow" ]; then
      info "flow: warning: cannot create symlink $install_dir/flow (path is a directory)"
    else
      ln -sf "f" "$install_dir/flow" 2>/dev/null || true
    fi
  elif [ "$base" = "flow" ]; then
    if [ -e "$install_dir/f" ] && [ -d "$install_dir/f" ]; then
      info "flow: warning: cannot create symlink $install_dir/f (path is a directory)"
    else
      ln -sf "flow" "$install_dir/f" 2>/dev/null || true
    fi
  fi

  # Cleanup
  rm -rf "$download_dir" "$extract_dir"

  info "flow: installed to $install_path"
}

configure_shell() {
  install_dir="$(dirname "${FLOW_INSTALL_PATH:-$HOME/.flow/bin/f}")"
  registry_url="${FLOW_REGISTRY_URL:-https://myflow.sh}"

  # Fish
  if [ -f "$HOME/.config/fish/config.fish" ]; then
    if ! grep -q ".flow/bin" "$HOME/.config/fish/config.fish" 2>/dev/null; then
      echo "fish_add_path $install_dir" >> "$HOME/.config/fish/config.fish"
      info "flow: added to ~/.config/fish/config.fish"
    fi
    if ! grep -q "FLOW_REGISTRY_URL" "$HOME/.config/fish/config.fish" 2>/dev/null; then
      echo "set -gx FLOW_REGISTRY_URL \"$registry_url\"" >> "$HOME/.config/fish/config.fish"
    fi
  fi

  # Zsh
  if [ -f "$HOME/.zshrc" ]; then
    if ! grep -q ".flow/bin" "$HOME/.zshrc" 2>/dev/null; then
      echo "export PATH=\"$install_dir:\$PATH\"" >> "$HOME/.zshrc"
      info "flow: added to ~/.zshrc"
    fi
    if ! grep -q "FLOW_REGISTRY_URL" "$HOME/.zshrc" 2>/dev/null; then
      echo "export FLOW_REGISTRY_URL=\"$registry_url\"" >> "$HOME/.zshrc"
    fi
  fi

  # Bash
  for rc in "$HOME/.bashrc" "$HOME/.bash_profile"; do
    if [ -f "$rc" ]; then
      if ! grep -q ".flow/bin" "$rc" 2>/dev/null; then
        echo "export PATH=\"$install_dir:\$PATH\"" >> "$rc"
        info "flow: added to $rc"
      fi
      if ! grep -q "FLOW_REGISTRY_URL" "$rc" 2>/dev/null; then
        echo "export FLOW_REGISTRY_URL=\"$registry_url\"" >> "$rc"
      fi
      break
    fi
  done
}

after_install() {
  info ""
  info "flow: installed successfully!"
  case "${SHELL:-}" in
    */fish) info "flow: restart shell or run: fish_add_path ~/.flow/bin" ;;
    */zsh|*/bash) info "flow: restart shell or run: export PATH=\"\$HOME/.flow/bin:\$PATH\"" ;;
    *) info "flow: add ~/.flow/bin to your PATH" ;;
  esac
  info "flow: then run 'f --help' to get started"
  info "flow: docs: https://myflow.sh"
}

install_flow
configure_shell
after_install
