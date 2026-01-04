#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
    echo "Usage: $0 <ssh-host> <domain>" >&2
    echo "Env: RELEASE_ROOT=/var/www/flow CADDYFILE_PATH=/etc/caddy/Caddyfile" >&2
    exit 1
fi

SSH_HOST="$1"
DOMAIN="$2"
RELEASE_ROOT="${RELEASE_ROOT:-/var/www/flow}"
CADDYFILE_PATH="${CADDYFILE_PATH:-/etc/caddy/Caddyfile}"

if ! command -v ssh >/dev/null 2>&1; then
    echo "ssh is required to configure the release host." >&2
    exit 1
fi

ssh "${SSH_HOST}" "sudo bash -s -- '${DOMAIN}' '${RELEASE_ROOT}' '${CADDYFILE_PATH}'" <<'EOF'
set -euo pipefail

DOMAIN="${1:?missing domain}"
RELEASE_ROOT="${2:-/var/www/flow}"
CADDYFILE_PATH="${3:-/etc/caddy/Caddyfile}"

fail() {
    echo "release-host: $*" >&2
    exit 1
}

install_caddy() {
    if command -v caddy >/dev/null 2>&1; then
        return
    fi

    if ! command -v apt-get >/dev/null 2>&1; then
        fail "caddy not installed and apt-get not available"
    fi

    apt-get update
    apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gnupg
    curl -1sLf "https://dl.cloudsmith.io/public/caddy/stable/gpg.key" | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
    curl -1sLf "https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt" | tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
    apt-get update
    apt-get install -y caddy
}

if [[ ! -d /run/systemd/system ]]; then
    fail "systemd is required to manage Caddy"
fi

install_caddy

mkdir -p "${RELEASE_ROOT}"
chmod 755 "${RELEASE_ROOT}"

mkdir -p "$(dirname "${CADDYFILE_PATH}")"
if [[ ! -f "${CADDYFILE_PATH}" ]]; then
    touch "${CADDYFILE_PATH}"
fi

if ! grep -Fq "${DOMAIN}" "${CADDYFILE_PATH}"; then
    cat <<CFG >> "${CADDYFILE_PATH}"

${DOMAIN} {
  root * ${RELEASE_ROOT}
  file_server
}
CFG
fi

systemctl enable --now caddy
systemctl reload caddy

echo "release-host: serving ${RELEASE_ROOT} on https://${DOMAIN}"
EOF
