#!/usr/bin/env bash
set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <ssh-host> [config-path]" >&2
    exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
ROOT_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

SSH_HOST="$1"
CONFIG_PATH="${2:-$HOME/.config/flow/config.toml}"

if [ ! -f "${CONFIG_PATH}" ]; then
    echo "Config file not found at ${CONFIG_PATH}" >&2
    exit 1
fi

REMOTE_HOME="$(ssh "${SSH_HOST}" 'printf %s "$HOME"')"
REMOTE_ROOT="${REMOTE_ROOT:-${REMOTE_HOME}/flow-hub}"
REMOTE_PORT="${REMOTE_PORT:-9050}"
REMOTE_BIN_DIR="${REMOTE_ROOT}/bin"
REMOTE_CONFIG_DIR="${REMOTE_ROOT}/config"
REMOTE_SYNC_DIR="${REMOTE_ROOT}/sync"
REMOTE_SERVICE_USER="${REMOTE_SERVICE_USER:-$(ssh "${SSH_HOST}" 'whoami')}"

echo "Building flow CLI and daemon (release profile)..."
FLOW_PROFILE=release "${ROOT_DIR}/scripts/deploy.sh" >/dev/null
FLOW_BIN="${ROOT_DIR}/target/release/f"

echo "Copying binary and config to ${SSH_HOST}:${REMOTE_ROOT}"
ssh "${SSH_HOST}" "mkdir -p ${REMOTE_BIN_DIR} ${REMOTE_CONFIG_DIR}"
scp "${FLOW_BIN}" "${SSH_HOST}:${REMOTE_BIN_DIR}/f"
scp "${CONFIG_PATH}" "${SSH_HOST}:${REMOTE_CONFIG_DIR}/flow.toml"

if [ -n "${REMOTE_SYNC_PATHS:-}" ]; then
    ssh "${SSH_HOST}" "mkdir -p ${REMOTE_SYNC_DIR}"
    IFS=':' read -ra SYNC_PATHS <<<"${REMOTE_SYNC_PATHS}"
    for path in "${SYNC_PATHS[@]}"; do
        [ -z "${path}" ] && continue
        if [ ! -e "${path}" ]; then
            echo "Skipping sync path (missing): ${path}" >&2
            continue
        fi
        echo "Syncing ${path} -> ${SSH_HOST}:${REMOTE_SYNC_DIR}"
        scp -r "${path}" "${SSH_HOST}:${REMOTE_SYNC_DIR}"
    done
fi

SERVICE_UNIT="[Unit]
Description=Remote flow hub
After=network.target

[Service]
Type=simple
Environment=FLOW_CONFIG=${REMOTE_CONFIG_DIR}/flow.toml
ExecStart=${REMOTE_BIN_DIR}/f daemon --host 0.0.0.0 --port ${REMOTE_PORT}
Restart=always
RestartSec=5
User=${REMOTE_SERVICE_USER}
WorkingDirectory=${REMOTE_ROOT}

[Install]
WantedBy=multi-user.target"

echo "Configuring systemd service on ${SSH_HOST}"
ssh "${SSH_HOST}" "sudo bash -c 'cat <<\"EOF\" > /etc/systemd/system/flowd.service
${SERVICE_UNIT}
EOF
systemctl daemon-reload
systemctl enable --now flowd.service'"

echo "Remote hub deployed. Use tailscale to reach ${SSH_HOST}:${REMOTE_PORT}."
