#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

cd "${ROOT_DIR}"

PROFILE="${FLOW_PROFILE:-debug}"
TARGET_DIR="debug"
BUILD_ARGS=()
if [ "${PROFILE}" = "release" ]; then
    TARGET_DIR="release"
    BUILD_ARGS+=("--release")
fi

echo "Building flow CLI and daemon (${PROFILE} profile)..."
if [ "${#BUILD_ARGS[@]}" -gt 0 ]; then
    cargo build "${BUILD_ARGS[@]}"
else
    cargo build
fi

INSTALL_DIR="${FLOW_INSTALL_DIR:-$HOME/bin}"
mkdir -p "${INSTALL_DIR}"

SOURCE_BIN="${ROOT_DIR}/target/${TARGET_DIR}/f"
TARGET_BIN_F="${INSTALL_DIR}/f"
TARGET_BIN_FLOW="${INSTALL_DIR}/flow"
SOURCE_LIN="${ROOT_DIR}/target/${TARGET_DIR}/lin"
TARGET_BIN_LIN="${INSTALL_DIR}/lin"

for target in "${TARGET_BIN_F}" "${TARGET_BIN_FLOW}"; do
    echo "Linking ${target} -> ${SOURCE_BIN}"
    rm -f "${target}"
    ln -s "${SOURCE_BIN}" "${target}"
done

echo "Linking ${TARGET_BIN_LIN} -> ${SOURCE_LIN}"
rm -f "${TARGET_BIN_LIN}"
ln -s "${SOURCE_LIN}" "${TARGET_BIN_LIN}"

echo "Symlinked CLI to ${TARGET_BIN_F} and ${TARGET_BIN_FLOW}"
echo "Symlinked watcher daemon to ${TARGET_BIN_LIN}"
echo "Ensure ${INSTALL_DIR} is on your PATH to run 'f', 'flow', or 'lin' from anywhere."
