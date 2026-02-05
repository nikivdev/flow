#!/usr/bin/env bash
set -euo pipefail

# Build and package flow binaries into a tar.gz release artifact.
# Usage:
#   FLOW_VERSION=v0.1.0 CODESIGN_IDENTITY="Developer ID Application: Example (TEAMID)" scripts/package-release.sh
#
# Outputs:
#   dist/flow-<version>-<os>-<arch>.tar.gz
#   dist/flow-<version>-<os>-<arch>.tar.gz.sha256
# Contents:
#   f (binary), flow (binary), lin (binary)
# Notes:
    #   - macOS: if CODESIGN_IDENTITY is set, f, flow, and lin are codesigned (--timestamp --options runtime).
#   - Build is local-only; run on each target platform (macOS arm64/x86_64, Linux x86_64/aarch64).

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${ROOT_DIR}/dist"
PROFILE=release

fail() {
    echo "package-release: $*" >&2
    exit 1
}

info() {
    echo "package-release: $*"
}

detect_platform() {
    local os uname_s arch uname_m
    uname_s="$(uname -s)"
    uname_m="$(uname -m)"

    case "${uname_s}" in
        Darwin) os="darwin" ;;
        Linux) os="linux" ;;
        *) fail "unsupported OS: ${uname_s}" ;;
    esac

    case "${uname_m}" in
        arm64|aarch64) arch="arm64" ;;
        x86_64|amd64) arch="amd64" ;;
        *) fail "unsupported arch: ${uname_m}" ;;
    esac

    OS_NAME="${os}"
    ARCH_NAME="${arch}"
}

resolve_version() {
    if [[ -n "${FLOW_VERSION:-}" ]]; then
        VERSION="${FLOW_VERSION}"
        return
    fi

    if command -v git >/dev/null 2>&1; then
        VERSION="$(git -C "${ROOT_DIR}" describe --tags --always --dirty 2>/dev/null || true)"
    fi

    VERSION="${VERSION:-dev}"
}

codesign_if_requested() {
    local bin="$1"
    if [[ "${OS_NAME}" != "darwin" ]]; then
        return
    fi
    if [[ -z "${CODESIGN_IDENTITY:-}" ]]; then
        info "No CODESIGN_IDENTITY set; skipping codesign for ${bin}"
        return
    fi
    if ! command -v codesign >/dev/null 2>&1; then
        fail "codesign not found; install Xcode command line tools to sign"
    fi
    info "Codesigning ${bin}"
    codesign --force --timestamp --options runtime --sign "${CODESIGN_IDENTITY}" "${bin}"
}

checksum() {
    local file="$1"
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "${file}"
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${file}"
    else
        fail "neither shasum nor sha256sum found for checksumming"
    fi
}

main() {
    detect_platform
    resolve_version

    info "Building flow (version ${VERSION}, ${OS_NAME}/${ARCH_NAME})"
    cargo build --locked --release --bin f --bin flow --bin lin

    local stage="${DIST_DIR}/flow_${VERSION}_${OS_NAME}_${ARCH_NAME}"
    local target_dir="${ROOT_DIR}/target/${PROFILE}"
    rm -rf "${stage}"
    mkdir -p "${stage}"

    cp "${target_dir}/f" "${stage}/f"
    cp "${target_dir}/flow" "${stage}/flow"
    cp "${target_dir}/lin" "${stage}/lin"

    codesign_if_requested "${stage}/f"
    codesign_if_requested "${stage}/flow"
    codesign_if_requested "${stage}/lin"

    mkdir -p "${DIST_DIR}"
    local tarball="${DIST_DIR}/flow_${VERSION}_${OS_NAME}_${ARCH_NAME}.tar.gz"
    tar -C "${DIST_DIR}" -czf "${tarball}" "flow_${VERSION}_${OS_NAME}_${ARCH_NAME}"

    checksum "${tarball}" > "${tarball}.sha256"

    info "Built ${tarball}"
    info "Checksum written to ${tarball}.sha256"
    info "Upload these to the GitHub release for ${VERSION} so install.sh can fetch them."
}

main "$@"
