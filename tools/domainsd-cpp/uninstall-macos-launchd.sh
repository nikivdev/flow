#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: this uninstaller is macOS-only" >&2
  exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
  exec sudo "$0" "$@"
fi

LABEL="dev.flow.domainsd"
PLIST_PATH="/Library/LaunchDaemons/${LABEL}.plist"

echo "[domainsd-launchd] unloading service..."
launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
launchctl disable "system/${LABEL}" >/dev/null 2>&1 || true

if [[ -f "${PLIST_PATH}" ]]; then
  rm -f "${PLIST_PATH}"
  echo "[domainsd-launchd] removed ${PLIST_PATH}"
fi

echo "[domainsd-launchd] uninstalled."
echo "Note: binary/routes/log files under ~/Library/Application Support/flow/local-domains were kept."
