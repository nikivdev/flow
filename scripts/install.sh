#!/usr/bin/env bash
set -euo pipefail

# Installs flow/f to the current user. Usage:
#   curl -fsSL https://raw.githubusercontent.com/nikivdev/flow/main/scripts/install.sh | bash
# Customize with:
#   FLOW_INSTALL_ROOT=/usr/local         # overrides install prefix (default: ~/.local)
#   FLOW_BIN_DIR=/usr/local/bin          # overrides bin dir (defaults to <root>/bin)
#   FLOW_REF=<git ref>                   # install a specific commit/tag/branch (default: main)
#   FLOW_REPO_URL=<repo url>             # override repo (default: https://github.com/nikivdev/flow)
#   FLOW_BINARY_URL=<url>                # skip build; download a prebuilt f binary
#   FLOW_INSTALL_LIN=0                   # skip installing the lin helper binary

REPO_URL="${FLOW_REPO_URL:-https://github.com/nikivdev/flow}"
REF="${FLOW_REF:-main}"
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

is_github_repo() {
    [[ "${REPO_URL}" =~ ^https://github.com/[^/]+/[^/]+/?$ ]]
}

download_source_tarball() {
    need_cmd curl
    need_cmd tar
    local dest="$1"

    local repo="$REPO_URL"
    repo="${repo%/}"
    local tar_url=""
    if is_github_repo; then
        # Use codeload to avoid git auth prompts.
        local owner_repo="${repo#https://github.com/}"
        owner_repo="${owner_repo%.git}"
        local owner="${owner_repo%%/*}"
        local repo_name="${owner_repo#*/}"
        if [[ -z "${owner}" || -z "${repo_name}" || "${repo_name}" == "${owner_repo}" ]]; then
            fail "could not parse owner/repo from ${REPO_URL}"
        fi
        tar_url="https://codeload.github.com/${owner}/${repo_name}/tar.gz/${REF}"
    else
        fail "FLOW_REPO_URL must be a GitHub https URL when not using FLOW_BINARY_URL (got ${REPO_URL})"
    fi

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
    local args=(install --locked --force --path "${tmp}" --root "${INSTALL_ROOT}" --bin f)
    if [[ "${INSTALL_LIN}" != "0" ]]; then
        args+=(--bin lin)
    fi

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
