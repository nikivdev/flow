#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PORT=${FLOW_DESKTOP_PORT:-}

if [[ ! -d "${ROOT_DIR}/node_modules" || ! -d "${ROOT_DIR}/node_modules/@tauri-apps/plugin-http" || ! -d "${ROOT_DIR}/node_modules/lucide-react" ]]; then
  (cd "$ROOT_DIR" && bun install)
fi

if [[ -z "$PORT" ]]; then
  PORT=$(python - <<'PY'
import socket
s = socket.socket()
s.bind(("", 0))
print(s.getsockname()[1])
s.close()
PY
)
fi

DEV_URL="http://localhost:${PORT}"
CONFIG_PATH="${ROOT_DIR}/.tauri-dev.json"

cat > "$CONFIG_PATH" <<CONFIG
{
  "build": {
    "devUrl": "${DEV_URL}",
    "beforeDevCommand": "bun run dev -- --port ${PORT} --strictPort"
  }
}
CONFIG

cleanup() {
  rm -f "$CONFIG_PATH"
}
trap cleanup EXIT

bun run tauri dev --config "$CONFIG_PATH"
