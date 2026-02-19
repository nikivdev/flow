#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this installer is macOS-only" >&2
  exit 1
fi

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  cat <<'EOF'
Install native domainsd launchd socket-activation on macOS (port 80, no docker).

Usage:
  sudo ./tools/domainsd-cpp/install-macos-launchd.sh
EOF
  exit 0
fi

if [[ "${EUID}" -ne 0 ]]; then
  exec sudo "$0" "$@"
fi

LABEL="dev.flow.domainsd"
SOCKET_NAME="domainsd"
PLIST_PATH="/Library/LaunchDaemons/${LABEL}.plist"

TARGET_USER="${SUDO_USER:-}"
if [[ -z "${TARGET_USER}" ]]; then
  TARGET_USER="$(stat -f '%Su' /dev/console)"
fi
if [[ -z "${TARGET_USER}" ]]; then
  echo "error: failed to determine target user" >&2
  exit 1
fi
TARGET_GROUP="$(id -gn "${TARGET_USER}")"
TARGET_HOME="$(dscl . -read "/Users/${TARGET_USER}" NFSHomeDirectory 2>/dev/null | awk '{print $2}')"
if [[ -z "${TARGET_HOME}" ]]; then
  TARGET_HOME="$(eval echo "~${TARGET_USER}")"
fi
if [[ ! -d "${TARGET_HOME}" ]]; then
  echo "error: target home does not exist: ${TARGET_HOME}" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FLOW_REPO="$(cd "${SCRIPT_DIR}/../.." && pwd)"
SOURCE_PATH="${FLOW_REPO}/tools/domainsd-cpp/domainsd.cpp"
if [[ ! -f "${SOURCE_PATH}" ]]; then
  echo "error: source missing: ${SOURCE_PATH}" >&2
  exit 1
fi

STATE_ROOT="${TARGET_HOME}/Library/Application Support/flow/local-domains"
BIN_PATH="${STATE_ROOT}/domainsd-cpp"
ROUTES_PATH="${STATE_ROOT}/routes.json"
PID_PATH="${STATE_ROOT}/domainsd.pid"
LOG_PATH="${STATE_ROOT}/domainsd.log"

mkdir -p "${STATE_ROOT}"
if [[ ! -f "${ROUTES_PATH}" ]]; then
  printf '{}\n' > "${ROUTES_PATH}"
fi
touch "${LOG_PATH}"
rm -f "${PID_PATH}"

echo "[domainsd-launchd] building native daemon..."
/usr/bin/clang++ -std=c++20 -O3 -DNDEBUG -Wall -Wextra -pthread \
  "${SOURCE_PATH}" \
  -o "${BIN_PATH}"

chown "${TARGET_USER}:${TARGET_GROUP}" "${BIN_PATH}" "${ROUTES_PATH}" "${LOG_PATH}" "${STATE_ROOT}"
chmod 755 "${BIN_PATH}"
chmod 644 "${ROUTES_PATH}" "${LOG_PATH}"

cat > "${PLIST_PATH}" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>${BIN_PATH}</string>
    <string>--launchd-socket</string>
    <string>${SOCKET_NAME}</string>
    <string>--routes</string>
    <string>${ROUTES_PATH}</string>
    <string>--pidfile</string>
    <string>${PID_PATH}</string>
  </array>
  <key>UserName</key>
  <string>${TARGET_USER}</string>
  <key>WorkingDirectory</key>
  <string>${STATE_ROOT}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${LOG_PATH}</string>
  <key>StandardErrorPath</key>
  <string>${LOG_PATH}</string>
  <key>Sockets</key>
  <dict>
    <key>${SOCKET_NAME}</key>
    <dict>
      <key>SockNodeName</key>
      <string>127.0.0.1</string>
      <key>SockServiceName</key>
      <string>80</string>
      <key>SockType</key>
      <string>stream</string>
      <key>SockProtocol</key>
      <string>TCP</string>
    </dict>
  </dict>
</dict>
</plist>
EOF

chown root:wheel "${PLIST_PATH}"
chmod 644 "${PLIST_PATH}"

echo "[domainsd-launchd] loading launchd service..."
launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
launchctl bootstrap system "${PLIST_PATH}"
launchctl enable "system/${LABEL}" >/dev/null 2>&1 || true
launchctl kickstart -k "system/${LABEL}"

sleep 0.3
if curl -fsS "http://127.0.0.1/_flow/domains/health" >/dev/null 2>&1; then
  echo "[domainsd-launchd] health check OK"
else
  echo "[domainsd-launchd] warning: health check failed, inspect log: ${LOG_PATH}" >&2
fi

cat <<EOF
[domainsd-launchd] installed.
  label: ${LABEL}
  plist: ${PLIST_PATH}
  binary: ${BIN_PATH}
  routes: ${ROUTES_PATH}
  log: ${LOG_PATH}

Next:
  cd ~/code/myflow
  f domains --engine native up
  f up
EOF
