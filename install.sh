#!/bin/bash
set -e

# Flow CLI installer
# Usage: curl -fsSL https://myflow.sh/install.sh | bash

INSTALL_DIR="${FLOW_INSTALL_DIR:-$HOME/.flow}"
BIN_DIR="$INSTALL_DIR/bin"
BINARY_NAME="f"
REPO="nikitavoloboev/flow"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}[INFO]${NC} $1"; }
success() { echo -e "${GREEN}[SUCCESS]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1" >&2; exit 1; }

# Detect platform
detect_platform() {
    local os arch

    case "$(uname -s)" in
        Darwin) os="apple-darwin" ;;
        Linux) os="unknown-linux-gnu" ;;
        *) error "Unsupported operating system: $(uname -s)" ;;
    esac

    case "$(uname -m)" in
        x86_64|amd64) arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) error "Unsupported architecture: $(uname -m)" ;;
    esac

    echo "${arch}-${os}"
}

# Get latest release version from GitHub
get_latest_version() {
    local url="https://api.github.com/repos/${REPO}/releases/latest"
    if command -v curl &> /dev/null; then
        curl -fsSL "$url" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'
    elif command -v wget &> /dev/null; then
        wget -qO- "$url" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/'
    else
        error "Neither curl nor wget found. Please install one of them."
    fi
}

# Download file
download() {
    local url="$1"
    local dest="$2"

    if command -v curl &> /dev/null; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget &> /dev/null; then
        wget -q "$url" -O "$dest"
    else
        error "Neither curl nor wget found."
    fi
}

# Configure shell PATH
configure_path() {
    local shell_config=""
    local path_line="export PATH=\"\$HOME/.flow/bin:\$PATH\""
    local fish_path_line="fish_add_path \$HOME/.flow/bin"

    # Fish
    if [ -f "$HOME/.config/fish/config.fish" ]; then
        if ! grep -q ".flow/bin" "$HOME/.config/fish/config.fish" 2>/dev/null; then
            echo "$fish_path_line" >> "$HOME/.config/fish/config.fish"
            info "Added PATH to ~/.config/fish/config.fish"
        else
            info "PATH already configured in ~/.config/fish/config.fish"
        fi
    fi

    # Bash
    for rc in "$HOME/.bashrc" "$HOME/.bash_profile"; do
        if [ -f "$rc" ]; then
            if ! grep -q ".flow/bin" "$rc" 2>/dev/null; then
                echo "$path_line" >> "$rc"
                info "Added PATH to $rc"
            else
                info "PATH already configured in $rc"
            fi
            break
        fi
    done

    # Zsh
    if [ -f "$HOME/.zshrc" ]; then
        if ! grep -q ".flow/bin" "$HOME/.zshrc" 2>/dev/null; then
            echo "$path_line" >> "$HOME/.zshrc"
            info "Added PATH to ~/.zshrc"
        else
            info "PATH already configured in ~/.zshrc"
        fi
    fi
}

main() {
    info "Starting Flow CLI installation..."

    # Detect platform
    local platform
    platform=$(detect_platform)
    info "Detected platform: $platform"

    # Get latest version
    info "Fetching latest version..."
    local version
    version=$(get_latest_version)
    if [ -z "$version" ]; then
        error "Failed to fetch latest version"
    fi
    info "Installing version: $version"

    # Create directories
    mkdir -p "$BIN_DIR"

    # Download binary
    local download_url="https://github.com/${REPO}/releases/download/${version}/flow-${platform}.tar.gz"
    local tmp_dir
    tmp_dir=$(mktemp -d)
    local archive="$tmp_dir/flow.tar.gz"

    info "Downloading Flow CLI..."
    download "$download_url" "$archive" || error "Failed to download from $download_url"

    # Extract
    info "Extracting..."
    tar -xzf "$archive" -C "$tmp_dir"

    # Find and install binary
    local binary
    binary=$(find "$tmp_dir" -name "f" -o -name "flow" | head -1)
    if [ -z "$binary" ]; then
        # Try looking for any executable
        binary=$(find "$tmp_dir" -type f -perm +111 | head -1)
    fi

    if [ -z "$binary" ]; then
        error "Could not find binary in archive"
    fi

    mv "$binary" "$BIN_DIR/$BINARY_NAME"
    chmod +x "$BIN_DIR/$BINARY_NAME"

    # Cleanup
    rm -rf "$tmp_dir"

    success "Flow CLI installed to $BIN_DIR/$BINARY_NAME"

    # Configure PATH
    configure_path

    echo ""
    success "Flow CLI installed successfully!"
    info "Run 'f --help' to get started (after restarting your shell)"
    info "Or run: export PATH=\"\$HOME/.flow/bin:\$PATH\""
    info "Visit https://myflow.sh for documentation"
}

main "$@"
