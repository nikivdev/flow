#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"

PROFILE="${FLOW_PROFILE:-debug}"
TARGET_DIR="debug"
BUILD_ARGS=()
[[ "${PROFILE}" == "release" ]] && TARGET_DIR="release" && BUILD_ARGS+=("--release")

append_rustflag() {
    local flag="$1"
    if [[ -n "${RUSTFLAGS:-}" ]]; then
        RUSTFLAGS+=" ${flag}"
    else
        RUSTFLAGS="${flag}"
    fi
}

if [[ "${PROFILE}" == "release" ]]; then
    export CARGO_INCREMENTAL=0

    if [[ "$(uname -s)" == "Darwin" ]]; then
        append_rustflag "-C target-cpu=${FLOW_DEPLOY_TARGET_CPU:-native}"
        append_rustflag "-C link-arg=-Wl,-dead_strip"
        append_rustflag "-C link-arg=-Wl,-dead_strip_dylibs"
    fi

    if [[ -n "${FLOW_DEPLOY_RUSTFLAGS:-}" ]]; then
        append_rustflag "${FLOW_DEPLOY_RUSTFLAGS}"
    fi

    export RUSTFLAGS
fi

# Build
if [[ ${#BUILD_ARGS[@]} -gt 0 ]]; then
    cargo build "${BUILD_ARGS[@]}" --quiet
else
    cargo build --quiet
fi

SOURCE_F="${ROOT_DIR}/target/${TARGET_DIR}/f"
SOURCE_LIN="${ROOT_DIR}/target/${TARGET_DIR}/lin"
RUNTIME_ASSETS=(
    "scripts/private_mirror.py"
    "scripts/codex-skill-eval-launchd.py"
    "tools/domainsd-cpp/domainsd.cpp"
    "tools/domainsd-cpp/install-macos-launchd.sh"
    "tools/domainsd-cpp/uninstall-macos-launchd.sh"
)

PRIMARY_DIR="${HOME}/bin"
ALT_DIR="${HOME}/.local/bin"
PRIMARY_F="$(command -v f 2>/dev/null || true)"
PRIMARY_INSTALLED=false

ad_hoc_sign_if_available() {
    local bin_path="$1"
    [[ -f "$bin_path" ]] || return 0
    if command -v codesign >/dev/null 2>&1; then
        # Avoid macOS "load code signature error" on copied local binaries.
        codesign --force --sign - --timestamp=none "$bin_path" >/dev/null 2>&1 || true
    fi
}

if [[ -n "${PRIMARY_F}" ]]; then
    PRIMARY_DIR="$(dirname -- "${PRIMARY_F}")"
fi

install_to_dir() {
    local dir="$1"
    [[ -d "${dir}" ]] || return 0
    [[ -w "${dir}" ]] || return 0

    # Copy binaries (more reliable than symlinks)
    if [[ -e "${dir}/f" && "${SOURCE_F}" -ef "${dir}/f" ]]; then
        :
    else
        cp -f "${SOURCE_F}" "${dir}/f" 2>/dev/null || return 1
    fi
    ad_hoc_sign_if_available "${dir}/f"
    if [[ -e "${dir}/flow" && "${SOURCE_F}" -ef "${dir}/flow" ]]; then
        :
    else
        cp -f "${SOURCE_F}" "${dir}/flow" 2>/dev/null || true
    fi
    ad_hoc_sign_if_available "${dir}/flow"
    if [[ -e "${dir}/lin" && "${SOURCE_LIN}" -ef "${dir}/lin" ]]; then
        :
    else
        cp -f "${SOURCE_LIN}" "${dir}/lin" 2>/dev/null || true
    fi
    ad_hoc_sign_if_available "${dir}/lin"
    return 0
}

install_runtime_assets() {
    local install_root="$1"
    local assets_root="${install_root}/share/flow"
    local rel=""
    mkdir -p "${assets_root}"
    for rel in "${RUNTIME_ASSETS[@]}"; do
        local src="${ROOT_DIR}/${rel}"
        local dest="${assets_root}/${rel}"
        [[ -f "${src}" ]] || {
            echo "missing runtime asset: ${src}" >&2
            return 1
        }
        mkdir -p "$(dirname "${dest}")"
        cp -f "${src}" "${dest}"
        if [[ -x "${src}" ]]; then
            chmod +x "${dest}" 2>/dev/null || true
        fi
    done
}

mkdir -p "${PRIMARY_DIR}"
if install_to_dir "${PRIMARY_DIR}"; then
    PRIMARY_INSTALLED=true
fi

if [[ "${PRIMARY_INSTALLED}" == true ]]; then
    install_runtime_assets "$(dirname -- "${PRIMARY_DIR}")"
fi

# If ~/.local/bin exists, link to the primary install for consistency.
if [[ -d "${ALT_DIR}" ]]; then
    ln -sf "${PRIMARY_DIR}/f" "${ALT_DIR}/f"
    ln -sf "${PRIMARY_DIR}/f" "${ALT_DIR}/flow"
    ln -sf "${PRIMARY_DIR}/lin" "${ALT_DIR}/lin"
fi

# Verify
if command -v f &>/dev/null; then
    echo "flow ${PROFILE} build installed"
else
    echo "Installed to ~/bin - add to PATH: export PATH=\"\$HOME/bin:\$PATH\""
fi
