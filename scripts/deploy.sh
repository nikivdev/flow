#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

PROFILE="${FLOW_PROFILE:-debug}"
TARGET_DIR="debug"
BUILD_ARGS=()
[[ "${PROFILE}" == "release" ]] && TARGET_DIR="release" && BUILD_ARGS+=("--release")

# Build
cargo build "${BUILD_ARGS[@]}" --quiet

SOURCE_F="${ROOT_DIR}/target/${TARGET_DIR}/f"
SOURCE_LIN="${ROOT_DIR}/target/${TARGET_DIR}/lin"

# Install locations (primary + fallback)
INSTALL_DIRS=("${HOME}/bin" "${HOME}/.local/bin")
PRIMARY_F="$(command -v f 2>/dev/null || true)"
PRIMARY_DIR=""
PRIMARY_INSTALLED=false

if [[ -n "${PRIMARY_F}" ]]; then
    PRIMARY_DIR="$(dirname -- "${PRIMARY_F}")"
fi

install_to_dir() {
    local dir="$1"
    [[ -d "${dir}" ]] || return 0
    [[ -w "${dir}" ]] || return 0

    # Copy binaries (more reliable than symlinks)
    cp -f "${SOURCE_F}" "${dir}/f" 2>/dev/null || return 1
    cp -f "${SOURCE_F}" "${dir}/flow" 2>/dev/null || true
    cp -f "${SOURCE_LIN}" "${dir}/lin" 2>/dev/null || true
    return 0
}

installed=false

if [[ -n "${PRIMARY_DIR}" ]]; then
    if install_to_dir "${PRIMARY_DIR}"; then
        installed=true
        PRIMARY_INSTALLED=true
    fi
fi

for dir in "${INSTALL_DIRS[@]}"; do
    [[ "${dir}" == "${PRIMARY_DIR}" ]] && continue
    if install_to_dir "${dir}"; then
        installed=true
    fi
done

if ! $installed; then
    mkdir -p "${HOME}/bin"
    cp -f "${SOURCE_F}" "${HOME}/bin/f"
    cp -f "${SOURCE_F}" "${HOME}/bin/flow"
    cp -f "${SOURCE_LIN}" "${HOME}/bin/lin"
fi

# Verify
if command -v f &>/dev/null; then
    if [[ -n "${PRIMARY_F}" && "${PRIMARY_INSTALLED}" == "false" ]]; then
        echo "flow ${PROFILE} build installed, but ${PRIMARY_F} was not updated."
        echo "Ensure ${PRIMARY_DIR} is writable or run: cp \"${SOURCE_F}\" \"${PRIMARY_F}\""
    else
        echo "flow ${PROFILE} build installed"
    fi
else
    echo "Installed to ~/bin - add to PATH: export PATH=\"\$HOME/bin:\$PATH\""
fi
