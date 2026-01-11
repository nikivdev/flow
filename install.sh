#!/bin/sh
set -eu

# Flow CLI installer
# Usage: curl -fsSL https://myflow.sh/install.sh | sh

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
#endregion

#region download helpers
download_file() {
  url="$1"
  file="$2"
  if command -v curl >/dev/null 2>&1; then
    debug ">" curl -fsSL -o "$file" "$url"
    curl -fsSL -o "$file" "$url"
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
    curl -fsSL "$url"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- "$url"
  else
    error "curl or wget is required"
  fi
}

get_latest_version() {
  url="https://api.github.com/repos/nikitavoloboev/flow/releases/latest"
  fetch_url "$url" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'
}

get_checksum() {
  version="$1"
  target="$2"
  url="https://github.com/nikitavoloboev/flow/releases/download/${version}/checksums.txt"
  checksums="$(fetch_url "$url" 2>/dev/null)" || return 1
  echo "$checksums" | grep "flow-${target}.tar.gz" | awk '{print $1}'
}
#endregion

install_flow() {
  version="${FLOW_VERSION:-latest}"
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
  info "flow: version: $version"

  # URLs - try CDN first, fallback to GitHub
  cdn_url="https://cdn.myflow.sh/${version}/flow-${target}.tar.gz"
  github_url="https://github.com/nikitavoloboev/flow/releases/download/${version}/flow-${target}.tar.gz"

  download_dir="$(mktemp -d)"
  tarball="$download_dir/flow.tar.gz"

  # Try CDN first (faster)
  info "flow: downloading..."
  if command -v curl >/dev/null 2>&1 && curl -fsSL -o "$tarball" "$cdn_url" 2>/dev/null; then
    debug "flow: downloaded from CDN"
  else
    debug "flow: trying GitHub..."
    download_file "$github_url" "$tarball" || error "download failed"
  fi

  # Verify checksum if available
  shasum="$(shasum_bin)"
  if [ -n "$shasum" ]; then
    expected="$(get_checksum "$version" "$target" 2>/dev/null)" || true
    if [ -n "${expected:-}" ]; then
      debug "flow: verifying checksum..."
      actual="$($shasum "$tarball" | awk '{print $1}')"
      if [ "$expected" != "$actual" ]; then
        rm -rf "$download_dir"
        error "checksum mismatch"
      fi
      info "flow: checksum verified"
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

  # Cleanup
  rm -rf "$download_dir" "$extract_dir"

  info "flow: installed to $install_path"
}

configure_shell() {
  install_dir="$(dirname "${FLOW_INSTALL_PATH:-$HOME/.flow/bin/f}")"

  # Fish
  if [ -f "$HOME/.config/fish/config.fish" ]; then
    if ! grep -q ".flow/bin" "$HOME/.config/fish/config.fish" 2>/dev/null; then
      echo "fish_add_path $install_dir" >> "$HOME/.config/fish/config.fish"
      info "flow: added to ~/.config/fish/config.fish"
    fi
  fi

  # Zsh
  if [ -f "$HOME/.zshrc" ]; then
    if ! grep -q ".flow/bin" "$HOME/.zshrc" 2>/dev/null; then
      echo "export PATH=\"$install_dir:\$PATH\"" >> "$HOME/.zshrc"
      info "flow: added to ~/.zshrc"
    fi
  fi

  # Bash
  for rc in "$HOME/.bashrc" "$HOME/.bash_profile"; do
    if [ -f "$rc" ]; then
      if ! grep -q ".flow/bin" "$rc" 2>/dev/null; then
        echo "export PATH=\"$install_dir:\$PATH\"" >> "$rc"
        info "flow: added to $rc"
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
