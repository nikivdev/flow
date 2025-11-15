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
TARGET_BIN="${INSTALL_DIR}/f"

echo "Linking ${TARGET_BIN} -> ${SOURCE_BIN}"
rm -f "${TARGET_BIN}"
ln -s "${SOURCE_BIN}" "${TARGET_BIN}"

echo "Symlinked f to ${TARGET_BIN}"
echo "Ensure ${INSTALL_DIR} is on your PATH to run 'f' from anywhere."
