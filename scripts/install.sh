#!/usr/bin/env bash
set -euo pipefail

# Installs flow/f to the current user. Usage:
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
#   FLOW_NO_RELEASE=1                    # force source build even if a release exists

REPO_URL="${FLOW_REPO_URL:-https://github.com/nikivdev/flow}"
REF="${FLOW_REF:-main}"
INSTALL_LIN="${FLOW_INSTALL_LIN:-1}"
REGISTRY_URL="${FLOW_REGISTRY_URL:-}"
REGISTRY_PACKAGE="${FLOW_REGISTRY_PACKAGE:-flow}"
RESOLVED_VERSION=""
OS_NAME=""
ARCH_NAME=""
OWNER=""
REPO_NAME=""

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
        bins=("f")
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
    for bin in f lin; do
        if [[ -f "${extracted}/${bin}" ]]; then
            cp "${extracted}/${bin}" "${BIN_DIR}/${bin}"
            chmod +x "${BIN_DIR}/${bin}"
            copied=1
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
    detect_platform
    parse_repo_url
    info "Installing to ${BIN_DIR}"

    if [[ -n "${FLOW_BINARY_URL:-}" ]]; then
        install_from_binary_url "${FLOW_BINARY_URL}"
    elif [[ -n "${REGISTRY_URL}" ]]; then
        if ! install_from_registry; then
            info "Registry install failed; falling back to release/source."
            REGISTRY_URL=""
        else
            ensure_path_hint
            info "Done. Launch with \"flow --help\" or \"f --help\"."
            return
        fi
    elif [[ -z "${FLOW_NO_RELEASE:-}" ]]; then
        resolve_release_version
        if [[ -n "${RESOLVED_VERSION}" ]] && install_from_release "${RESOLVED_VERSION}"; then
            :
        else
            info "Falling back to source build (release not found or unavailable)."
            install_from_source
        fi
    else
        install_from_source
    fi

    ensure_aliases
    ensure_path_hint

    info "Done. Launch with \"flow --help\" or \"f --help\"."
}

main "$@"
