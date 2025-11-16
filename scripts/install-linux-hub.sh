#!/usr/bin/env bash
set -euo pipefail

# Installs the flow hub daemon on a Linux machine that can reach the supplied
# binary/config URLs. Intended to be run via:
#   curl -fsSL https://raw.githubusercontent.com/nikiv/flow/main/scripts/install-linux-hub.sh | \
#     sudo FLOW_BINARY_URL=https://example.com/f-linux FLOW_CONFIG_URL=https://example.com/config.toml bash

if [[ "${EUID}" -ne 0 ]]; then
    echo "This installer must run as root (use sudo)." >&2
    exit 1
fi

FLOW_BINARY_URL="${FLOW_BINARY_URL:-}"
FLOW_CONFIG_URL="${FLOW_CONFIG_URL:-}"
FLOW_ROOT="${FLOW_ROOT:-/opt/flow}"
FLOW_USER="${FLOW_USER:-flow}"
FLOW_PORT="${FLOW_PORT:-9050}"
FLOW_SERVICE_NAME="${FLOW_SERVICE_NAME:-flowd}"

if [[ -z "${FLOW_BINARY_URL}" ]]; then
    echo "FLOW_BINARY_URL must be set to a downloadable flow binary." >&2
    exit 1
fi

if [[ -z "${FLOW_CONFIG_URL}" ]]; then
    echo "FLOW_CONFIG_URL must be set to a downloadable flow.toml." >&2
    exit 1
fi

command -v curl >/dev/null 2>&1 || {
    echo "curl is required to download assets. Install it and retry." >&2
    exit 1
}

if [[ ! -d /run/systemd/system ]]; then
    echo "systemd is required to install the hub as a service." >&2
    exit 1
fi

if ! id -u "${FLOW_USER}" >/dev/null 2>&1; then
    echo "Creating system user ${FLOW_USER}"
    useradd --system --create-home --shell /usr/sbin/nologin "${FLOW_USER}"
fi

BIN_DIR="${FLOW_ROOT}/bin"
CONFIG_DIR="${FLOW_ROOT}/config"
mkdir -p "${BIN_DIR}" "${CONFIG_DIR}"
chown -R "${FLOW_USER}:${FLOW_USER}" "${FLOW_ROOT}"

BIN_PATH="${BIN_DIR}/f"
CONFIG_PATH="${CONFIG_DIR}/flow.toml"

echo "Downloading flow binary from ${FLOW_BINARY_URL}"
curl -fsSL "${FLOW_BINARY_URL}" -o "${BIN_PATH}"
chmod +x "${BIN_PATH}"
chown "${FLOW_USER}:${FLOW_USER}" "${BIN_PATH}"

echo "Downloading flow config from ${FLOW_CONFIG_URL}"
curl -fsSL "${FLOW_CONFIG_URL}" -o "${CONFIG_PATH}"
chown "${FLOW_USER}:${FLOW_USER}" "${CONFIG_PATH}"

UNIT_FILE="/etc/systemd/system/${FLOW_SERVICE_NAME}.service"
cat <<EOF >"${UNIT_FILE}"
[Unit]
Description=Flow hub daemon
After=network.target

[Service]
Type=simple
Environment=FLOW_CONFIG=${CONFIG_PATH}
ExecStart=${BIN_PATH} daemon --host 0.0.0.0 --port ${FLOW_PORT}
Restart=always
RestartSec=5
User=${FLOW_USER}
WorkingDirectory=${FLOW_ROOT}

[Install]
WantedBy=multi-user.target
EOF

echo "Enabling ${FLOW_SERVICE_NAME} systemd unit"
systemctl daemon-reload
systemctl enable --now "${FLOW_SERVICE_NAME}"

echo "Flow hub installed."
echo "Check status with: sudo systemctl status ${FLOW_SERVICE_NAME}"
echo "Verify health: curl http://<tailscale-ip>:${FLOW_PORT}/health"
