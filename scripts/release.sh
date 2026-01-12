#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
SETUP=1

if [[ "${1:-}" == "--no-setup" ]]; then
    SETUP=0
    shift
elif [[ "${1:-}" == "--setup" ]]; then
    SETUP=1
    shift
fi

if ! command -v infra >/dev/null 2>&1; then
    echo "release: infra CLI not found. Build it with:" >&2
    echo "  (cd /path/to/infra/cli && cargo build --release && cp target/release/infra ~/.local/bin/infra)" >&2
    exit 1
fi

bash "${ROOT_DIR}/scripts/package-release.sh"

tarball="$(ls -t "${ROOT_DIR}"/dist/flow_*_darwin_arm64.tar.gz 2>/dev/null | head -n1 || true)"
if [[ -z "${tarball}" ]]; then
    echo "release: no darwin/arm64 tarball found in dist/" >&2
    exit 1
fi

cmd=(infra release publish "${tarball}" --path "${ROOT_DIR}")
if [[ "${SETUP}" -eq 1 ]]; then
    cmd+=(--setup)
fi

"${cmd[@]}"
