#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

cd "${ROOT_DIR}"

echo "Building flow CLI and daemon (debug profile)..."
cargo build

INSTALL_DIR="${FLOW_INSTALL_DIR:-$HOME/bin}"
mkdir -p "${INSTALL_DIR}"

SOURCE_BIN="${ROOT_DIR}/target/debug/f"
TARGET_BIN="${INSTALL_DIR}/f"

echo "Linking ${TARGET_BIN} -> ${SOURCE_BIN}"
rm -f "${TARGET_BIN}"
ln -s "${SOURCE_BIN}" "${TARGET_BIN}"

echo "Symlinked f to ${TARGET_BIN}"
echo "Ensure ${INSTALL_DIR} is on your PATH to run 'f' from anywhere."
