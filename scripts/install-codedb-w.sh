#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

CODEDB_DIR="${CODEDB_DIR:-$HOME/bin}"
FLOW_CODEDB_ALIAS_DIR="${FLOW_CODEDB_ALIAS_DIR:-$HOME/bin}"
CODEDB_INSTALL_SCRIPT="${CODEDB_INSTALL_SCRIPT:-$HOME/repos/justrach/codedb/install/install.sh}"
CODEDB_INSTALL_URL="${CODEDB_INSTALL_URL:-https://codedb.codegraff.com/install.sh}"

resolve_codedb_bin() {
    if [[ -n "${CODEDB_BIN:-}" && -x "${CODEDB_BIN}" ]]; then
        printf '%s\n' "${CODEDB_BIN}"
        return 0
    fi

    local candidate=""
    for candidate in \
        "${CODEDB_DIR}/codedb" \
        "$HOME/bin/codedb" \
        "$HOME/.local/bin/codedb" \
        "$(command -v codedb 2>/dev/null || true)"
    do
        if [[ -n "${candidate}" && -x "${candidate}" ]]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
    done

    return 1
}

install_codedb() {
    if [[ -x "${CODEDB_INSTALL_SCRIPT}" ]]; then
        CODEDB_DIR="${CODEDB_DIR}" bash "${CODEDB_INSTALL_SCRIPT}"
        return 0
    fi

    curl -fsSL "${CODEDB_INSTALL_URL}" | CODEDB_DIR="${CODEDB_DIR}" sh
}

write_wrapper() {
    local wrapper_dir="${FLOW_CODEDB_ALIAS_DIR}"
    local wrapper="${wrapper_dir}/w"
    local tmp

    mkdir -p "${wrapper_dir}"
    tmp="$(mktemp "${TMPDIR:-/tmp}/flow-codedb-w.XXXXXX")"

    {
        echo '#!/usr/bin/env bash'
        echo 'set -euo pipefail'
        printf 'CODEDB_RESOLVED=%q\n' "${CODEDB_BIN_RESOLVED}"
        cat <<'EOF'

if [[ -n "${CODEDB_BIN:-}" && -x "${CODEDB_BIN}" ]]; then
    exec "${CODEDB_BIN}" "$@"
fi

if [[ -n "${CODEDB_RESOLVED:-}" && -x "${CODEDB_RESOLVED}" ]]; then
    exec "${CODEDB_RESOLVED}" "$@"
fi

CODEDB_DIR="${CODEDB_DIR:-$HOME/bin}"
for candidate in \
    "${CODEDB_DIR}/codedb" \
    "$HOME/bin/codedb" \
    "$HOME/.local/bin/codedb" \
    "$(command -v codedb 2>/dev/null || true)"
do
    if [[ -n "${candidate}" && -x "${candidate}" ]]; then
        exec "${candidate}" "$@"
    fi
done

echo "w: codedb not found. Re-run: f install-codedb-w" >&2
exit 127
EOF
    } > "${tmp}"

    install -m 755 "${tmp}" "${wrapper}"
    rm -f "${tmp}"

    printf 'codedb  -> %s\n' "${CODEDB_BIN_RESOLVED}"
    printf 'wrapper -> %s\n' "${wrapper}"
    printf 'note    -> shadows /usr/bin/w when %s is earlier on PATH\n' "${wrapper_dir}"
    printf 'which w -> %s\n' "$(command -v w)"
}

main() {
    if ! CODEDB_BIN_RESOLVED="$(resolve_codedb_bin)"; then
        install_codedb
        CODEDB_BIN_RESOLVED="$(resolve_codedb_bin)"
    fi

    write_wrapper
}

main "$@"
