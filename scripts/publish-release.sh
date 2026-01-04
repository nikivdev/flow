#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "Usage: $0 <ssh-host> <tarball>" >&2
    echo "Env: REMOTE_ROOT=/var/www/flow" >&2
    exit 1
fi

SSH_HOST="$1"
TARBALL="$2"

if [[ ! -f "${TARBALL}" ]]; then
    echo "publish-release: tarball not found: ${TARBALL}" >&2
    exit 1
fi

if ! command -v ssh >/dev/null 2>&1; then
    echo "publish-release: ssh is required." >&2
    exit 1
fi

if ! command -v scp >/dev/null 2>&1; then
    echo "publish-release: scp is required." >&2
    exit 1
fi

FILENAME="$(basename "${TARBALL}")"
if [[ "${FILENAME}" =~ ^flow_(.+)_darwin_arm64\.tar\.gz$ ]]; then
    VERSION="${BASH_REMATCH[1]}"
else
    echo "publish-release: expected flow_<version>_darwin_arm64.tar.gz" >&2
    exit 1
fi

SHA_FILE="${TARBALL}.sha256"
if [[ ! -f "${SHA_FILE}" ]]; then
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "${TARBALL}" > "${SHA_FILE}"
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${TARBALL}" > "${SHA_FILE}"
    else
        echo "publish-release: need shasum or sha256sum to create checksum" >&2
        exit 1
    fi
fi

REMOTE_ROOT="${REMOTE_ROOT:-/var/www/flow}"
REMOTE_VERSION_DIR="${REMOTE_ROOT}/${VERSION}"
REMOTE_LATEST_DIR="${REMOTE_ROOT}/latest"
LATEST_NAME="flow_latest_darwin_arm64.tar.gz"
LATEST_SHA="${LATEST_NAME}.sha256"

ssh "${SSH_HOST}" "mkdir -p '${REMOTE_VERSION_DIR}' '${REMOTE_LATEST_DIR}'"
scp "${TARBALL}" "${SSH_HOST}:${REMOTE_VERSION_DIR}/${FILENAME}"
scp "${SHA_FILE}" "${SSH_HOST}:${REMOTE_VERSION_DIR}/${FILENAME}.sha256"

ssh "${SSH_HOST}" "ln -sf '${REMOTE_VERSION_DIR}/${FILENAME}' '${REMOTE_LATEST_DIR}/${LATEST_NAME}'"
ssh "${SSH_HOST}" "ln -sf '${REMOTE_VERSION_DIR}/${FILENAME}.sha256' '${REMOTE_LATEST_DIR}/${LATEST_SHA}'"

echo "publish-release: uploaded ${FILENAME} to ${SSH_HOST}:${REMOTE_VERSION_DIR}"
echo "publish-release: latest -> ${REMOTE_LATEST_DIR}/${LATEST_NAME}"
