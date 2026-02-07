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

# Codesign source binaries so all copies inherit the signature
source "${HOME}/.config/flow/codesign.sh" 2>/dev/null || true
flow_codesign "$SOURCE_F" 2>/dev/null || true
flow_codesign "$SOURCE_LIN" 2>/dev/null || true

PRIMARY_DIR="${HOME}/bin"
ALT_DIR="${HOME}/.local/bin"
PRIMARY_F="$(command -v f 2>/dev/null || true)"
PRIMARY_INSTALLED=false

if [[ -n "${PRIMARY_F}" ]]; then
    PRIMARY_DIR="$(dirname -- "${PRIMARY_F}")"
fi

install_to_dir() {
    local dir="$1"
    [[ -d "${dir}" ]] || return 0
    [[ -w "${dir}" ]] || return 0

    # Copy binaries (more reliable than symlinks)
    if [[ -e "${dir}/f" && "${SOURCE_F}" -ef "${dir}/f" ]]; then
        :
    else
        cp -f "${SOURCE_F}" "${dir}/f" 2>/dev/null || return 1
    fi
    if [[ -e "${dir}/flow" && "${SOURCE_F}" -ef "${dir}/flow" ]]; then
        :
    else
        cp -f "${SOURCE_F}" "${dir}/flow" 2>/dev/null || true
    fi
    if [[ -e "${dir}/lin" && "${SOURCE_LIN}" -ef "${dir}/lin" ]]; then
        :
    else
        cp -f "${SOURCE_LIN}" "${dir}/lin" 2>/dev/null || true
    fi
    return 0
}

mkdir -p "${PRIMARY_DIR}"
if install_to_dir "${PRIMARY_DIR}"; then
    PRIMARY_INSTALLED=true
fi

# If ~/.local/bin exists, link to the primary install for consistency.
if [[ -d "${ALT_DIR}" ]]; then
    ln -sf "${PRIMARY_DIR}/f" "${ALT_DIR}/f"
    ln -sf "${PRIMARY_DIR}/f" "${ALT_DIR}/flow"
    ln -sf "${PRIMARY_DIR}/lin" "${ALT_DIR}/lin"
fi

# Verify
if command -v f &>/dev/null; then
    echo "flow ${PROFILE} build installed"
else
    echo "Installed to ~/bin - add to PATH: export PATH=\"\$HOME/bin:\$PATH\""
fi
