#!/usr/bin/env bash
set -euo pipefail

# Installs flow/f to the current user. Usage:
#   curl -fsSL https://raw.githubusercontent.com/nikiv/flow/main/scripts/install.sh | bash
# Customize with:
#   FLOW_INSTALL_ROOT=/usr/local         # overrides install prefix (default: ~/.local)
#   FLOW_BIN_DIR=/usr/local/bin          # overrides bin dir (defaults to <root>/bin)
#   FLOW_REF=<git ref>                   # install a specific commit/tag/branch
#   FLOW_REPO_URL=<repo url>             # override repo (default: https://github.com/nikivdev/flow)
#   FLOW_BINARY_URL=<url>                # skip build; download a prebuilt f binary
#   FLOW_INSTALL_LIN=0                   # skip installing the lin helper binary

REPO_URL="${FLOW_REPO_URL:-https://github.com/nikivdev/flow}"
REF="${FLOW_REF:-}"
INSTALL_LIN="${FLOW_INSTALL_LIN:-1}"

fail() {
    echo "flow installer: $*" >&2
    exit 1
}

info() {
    echo "flow installer: $*"
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

install_from_binary_url() {
    local url="$1"
    need_cmd curl

    info "Downloading flow from ${url}"
    mkdir -p "${BIN_DIR}"
    curl -fsSL "${url}" -o "${BIN_DIR}/f"
    chmod +x "${BIN_DIR}/f"
}

install_from_source() {
    need_cmd curl
    need_cmd git
    need_cmd cargo

    mkdir -p "${BIN_DIR}"
    info "Building flow from source with cargo (this may take a moment)..."

    local args=(install --locked --force --git "${REPO_URL}" --root "${INSTALL_ROOT}" --bin f)
    if [[ "${INSTALL_LIN}" != "0" ]]; then
        args+=(--bin lin)
    fi
    if [[ -n "${REF}" ]]; then
        args+=(--rev "${REF}")
    fi

    export CARGO_NET_GIT_FETCH_WITH_CLI="${CARGO_NET_GIT_FETCH_WITH_CLI:-true}"
    cargo "${args[@]}"
}

ensure_aliases() {
    local target="${BIN_DIR}/f"
    [[ -x "${target}" ]] || fail "expected ${target} after install"

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

main() {
    resolve_paths
    info "Installing to ${BIN_DIR}"

    if [[ -n "${FLOW_BINARY_URL:-}" ]]; then
        install_from_binary_url "${FLOW_BINARY_URL}"
    else
        install_from_source
    fi

    ensure_aliases
    ensure_path_hint

    info "Done. Launch with \"flow --help\" or \"f --help\"."
}

main "$@"
