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

installed=false
for dir in "${INSTALL_DIRS[@]}"; do
    [[ -d "${dir}" ]] || continue

    # Copy binaries (more reliable than symlinks)
    cp -f "${SOURCE_F}" "${dir}/f" 2>/dev/null && installed=true
    cp -f "${SOURCE_F}" "${dir}/flow" 2>/dev/null || true
    cp -f "${SOURCE_LIN}" "${dir}/lin" 2>/dev/null || true
done

if ! $installed; then
    mkdir -p "${HOME}/bin"
    cp -f "${SOURCE_F}" "${HOME}/bin/f"
    cp -f "${SOURCE_F}" "${HOME}/bin/flow"
    cp -f "${SOURCE_LIN}" "${HOME}/bin/lin"
fi

# Verify
if command -v f &>/dev/null; then
    echo "flow ${PROFILE} build installed"
else
    echo "Installed to ~/bin - add to PATH: export PATH=\"\$HOME/bin:\$PATH\""
fi
