#!/bin/sh
# Allow `curl ... | sh` while still running the installer in bash.
if [ -z "${BASH_VERSION:-}" ]; then
    if ! command -v bash >/dev/null 2>&1; then
        echo "flow installer: bash is required. Install bash, then rerun the installer." >&2
        exit 1
    fi
    case "${0:-}" in
        sh|-sh|dash|-dash|*/sh|*/dash)
            tmp="$(mktemp "${TMPDIR:-/tmp}/flow-install.XXXXXX.bash")" || {
                echo "flow installer: failed to create temp file" >&2
                exit 1
            }
            cat > "${tmp}"
            FLOW_INSTALL_SCRIPT_TMP="${tmp}" exec bash "${tmp}" "$@"
            ;;
        *)
            exec bash "$0" "$@"
            ;;
    esac
fi

set -euo pipefail

if [[ -n "${FLOW_INSTALL_SCRIPT_TMP:-}" ]]; then
    trap 'rm -f "${FLOW_INSTALL_SCRIPT_TMP}"' EXIT
fi

# Installs flow + f to the current user. Usage:
#   curl -fsSL https://raw.githubusercontent.com/nikivdev/flow/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/nikivdev/flow/main/scripts/install.sh | bash
# Customize with:
#   FLOW_INSTALL_ROOT=/usr/local         # overrides install prefix (default: ~/.local)
#   FLOW_BIN_DIR=/usr/local/bin          # overrides bin dir (defaults to <root>/bin)
#   FLOW_VERSION=<tag>                   # release version to fetch (default: latest release)
#   FLOW_REF=<git ref>                   # fallback git ref for source build (default: main)
#   FLOW_REPO_URL=<repo url>             # override repo (default: https://github.com/nikivdev/flow)
#   FLOW_RELEASE_BASE=<base url>         # override release base (default: GitHub releases)
#   FLOW_BINARY_URL=<url>                # skip build; download a prebuilt f binary
#   FLOW_REGISTRY_URL=<url>              # install from Flow registry (e.g., https://myflow.sh)
#   FLOW_REGISTRY_PACKAGE=<name>         # registry package name (default: flow)
#   FLOW_INSTALL_LIN=0                   # skip installing the lin helper binary
#   FLOW_BOOTSTRAP_TOOLS="rise seq seqd" # install additional tools via `f install` after flow
#   FLOW_BOOTSTRAP_INSTALL_PARM=1         # auto-install parm before tool bootstrap
#   FLOW_NO_RELEASE=1                    # force source build even if a release exists
#   FLOW_DEV=1                           # dev install: clone to ~/code/org/1f/flow with jazz
#   FLOW_SKIP_DEPS=1                     # skip installing dependencies (brew, fnm, node, bun, rust)

REPO_URL="${FLOW_REPO_URL:-https://github.com/nikivdev/flow}"
JAZZ_REPO_URL="${FLOW_JAZZ_URL:-https://github.com/1focus-ai/jazz}"
REF="${FLOW_REF:-main}"
INSTALL_LIN="${FLOW_INSTALL_LIN:-1}"
REGISTRY_URL="${FLOW_REGISTRY_URL:-}"
REGISTRY_PACKAGE="${FLOW_REGISTRY_PACKAGE:-flow}"
DEV_INSTALL="${FLOW_DEV:-}"
SKIP_DEPS="${FLOW_SKIP_DEPS:-}"
RELEASE_ONLY="${FLOW_RELEASE_ONLY:-}"
BOOTSTRAP_TOOLS="${FLOW_BOOTSTRAP_TOOLS:-rise seq seqd}"
BOOTSTRAP_INSTALL_PARM="${FLOW_BOOTSTRAP_INSTALL_PARM:-1}"
FLOW_INSTALLED=0
RESOLVED_VERSION=""
OS_NAME=""
ARCH_NAME=""
OWNER=""
REPO_NAME=""

# Dev install paths
DEV_BASE="$HOME/code/org/1f"
DEV_FLOW_DIR="$DEV_BASE/flow"
DEV_JAZZ_DIR="$DEV_BASE/jazz"

fail() {
    echo "flow installer: $*" >&2
    exit 1
}

info() {
    echo "flow installer: $*"
}

# =============================================================================
# Dependency Installation
# =============================================================================

install_homebrew() {
    if command -v brew &>/dev/null; then
        info "Homebrew already installed"
        return 0
    fi

    info "Installing Homebrew..."
    /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

    # Add brew to PATH for this session
    if [[ -f "/opt/homebrew/bin/brew" ]]; then
        eval "$(/opt/homebrew/bin/brew shellenv)"
    elif [[ -f "/usr/local/bin/brew" ]]; then
        eval "$(/usr/local/bin/brew shellenv)"
    fi
}

install_fnm() {
    if command -v fnm &>/dev/null; then
        info "fnm already installed"
        return 0
    fi

    info "Installing fnm (Fast Node Manager)..."
    brew install fnm

    # Initialize fnm for this session
    eval "$(fnm env)"
}

install_node() {
    # Check if node is available via fnm
    if command -v fnm &>/dev/null; then
        if fnm list 2>/dev/null | grep -q "v"; then
            info "Node.js already installed via fnm"
            eval "$(fnm env)"
            return 0
        fi

        info "Installing Node.js LTS via fnm..."
        fnm install --lts
        fnm default lts-latest
        eval "$(fnm env)"
        return 0
    fi

    # Fallback: check if node exists
    if command -v node &>/dev/null; then
        info "Node.js already installed"
        return 0
    fi

    fail "fnm not available and node not found"
}

install_bun() {
    if command -v bun &>/dev/null; then
        info "Bun already installed"
        return 0
    fi

    info "Installing Bun..."
    curl -fsSL https://bun.sh/install | bash

    # Add bun to PATH for this session
    export BUN_INSTALL="$HOME/.bun"
    export PATH="$BUN_INSTALL/bin:$PATH"
}

install_rust() {
    if command -v cargo &>/dev/null; then
        info "Rust already installed"
        return 0
    fi

    info "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

    # Add cargo to PATH for this session
    source "$HOME/.cargo/env"
}

install_gh() {
    if command -v gh &>/dev/null; then
        info "GitHub CLI already installed"
        return 0
    fi

    info "Installing GitHub CLI..."
    brew install gh
}

install_fzf() {
    if command -v fzf &>/dev/null; then
        info "fzf already installed"
        return 0
    fi

    info "Installing fzf..."
    brew install fzf
}

install_all_deps() {
    if [[ -n "${SKIP_DEPS}" ]]; then
        info "Skipping dependency installation (FLOW_SKIP_DEPS=1)"
        return 0
    fi

    info ""
    info "=== Installing Dependencies ==="
    info ""

    install_homebrew
    install_fnm
    install_node
    install_bun
    install_rust
    install_gh
    install_fzf

    info ""
    info "=== Dependencies installed ==="
    info ""
}

resolve_paths() {
    local root="${FLOW_INSTALL_ROOT:-}"
    local bin="${FLOW_BIN_DIR:-}"

    if [[ -n "${root}" && -n "${bin}" ]]; then
        root="${root%/}"
        bin="${bin%/}"
        if [[ "${bin}" != "${root}/bin" ]]; then
            fail "FLOW_INSTALL_ROOT (${root}) and FLOW_BIN_DIR (${bin}) must align (expected ${root}/bin)."
        fi
    fi

    if [[ -z "${root}" && -z "${bin}" ]]; then
        root="$HOME/.local"
        bin="${root}/bin"
    elif [[ -z "${root}" ]]; then
        bin="${bin%/}"
        root="$(dirname "${bin}")"
    elif [[ -z "${bin}" ]]; then
        root="${root%/}"
        bin="${root}/bin"
    else
        root="${root%/}"
        bin="${bin%/}"
    fi

    INSTALL_ROOT="${root}"
    BIN_DIR="${bin}"
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

detect_platform() {
    local uname_s uname_m
    uname_s="$(uname -s)"
    uname_m="$(uname -m)"

    case "${uname_s}" in
        Darwin) OS_NAME="darwin" ;;
        Linux) OS_NAME="linux" ;;
        *) fail "unsupported OS: ${uname_s}" ;;
    esac

    case "${uname_m}" in
        arm64|aarch64) ARCH_NAME="arm64" ;;
        x86_64|amd64) ARCH_NAME="amd64" ;;
        *) fail "unsupported architecture: ${uname_m}" ;;
    esac
}

parse_repo_url() {
    local repo="${REPO_URL%/}"
    repo="${repo%.git}"
    case "${repo}" in
        https://github.com/*/*)
            repo="${repo#https://github.com/}"
            OWNER="${repo%%/*}"
            REPO_NAME="${repo#*/}"
            if [[ -z "${OWNER}" || -z "${REPO_NAME}" || "${REPO_NAME}" == "${repo}" ]]; then
                fail "could not parse owner/repo from ${REPO_URL}"
            fi
            ;;
        *)
            fail "FLOW_REPO_URL must be a GitHub https URL when not using FLOW_BINARY_URL (got ${REPO_URL})"
            ;;
    esac
}

install_from_binary_url() {
    local url="$1"
    need_cmd curl

    info "Downloading flow from ${url}"
    mkdir -p "${BIN_DIR}"
    curl -fsSL "${url}" -o "${BIN_DIR}/f"
    chmod +x "${BIN_DIR}/f"
}

resolve_release_version() {
    if [[ -n "${FLOW_VERSION:-}" ]]; then
        RESOLVED_VERSION="${FLOW_VERSION}"
        return
    fi

    if ! command -v curl >/dev/null 2>&1; then
        return
    fi

    local api="https://api.github.com/repos/${OWNER}/${REPO_NAME}/releases/latest"
    local tag
    tag="$(curl -fsSL "${api}" 2>/dev/null | sed -n 's/  *\"tag_name\" *: *\"\\(.*\\)\".*/\\1/p' | head -n1 || true)"
    if [[ -n "${tag}" ]]; then
        RESOLVED_VERSION="${tag}"
    fi
}

resolve_registry_version() {
    if [[ -n "${FLOW_VERSION:-}" ]]; then
        RESOLVED_VERSION="${FLOW_VERSION}"
        return 0
    fi

    local url="${REGISTRY_URL%/}/packages/${REGISTRY_PACKAGE}/latest.json"
    local manifest
    manifest="$(curl -fsSL "${url}" 2>/dev/null || true)"
    if [[ -z "${manifest}" ]]; then
        return 1
    fi

    local version
    version="$(echo "${manifest}" | sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1 || true)"
    if [[ -z "${version}" ]]; then
        return 1
    fi

    RESOLVED_VERSION="${version}"
    return 0
}

install_from_registry() {
    local registry="${REGISTRY_URL%/}"
    if [[ -z "${registry}" ]]; then
        return 1
    fi

    need_cmd curl

    if ! resolve_registry_version; then
        info "Failed to resolve latest registry version"
        return 1
    fi

    local target=""
    if [[ "${OS_NAME}" == "darwin" ]]; then
        target="${ARCH_NAME}-apple-darwin"
    elif [[ "${OS_NAME}" == "linux" ]]; then
        target="${ARCH_NAME}-unknown-linux-gnu"
    else
        fail "unsupported OS for registry install: ${OS_NAME}"
    fi

    mkdir -p "${BIN_DIR}"
    local bins=("${REGISTRY_PACKAGE}")
    if [[ "${REGISTRY_PACKAGE}" == "flow" ]]; then
        bins=("f" "flow")
        if [[ "${INSTALL_LIN}" != "0" ]]; then
            bins+=("lin")
        fi
    fi

    local installed=0
    for bin in "${bins[@]}"; do
        local url="${registry}/packages/${REGISTRY_PACKAGE}/${RESOLVED_VERSION}/${target}/${bin}"
        info "Downloading ${bin} from ${url}"
        if curl -fsSL "${url}" -o "${BIN_DIR}/${bin}"; then
            chmod +x "${BIN_DIR}/${bin}"
            installed=1
            if [[ "${bin}" == "flow" ]]; then
                FLOW_INSTALLED=1
            fi
        else
            info "Failed to download ${bin} from registry"
        fi
    done

    if [[ "${installed}" -eq 0 ]]; then
        info "Registry install failed"
        return 1
    fi

    if [[ "${REGISTRY_PACKAGE}" == "flow" ]]; then
        ensure_aliases
    fi

    info "Installed ${REGISTRY_PACKAGE} ${RESOLVED_VERSION} from registry"
    return 0
}

install_from_release() {
    local version="$1"
    local asset="flow_${version}_${OS_NAME}_${ARCH_NAME}.tar.gz"
    local base="${FLOW_RELEASE_BASE:-https://github.com/${OWNER}/${REPO_NAME}/releases/download}"
    local url="${base}/${version}/${asset}"

    need_cmd curl
    need_cmd tar

    info "Downloading release ${version} (${OS_NAME}/${ARCH_NAME})"
    local tmp_tar
    tmp_tar="$(mktemp)" || fail "failed to create temp file"
    if ! curl -fsSL "${url}" -o "${tmp_tar}"; then
        info "Release download failed; tried ${url}"
        rm -f "${tmp_tar}"
        return 1
    fi

    local tmp_dir
    tmp_dir="$(mktemp -d)" || fail "failed to create temp dir"

    if ! tar -xzf "${tmp_tar}" -C "${tmp_dir}"; then
        info "Failed to unpack release tarball"
        rm -rf "${tmp_dir}" "${tmp_tar}"
        return 1
    fi

    local extracted
    extracted="$(find "${tmp_dir}" -mindepth 1 -maxdepth 1 -type d | head -n1)"
    [[ -z "${extracted}" ]] && extracted="${tmp_dir}"

    mkdir -p "${BIN_DIR}"
    local copied=0
    for bin in f flow lin; do
        if [[ -f "${extracted}/${bin}" ]]; then
            cp "${extracted}/${bin}" "${BIN_DIR}/${bin}"
            chmod +x "${BIN_DIR}/${bin}"
            copied=1
            if [[ "${bin}" == "flow" ]]; then
                FLOW_INSTALLED=1
            fi
        fi
    done

    rm -rf "${tmp_dir}" "${tmp_tar}"

    if [[ "${copied}" -eq 0 ]]; then
        info "Release tarball did not contain expected binaries"
        return 1
    fi

    info "Installed release ${version} to ${BIN_DIR}"
    return 0
}

download_source_tarball() {
    need_cmd curl
    need_cmd tar
    local dest="$1"

    local tar_url="https://codeload.github.com/${OWNER}/${REPO_NAME}/tar.gz/${REF}"

    info "Downloading source tarball ${tar_url}"
    mkdir -p "${dest}"
    curl -fsSL "${tar_url}" | tar -xz -C "${dest}" --strip-components=1
}

install_from_source() {
    need_cmd cargo
    mkdir -p "${BIN_DIR}"

    local tmp
    tmp="$(mktemp -d)" || fail "failed to create temp dir"
    trap 'rm -rf "${tmp}"' EXIT

    download_source_tarball "${tmp}"

    info "Building flow from source with cargo (this may take a moment)..."
    local args=(install --locked --force --path "${tmp}" --root "${INSTALL_ROOT}" --bin f --bin flow)
    if [[ "${INSTALL_LIN}" != "0" ]]; then
        args+=(--bin lin)
    fi

    cargo "${args[@]}"
    if [[ -x "${BIN_DIR}/flow" ]]; then
        FLOW_INSTALLED=1
    fi
}

ensure_aliases() {
    local target="${BIN_DIR}/f"
    [[ -x "${target}" ]] || fail "expected ${target} after install"

    if [[ "${FLOW_INSTALLED}" -eq 1 ]]; then
        return 0
    fi

    ln -sf "${target}" "${BIN_DIR}/flow"
}

ensure_path_hint() {
    case ":$PATH:" in
        *":${BIN_DIR}:"*)
            ;;
        *)
            info "Add ${BIN_DIR} to your PATH, e.g. append: export PATH=\"${BIN_DIR}:\$PATH\""
            ;;
    esac
}

install_parm_if_needed() {
    if command -v parm >/dev/null 2>&1; then
        return 0
    fi
    if [[ "${BOOTSTRAP_INSTALL_PARM}" == "0" ]]; then
        return 1
    fi
    if ! command -v curl >/dev/null 2>&1; then
        info "curl missing; cannot auto-install parm."
        return 1
    fi
    info "Installing parm for robust GitHub fallback..."
    if curl -fsSL https://raw.githubusercontent.com/yhoundz/parm/master/scripts/install.sh | sh; then
        export PATH="$HOME/.local/bin:$HOME/bin:$PATH"
        return 0
    fi
    info "parm install failed; continuing with registry/flox-only bootstrap."
    return 1
}

bootstrap_core_tools() {
    local fbin="${BIN_DIR}/f"
    if [[ ! -x "${fbin}" ]]; then
        return 0
    fi
    if [[ -z "${BOOTSTRAP_TOOLS}" || "${BOOTSTRAP_TOOLS}" == "0" || "${BOOTSTRAP_TOOLS}" == "false" ]]; then
        return 0
    fi

    info ""
    info "=== Bootstrap Core Tools ==="
    info ""

    install_parm_if_needed || true

    local failures=0
    local tool=""
    for tool in ${BOOTSTRAP_TOOLS}; do
        info "Bootstrapping ${tool}..."
        if "${fbin}" install "${tool}" --backend auto --bin-dir "${BIN_DIR}" --force; then
            :
        else
            info "WARN failed to bootstrap ${tool}. Retry later with: f install ${tool} --backend auto"
            failures=$((failures + 1))
        fi
    done

    if [[ "${failures}" -eq 0 ]]; then
        info "Bootstrap complete: ${BOOTSTRAP_TOOLS}"
    else
        info "Bootstrap completed with ${failures} warning(s)."
    fi
}

# Dev install: clone repos to ~/code/org/1f/ and build from source
install_dev() {
    # Install all dependencies first
    install_all_deps

    info ""
    info "=== Dev Install: Setting up in ${DEV_BASE} ==="
    info ""
    mkdir -p "${DEV_BASE}"

    # Clone or update jazz
    if [[ -d "${DEV_JAZZ_DIR}" ]]; then
        info "Jazz directory exists, updating..."
        (cd "${DEV_JAZZ_DIR}" && git pull --rebase) || true
    else
        info "Cloning jazz to ${DEV_JAZZ_DIR}..."
        git clone "${JAZZ_REPO_URL}" "${DEV_JAZZ_DIR}"
    fi

    # Clone or update flow
    if [[ -d "${DEV_FLOW_DIR}" ]]; then
        info "Flow directory exists, updating..."
        (cd "${DEV_FLOW_DIR}" && git pull --rebase) || true
    else
        info "Cloning flow to ${DEV_FLOW_DIR}..."
        git clone "${REPO_URL}" "${DEV_FLOW_DIR}"
    fi

    # Create .cargo/config.toml for local path overrides
    mkdir -p "${DEV_FLOW_DIR}/.cargo"
    cat > "${DEV_FLOW_DIR}/.cargo/config.toml" << EOF
# Local path overrides for faster builds (auto-generated by install.sh)
[patch."https://github.com/1focus-ai/jazz"]
groove = { path = "${DEV_JAZZ_DIR}/crates/groove" }
groove-rocksdb = { path = "${DEV_JAZZ_DIR}/crates/groove-rocksdb" }
EOF
    info "Created .cargo/config.toml with local jazz paths"

    # Build
    info "Building flow from source..."
    (cd "${DEV_FLOW_DIR}" && cargo build --release)

    # Setup symlinks
    mkdir -p "${BIN_DIR}"
    ln -sf "${DEV_FLOW_DIR}/target/release/f" "${BIN_DIR}/f"
    if [[ -f "${DEV_FLOW_DIR}/target/release/flow" ]]; then
        ln -sf "${DEV_FLOW_DIR}/target/release/flow" "${BIN_DIR}/flow"
    else
        ln -sf "${DEV_FLOW_DIR}/target/release/f" "${BIN_DIR}/flow"
    fi
    if [[ "${INSTALL_LIN}" != "0" && -f "${DEV_FLOW_DIR}/target/release/lin" ]]; then
        ln -sf "${DEV_FLOW_DIR}/target/release/lin" "${BIN_DIR}/lin"
    fi

    info "Symlinked binaries to ${BIN_DIR}"
    info ""
    info "Dev install complete!"
    info "  Flow: ${DEV_FLOW_DIR}"
    info "  Jazz: ${DEV_JAZZ_DIR}"
    info "  Binaries: ${BIN_DIR}/f, ${BIN_DIR}/flow"
}

main() {
    resolve_paths
    detect_platform

    info ""
    info "=== Flow Installer ==="
    info ""

    # Dev install mode
    if [[ -n "${DEV_INSTALL}" ]]; then
        install_dev
        ensure_path_hint
        print_shell_setup
        info ""
        info "Done. Launch with \"flow --help\" or \"f --help\"."
        return
    fi

    parse_repo_url
    info "Installing to ${BIN_DIR}"

    if [[ -n "${FLOW_BINARY_URL:-}" ]]; then
        install_from_binary_url "${FLOW_BINARY_URL}"
    elif [[ -n "${REGISTRY_URL}" ]]; then
        if ! install_from_registry; then
            info "Registry install failed; falling back to release/source."
            REGISTRY_URL=""
        else
            ensure_aliases
            bootstrap_core_tools
            ensure_path_hint
            info "Done. Launch with \"flow --help\" or \"f --help\"."
            return
        fi
    elif [[ -z "${FLOW_NO_RELEASE:-}" ]]; then
        resolve_release_version
        if [[ -n "${RESOLVED_VERSION}" ]] && install_from_release "${RESOLVED_VERSION}"; then
            :
        else
            if [[ -n "${RELEASE_ONLY}" ]]; then
                fail "release not found or unavailable (FLOW_RELEASE_ONLY=1)"
            fi
            info "Falling back to source build (release not found or unavailable)."
            install_all_deps
            install_from_source
        fi
    else
        install_all_deps
        install_from_source
    fi

    ensure_aliases
    bootstrap_core_tools
    ensure_path_hint

    info "Done. Launch with \"flow --help\" or \"f --help\"."
}

print_shell_setup() {
    info ""
    info "=== Shell Setup ==="
    info ""
    info "Add these to your shell config:"
    info ""
    if [[ -f "$HOME/.config/fish/config.fish" ]]; then
        info "# Fish (~/.config/fish/config.fish):"
        info 'set -gx PATH $HOME/.local/bin $PATH'
        info ''
        info '# fnm (Node.js)'
        info 'fnm env | source'
        info ''
        info '# Bun'
        info 'set -gx BUN_INSTALL $HOME/.bun'
        info 'set -gx PATH $BUN_INSTALL/bin $PATH'
        info ''
        info '# Flow function'
        info 'function f'
        info '    if test -z "$argv[1]"'
        info '        ~/bin/f'
        info '    else'
        info '        ~/bin/f match $argv'
        info '    end'
        info 'end'
    else
        info "# Bash/Zsh (~/.bashrc or ~/.zshrc):"
        info 'export PATH="$HOME/.local/bin:$PATH"'
        info ''
        info '# fnm (Node.js)'
        info 'eval "$(fnm env)"'
        info ''
        info '# Bun'
        info 'export BUN_INSTALL="$HOME/.bun"'
        info 'export PATH="$BUN_INSTALL/bin:$PATH"'
        info ''
        info '# Flow function'
        info 'f() {'
        info '    if [ -z "$1" ]; then'
        info '        ~/bin/f'
        info '    else'
        info '        ~/bin/f match "$@"'
        info '    fi'
        info '}'
    fi
}

main "$@"
