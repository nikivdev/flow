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
SOURCE_IRIS="${ROOT_DIR}/target/${TARGET_DIR}/iris"
TARGET_BIN_IRIS="${INSTALL_DIR}/iris"

for target in "${TARGET_BIN_F}" "${TARGET_BIN_FLOW}"; do
    echo "Linking ${target} -> ${SOURCE_BIN}"
    rm -f "${target}"
    ln -s "${SOURCE_BIN}" "${target}"
done

echo "Linking ${TARGET_BIN_IRIS} -> ${SOURCE_IRIS}"
rm -f "${TARGET_BIN_IRIS}"
ln -s "${SOURCE_IRIS}" "${TARGET_BIN_IRIS}"

echo "Symlinked CLI to ${TARGET_BIN_F} and ${TARGET_BIN_FLOW}"
echo "Symlinked watcher daemon to ${TARGET_BIN_IRIS}"
echo "Ensure ${INSTALL_DIR} is on your PATH to run 'f', 'flow', or 'iris' from anywhere."
